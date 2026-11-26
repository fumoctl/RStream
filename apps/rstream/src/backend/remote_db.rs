//! SQLite downloads & binge-lock store with conflict-free append-only rclone remote sync (`RStream/.db/`).
//!
//! Architecture:
//! - Cinemeta (`tt...`) is the canonical ID backbone; `addon_url` is never part of download identity
//!   and is stripped from remote sync payloads so private addon tokens never leak.
//! - Each device writes only new, immutable JSONL event batches under `RStream/.db/events/<device_id>/`,
//!   eliminating concurrent write collisions across users/devices (including over `rclone crypt`).
//! - 2-tier compaction rolls up >40 event files into `RStream/.db/deltas/delta-*.jsonl` and >15 deltas
//!   into `RStream/.db/snapshots/base-*.jsonl` so cloud folder listings stay tiny even on huge libraries.
use super::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const EVENT_COMPACT_THRESHOLD: usize = 40;
const DELTA_COMPACT_THRESHOLD: usize = 15;
const MAX_HLC_DRIFT_MS: u64 = 86_400_000;
const MAX_HLC_CNT: u64 = 999_999;

static DB_INIT_LOCK: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
static DB_SYNC_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static PROFILE_SYNC_EPOCH: AtomicU64 = AtomicU64::new(0);
static PROFILE_DEBOUNCE_QUEUED: AtomicBool = AtomicBool::new(false);

struct SyncBusyGuard;

impl SyncBusyGuard {
    fn enter() -> Self {
        DB_SYNC_ACTIVE.fetch_add(1, Ordering::SeqCst);
        downloads::notify_background_activity(true);
        Self
    }
}

impl Drop for SyncBusyGuard {
    fn drop(&mut self) {
        DB_SYNC_ACTIVE.fetch_sub(1, Ordering::SeqCst);
        downloads::notify_background_activity(true);
    }
}

pub fn is_sync_busy() -> bool {
    DB_SYNC_ACTIVE.load(Ordering::SeqCst) > 0
}

pub fn profile_sync_epoch() -> u64 {
    PROFILE_SYNC_EPOCH.load(Ordering::SeqCst)
}

fn db_root_dir() -> PathBuf {
    downloads::data_root_for_db()
}

fn db_path() -> PathBuf {
    db_root_dir().join("downloads.sqlite")
}

/// Extract a validated IMDb ID (`"tt"` + ASCII digits) from `raw_id` (`"tt0903747"` or `"tt0903747:1:1"`)
/// or `raw_meta` (`"imdb_id"` / `"imdbId"`).
pub fn extract_imdb_id(raw_id: &str, raw_meta: &Value) -> Option<String> {
    let valid = |cand: &str| -> Option<String> {
        let head = cand.trim().split(':').next().unwrap_or("");
        (head.starts_with("tt") && head.len() > 2 && head[2..].bytes().all(|b| b.is_ascii_digit()))
            .then(|| head.to_owned())
    };
    valid(raw_id)
        .or_else(|| raw_meta.get("imdb_id").and_then(Value::as_str).and_then(valid))
        .or_else(|| raw_meta.get("imdbId").and_then(Value::as_str).and_then(valid))
}

/// Normalize catalog/search `Media` onto Cinemeta (`CINEMETA`) whenever it carries a valid IMDb ID,
/// keeping non-IMDb items (e.g. `kitsu:...` anime OVAs) intact as a fallback.
pub fn canonicalize_catalog_media(mut m: Media) -> Media {
    if let Some(imdb) = extract_imdb_id(&m.id, &m.raw) {
        m.id = imdb.clone();
        m.addon_url = CINEMETA.to_owned();
        if let Some(obj) = m.raw.as_object_mut() {
            obj.insert("id".into(), json!(imdb));
            obj.remove("addon_url");
            obj.remove("_addon_url");
        }
    }
    m
}

pub fn canonical_media_id(media: &Media) -> String {
    extract_imdb_id(&media.id, &media.raw).unwrap_or_else(|| media.id.trim().to_owned())
}

pub fn canonical_video_id(raw_media_id: &str, canon_media_id: &str, video_id: &str) -> String {
    let vid = video_id.trim();
    if vid.is_empty() || vid == raw_media_id {
        return canon_media_id.to_owned();
    }
    if canon_media_id != raw_media_id {
        if let Some(suffix) = vid.strip_prefix(&format!("{raw_media_id}:")) {
            return format!("{canon_media_id}:{suffix}");
        }
    }
    if let Some(imdb) = extract_imdb_id(vid, &Value::Null) {
        if let Some((_, suffix)) = vid.split_once(':') {
            return format!("{imdb}:{suffix}");
        }
        return imdb;
    }
    vid.to_owned()
}

pub fn canonical_series_key(media: &Media) -> String {
    format!("{}|{}", media.kind.trim(), canonical_media_id(media))
}

pub fn canonical_download_key_parts(kind: &str, raw_media_id: &str, raw_meta: &Value, video_id: &str) -> (String, String, String) {
    let k = kind.trim();
    let canon_mid = extract_imdb_id(raw_media_id, raw_meta).unwrap_or_else(|| raw_media_id.trim().to_owned());
    let canon_vid = canonical_video_id(raw_media_id.trim(), &canon_mid, video_id);
    let key = format!("{k}|{canon_mid}|{canon_vid}");
    (key, canon_mid, canon_vid)
}

pub fn entry_row_id(canonical_key: &str, remote: Option<&str>) -> String {
    match remote.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => format!("{canonical_key}@{r}"),
        None => format!("{canonical_key}@local"),
    }
}

/// Sanitize `Media` into self-contained JSON with zero custom `addon_url` tokens.
pub fn sanitized_media_json(m: &Media, canon_mid: &str) -> Value {
    json!({
        "id": canon_mid,
        "type": m.kind,
        "name": m.name,
        "addon_url": safe_sync_addon_url(canon_mid),
        "poster": m.poster,
        "background": m.background,
        "description": m.description,
        "releaseInfo": m.year,
        "genres": m.genres,
    })
}

// ---------------------------------------------------------------- Human-readable remote paths

fn is_dos_device_stem(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || matches!(
            upper.as_bytes(),
            [b'C', b'O', b'M', b'1'..=b'9'] | [b'L', b'P', b'T', b'1'..=b'9']
        )
}

fn truncate_utf8_trimmed(s: &str, max_bytes: usize) -> &str {
    let mut end = s.len().min(max_bytes);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_matches(|c: char| c.is_whitespace() || c == '.')
}

fn sanitize_path_piece(text: &str) -> String {
    let clean: String = text
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')' | '\'' | '&') {
                c
            } else {
                ' '
            }
        })
        .collect();
    let collapsed = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = truncate_utf8_trimmed(
        collapsed.trim_matches(|c: char| c.is_whitespace() || c == '.'),
        100,
    );
    if trimmed.is_empty() {
        return "Untitled".into();
    }
    let stem = trimmed.split('.').next().unwrap_or(trimmed).trim();
    if is_dos_device_stem(stem) {
        let capped = truncate_utf8_trimmed(trimmed, 99);
        if capped.is_empty() { "_Untitled".into() } else { format!("_{capped}") }
    } else {
        trimmed.to_owned()
    }
}

pub fn alpha_bucket(title: &str) -> &'static str {
    let trimmed = title.trim();
    let stripped = ["the ", "a ", "an "]
        .iter()
        .find_map(|p| {
            trimmed
                .get(..p.len())
                .filter(|head| head.eq_ignore_ascii_case(p))
                .and_then(|_| trimmed.get(p.len()..))
                .map(str::trim)
                .filter(|rest| !rest.is_empty())
        })
        .unwrap_or(trimmed);
    match stripped.chars().next().map(|c| c.to_ascii_uppercase()) {
        Some('A') => "A", Some('B') => "B", Some('C') => "C", Some('D') => "D",
        Some('E') => "E", Some('F') => "F", Some('G') => "G", Some('H') => "H",
        Some('I') => "I", Some('J') => "J", Some('K') => "K", Some('L') => "L",
        Some('M') => "M", Some('N') => "N", Some('O') => "O", Some('P') => "P",
        Some('Q') => "Q", Some('R') => "R", Some('S') => "S", Some('T') => "T",
        Some('U') => "U", Some('V') => "V", Some('W') => "W", Some('X') => "X",
        Some('Y') => "Y", Some('Z') => "Z",
        _ => "#",
    }
}

fn parse_season_episode(video_id: &str, title: &str, hint: &str) -> (Option<u32>, Option<u32>) {
    let parts: Vec<&str> = video_id.split(':').collect();
    if parts.len() >= 3 {
        let ep = parts.last().and_then(|x| x.parse::<u32>().ok());
        // `kitsu:1264:5` -> 3 parts where middle is the Kitsu anime ID, not season!
        if parts.len() == 3
            && (parts[0].eq_ignore_ascii_case("kitsu")
                || parts[0].eq_ignore_ascii_case("mal")
                || parts[0].eq_ignore_ascii_case("anilist"))
        {
            return (None, ep);
        }
        let se = parts.get(parts.len() - 2).and_then(|x| x.parse::<u32>().ok());
        if se.is_some() && ep.is_some() {
            return (se, ep);
        }
    }
    static RE_SE: OnceLock<regex::Regex> = OnceLock::new();
    static RE_EP: OnceLock<regex::Regex> = OnceLock::new();
    let re_se = RE_SE.get_or_init(|| regex::Regex::new(r"(?i)\bS(\d{1,2})\s*E(\d{1,3})\b").unwrap());
    let re_ep = RE_EP.get_or_init(|| regex::Regex::new(r"(?i)\bE(?:p(?:isode)?)?\s*(\d{1,3})\b").unwrap());
    for hay in [hint, title] {
        if let Some(c) = re_se.captures(hay) {
            return (c.get(1).and_then(|m| m.as_str().parse().ok()), c.get(2).and_then(|m| m.as_str().parse().ok()));
        }
    }
    for hay in [hint, title] {
        if let Some(c) = re_ep.captures(hay) {
            return (None, c.get(1).and_then(|m| m.as_str().parse().ok()));
        }
    }
    (None, None)
}

fn clean_ext(hint: &str) -> &'static str {
    let path_part = hint.split(['?', '#']).next().unwrap_or("");
    match path_part.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("mp4") => "mp4",
        Some("webm") => "webm",
        Some("m4v") => "m4v",
        Some("mov") => "mov",
        Some("avi") => "avi",
        Some("ts") => "ts",
        _ => "mkv",
    }
}

/// Human-readable Jellyfin/Plex-compatible remote path:
/// - Movies: `RStream/Movies/B/Big Buck Bunny (2008)/Big Buck Bunny (2008).mkv`
/// - Series: `RStream/Series/P/Pioneer One (2010)/Season 01/Pioneer One - S01E01.mkv`
pub fn human_remote_object_path(media: &Media, video_id: &str, title: &str, hint: &str) -> String {
    let bucket = alpha_bucket(&media.name);
    let show_clean = sanitize_path_piece(&media.name);
    let folder = match media.first_year() {
        Some(y) if !show_clean.contains(&format!("({y})")) => format!("{show_clean} ({y})"),
        _ => show_clean.clone(),
    };
    let ext = clean_ext(hint);
    let canon_mid = canonical_media_id(media);
    let canon_vid = canonical_video_id(&media.id, &canon_mid, video_id);
    if media.kind == "movie" || canon_vid == canon_mid {
        return format!("RStream/Movies/{bucket}/{folder}/{folder}.{ext}");
    }
    match parse_season_episode(&canon_vid, title, hint) {
        (Some(se), Some(ep)) if se > 0 && ep > 0 => {
            format!("RStream/Series/{bucket}/{folder}/Season {se:02}/{show_clean} - S{se:02}E{ep:02}.{ext}")
        }
        (_, Some(ep)) if ep > 0 => {
            format!("RStream/Series/{bucket}/{folder}/{show_clean} - Episode {ep:02}.{ext}")
        }
        _ => {
            let stem = sanitize_path_piece(title);
            format!("RStream/Series/{bucket}/{folder}/{stem}.{ext}")
        }
    }
}

// ---------------------------------------------------------------- SQLite schema & HLC clock

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .flatten()
        .collect();
    Ok(cols.iter().any(|c| c == column))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn open_db() -> Result<Connection> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existed = std::fs::metadata(&path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false);
    let mut init_guard = DB_INIT_LOCK
        .lock()
        .map_err(|_| anyhow!("downloads db lock poisoned"))?;
    let initialized = init_guard.get_or_insert_with(HashSet::new);
    if !existed {
        initialized.remove(&path);
    }
    let conn = Connection::open(&path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    if initialized.contains(&path) {
        drop(init_guard);
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        return Ok(conn);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let legacy_kv = table_exists(&conn, "profile_kv_hlc")?
        && (!table_has_column(&conn, "profile_kv_hlc", "remote")?
            || !table_has_column(&conn, "profile_kv_hlc", "profile")?);
    let legacy_ops = table_exists(&conn, "pending_profile_ops")?
        && (!table_has_column(&conn, "pending_profile_ops", "remote")?
            || !table_has_column(&conn, "pending_profile_ops", "profile")?);
    let cfg = store::load_settings();
    let target_scope = (legacy_kv || legacy_ops).then(|| {
        cfg.sync_remote
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(|r| (r.to_owned(), store::sanitize_profile_name(&cfg.sync_profile)))
    }).flatten();
    let old_kv: Vec<(String, String, String, String)> = if legacy_kv && target_scope.is_some() {
        conn.prepare("SELECT ns, item_key, updated_hlc, payload_json FROM profile_kv_hlc")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .flatten()
            .collect()
    } else {
        Vec::new()
    };
    let old_ops: Vec<(String, String)> = if legacy_ops && target_scope.is_some() {
        conn.prepare("SELECT op_key, event_json FROM pending_profile_ops ORDER BY seq ASC")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .flatten()
            .collect()
    } else {
        Vec::new()
    };
    if legacy_kv { conn.execute_batch("DROP TABLE IF EXISTS profile_kv_hlc;")?; }
    if legacy_ops { conn.execute_batch("DROP TABLE IF EXISTS pending_profile_ops;")?; }

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS downloads (
             id TEXT PRIMARY KEY, canonical_key TEXT NOT NULL, media_kind TEXT NOT NULL,
             canonical_media_id TEXT NOT NULL, video_id TEXT NOT NULL, title TEXT NOT NULL,
             subtitle TEXT NOT NULL, path TEXT NOT NULL, remote TEXT, bytes INTEGER NOT NULL,
             downloaded_at INTEGER NOT NULL, updated_hlc TEXT NOT NULL,
             deleted INTEGER NOT NULL DEFAULT 0, media_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_downloads_key ON downloads(canonical_key, deleted, downloaded_at DESC);
         CREATE INDEX IF NOT EXISTS idx_downloads_video ON downloads(media_kind, video_id, deleted, downloaded_at DESC);
         CREATE INDEX IF NOT EXISTS idx_downloads_list ON downloads(deleted, downloaded_at DESC);
         CREATE INDEX IF NOT EXISTS idx_downloads_remote ON downloads(remote, deleted);
         CREATE TABLE IF NOT EXISTS pending_remote_ops (seq INTEGER PRIMARY KEY AUTOINCREMENT, remote TEXT NOT NULL, event_json TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS applied_remote_files (remote TEXT NOT NULL, rel_path TEXT NOT NULL, PRIMARY KEY (remote, rel_path));
         CREATE TABLE IF NOT EXISTS binge_locks (series_key TEXT PRIMARY KEY, lock_json TEXT NOT NULL, updated_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS pending_profile_ops (
             seq INTEGER PRIMARY KEY AUTOINCREMENT, remote TEXT NOT NULL, profile TEXT NOT NULL,
             op_key TEXT NOT NULL, event_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_pending_profile_ops_scope ON pending_profile_ops(remote, profile, seq ASC);
         CREATE TABLE IF NOT EXISTS applied_profile_files (
             remote TEXT NOT NULL, profile TEXT NOT NULL, rel_path TEXT NOT NULL,
             PRIMARY KEY (remote, profile, rel_path)
         );
         CREATE TABLE IF NOT EXISTS profile_kv_hlc (
             remote TEXT NOT NULL, profile TEXT NOT NULL, ns TEXT NOT NULL, item_key TEXT NOT NULL,
             updated_hlc TEXT NOT NULL, payload_json TEXT NOT NULL,
             PRIMARY KEY (remote, profile, ns, item_key)
         );",
    )?;
    if legacy_kv || legacy_ops {
        if let Some((rem, prof)) = target_scope {
            for (ns, ikey, hlc, pjson) in old_kv {
                conn.execute(
                    "INSERT OR REPLACE INTO profile_kv_hlc(remote, profile, ns, item_key, updated_hlc, payload_json) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                    params![rem, prof, ns, ikey, hlc, pjson],
                )?;
            }
            for (okey, ejson) in old_ops {
                conn.execute(
                    "INSERT INTO pending_profile_ops(remote, profile, op_key, event_json) VALUES(?1, ?2, ?3, ?4)",
                    params![rem, prof, okey, ejson],
                )?;
            }
            conn.execute("DELETE FROM applied_profile_files WHERE remote <> ?1 OR profile <> ?2", params![rem, prof])?;
            conn.execute("DELETE FROM meta WHERE k LIKE 'profile_cursor|%' AND k <> ?1", params![format!("profile_cursor|{rem}|{prof}")])?;
        } else {
            conn.execute("DELETE FROM applied_profile_files", [])?;
            conn.execute("DELETE FROM meta WHERE k LIKE 'profile_cursor|%'", [])?;
        }
    }
    ensure_device_id(&conn)?;
    migrate_legacy_json_once(&conn)?;
    initialized.insert(path);
    Ok(conn)
}

fn ensure_device_id(conn: &Connection) -> Result<String> {
    if let Some(existing) = conn
        .query_row("SELECT v FROM meta WHERE k = 'device_id'", [], |r| r.get::<_, String>(0))
        .optional()?
    {
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let mut buf = [0u8; 8];
    let _ = getrandom::fill(&mut buf);
    let id: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    conn.execute("INSERT OR REPLACE INTO meta(k, v) VALUES('device_id', ?1)", params![id])?;
    Ok(id)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_hlc_parts(hlc: &str) -> Option<(u64, u64)> {
    let mut it = hlc.splitn(3, '-');
    let ms: u64 = it.next()?.parse().ok()?;
    let cnt: u64 = it.next()?.parse().ok()?;
    Some((ms, cnt.min(MAX_HLC_CNT)))
}

fn read_hlc_meta(conn: &Connection) -> Result<(u64, u64)> {
    let last_ms: u64 = conn
        .query_row("SELECT v FROM meta WHERE k = 'hlc_ms'", [], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let last_cnt: u64 = conn
        .query_row("SELECT v FROM meta WHERE k = 'hlc_cnt'", [], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
        .min(MAX_HLC_CNT);
    Ok((last_ms, last_cnt))
}

fn observe_hlc(conn: &Connection, remote_hlc: &str) -> Result<()> {
    let Some((rem_ms, rem_cnt)) = parse_hlc_parts(remote_hlc) else {
        return Ok(());
    };
    let max_ms = now_ms().saturating_add(MAX_HLC_DRIFT_MS);
    let (clamped_ms, clamped_cnt) = if rem_ms > max_ms {
        (max_ms, 0)
    } else {
        (rem_ms, rem_cnt)
    };
    let (last_ms, last_cnt) = read_hlc_meta(conn)?;
    if (clamped_ms, clamped_cnt) > (last_ms, last_cnt) {
        conn.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES('hlc_ms', ?1)",
            params![clamped_ms.to_string()],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES('hlc_cnt', ?1)",
            params![clamped_cnt.to_string()],
        )?;
    }
    Ok(())
}

fn next_hlc_after(conn: &Connection, floor_hlc: Option<&str>) -> Result<(String, String, u64)> {
    let device = ensure_device_id(conn)?;
    let now = now_ms();
    let (last_ms, last_cnt) = read_hlc_meta(conn)?;
    let (mut ms, mut cnt) = if now > last_ms {
        (now, 0)
    } else if last_cnt >= MAX_HLC_CNT {
        (last_ms + 1, 0)
    } else {
        (last_ms, last_cnt + 1)
    };
    let mut hlc = format!("{ms:015}-{cnt:06}-{device}");
    if let Some(floor) = floor_hlc.filter(|f| !f.is_empty()) {
        if hlc.as_str() <= floor {
            if let Some((f_ms, f_cnt)) = parse_hlc_parts(floor) {
                let (r_ms, r_cnt) = if f_cnt >= MAX_HLC_CNT {
                    (f_ms + 1, 0)
                } else {
                    (f_ms, f_cnt + 1)
                };
                hlc = format!("{r_ms:015}-{r_cnt:06}-{device}");
                if r_ms <= now.saturating_add(MAX_HLC_DRIFT_MS) && (r_ms, r_cnt) > (ms, cnt) {
                    ms = r_ms;
                    cnt = r_cnt;
                }
            }
        }
    }
    let seq: u64 = conn
        .query_row("SELECT v FROM meta WHERE k = 'local_seq'", [], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
        + 1;
    conn.execute("INSERT OR REPLACE INTO meta(k, v) VALUES('hlc_ms', ?1)", params![ms.to_string()])?;
    conn.execute("INSERT OR REPLACE INTO meta(k, v) VALUES('hlc_cnt', ?1)", params![cnt.to_string()])?;
    conn.execute("INSERT OR REPLACE INTO meta(k, v) VALUES('local_seq', ?1)", params![seq.to_string()])?;
    Ok((hlc, device, seq))
}

fn next_hlc(conn: &Connection) -> Result<(String, String, u64)> {
    next_hlc_after(conn, None)
}

fn migrate_legacy_json_once(conn: &Connection) -> Result<()> {
    let legacy_path = db_root_dir().join("downloads.json");
    if !legacy_path.is_file() {
        return Ok(());
    }
    let Ok(raw) = std::fs::read_to_string(&legacy_path) else {
        return Ok(());
    };
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw) {
        for e in items {
            let remote = {
                let r = s(&e, "remote");
                (!r.is_empty()).then_some(r)
            };
            let file = PathBuf::from(s(&e, "path"));
            if remote.is_none() && !file.is_file() {
                continue;
            }
            let Some(media) = Media::from_json(&e["media"], CINEMETA) else {
                continue;
            };
            let raw_vid = s(&e, "video_id");
            let (canon_key, canon_mid, canon_vid) =
                canonical_download_key_parts(&media.kind, &media.id, &media.raw, &raw_vid);
            let id = entry_row_id(&canon_key, remote.as_deref());
            let (hlc, _, _) = next_hlc(conn)?;
            let bytes = if remote.is_none() {
                std::fs::metadata(&file).map(|m| m.len()).unwrap_or_else(|_| e["bytes"].as_u64().unwrap_or(0))
            } else {
                e["bytes"].as_u64().unwrap_or(0)
            };
            let mjson = sanitized_media_json(&media, &canon_mid).to_string();
            conn.execute(
                "INSERT OR IGNORE INTO downloads(
                    id, canonical_key, media_kind, canonical_media_id, video_id, title, subtitle,
                    path, remote, bytes, downloaded_at, updated_hlc, deleted, media_json
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, ?13)",
                params![
                    id,
                    canon_key,
                    media.kind,
                    canon_mid,
                    canon_vid,
                    s(&e, "title"),
                    s(&e, "subtitle"),
                    file.to_string_lossy().as_ref(),
                    remote,
                    bytes as i64,
                    e["downloaded_at"].as_i64().unwrap_or_else(now_secs),
                    hlc,
                    mjson,
                ],
            )?;
        }
    }
    let migrated = legacy_path.with_extension("json.migrated");
    let _ = std::fs::rename(&legacy_path, migrated);
    Ok(())
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<DownloadEntry>> {
    let id: String = row.get(0)?;
    let key: String = row.get(1)?;
    let video_id: String = row.get(2)?;
    let title: String = row.get(3)?;
    let subtitle: String = row.get(4)?;
    let path_str: String = row.get(5)?;
    let remote: Option<String> = row.get(6)?;
    let bytes_raw: i64 = row.get(7)?;
    let downloaded_at: i64 = row.get(8)?;
    let media_str: String = row.get(9)?;

    let path = PathBuf::from(&path_str);
    if remote.is_none() && !path.is_file() {
        return Ok(None);
    }
    let Ok(mv) = serde_json::from_str::<Value>(&media_str) else {
        return Ok(None);
    };
    let Some(media) = Media::from_json(&mv, CINEMETA).map(canonicalize_catalog_media) else {
        return Ok(None);
    };
    let bytes = if remote.is_none() {
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(bytes_raw.max(0) as u64)
    } else {
        bytes_raw.max(0) as u64
    };
    Ok(Some(DownloadEntry {
        id,
        key,
        media,
        video_id,
        title,
        subtitle,
        path,
        remote,
        bytes,
        downloaded_at,
    }))
}

// ---------------------------------------------------------------- Local CRUD + queue hooks

pub fn upsert_download_entry(mut entry: DownloadEntry) -> Result<()> {
    let conn = open_db()?;
    let (canon_key, canon_mid, canon_vid) = canonical_download_key_parts(
        &entry.media.kind,
        &entry.media.id,
        &entry.media.raw,
        &entry.video_id,
    );
    entry.key = canon_key.clone();
    entry.video_id = canon_vid.clone();
    entry.media = canonicalize_catalog_media(entry.media);
    let row_id = entry_row_id(&canon_key, entry.remote.as_deref());
    let prev_hlc: Option<String> = conn
        .query_row("SELECT updated_hlc FROM downloads WHERE id = ?1", params![row_id], |r| r.get(0))
        .optional()?;
    let (hlc, device, seq) = next_hlc_after(&conn, prev_hlc.as_deref())?;
    let mjson_val = sanitized_media_json(&entry.media, &canon_mid);
    let mjson = mjson_val.to_string();

    conn.execute(
        "INSERT INTO downloads(
            id, canonical_key, media_kind, canonical_media_id, video_id, title, subtitle,
            path, remote, bytes, downloaded_at, updated_hlc, deleted, media_json
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, ?13)
         ON CONFLICT(id) DO UPDATE SET
            canonical_key = excluded.canonical_key,
            media_kind = excluded.media_kind,
            canonical_media_id = excluded.canonical_media_id,
            video_id = excluded.video_id,
            title = excluded.title,
            subtitle = excluded.subtitle,
            path = excluded.path,
            remote = excluded.remote,
            bytes = excluded.bytes,
            downloaded_at = excluded.downloaded_at,
            updated_hlc = excluded.updated_hlc,
            deleted = 0,
            media_json = excluded.media_json",
        params![
            row_id,
            canon_key,
            entry.media.kind,
            canon_mid,
            canon_vid,
            entry.title,
            entry.subtitle,
            entry.path.to_string_lossy().as_ref(),
            entry.remote,
            entry.bytes as i64,
            entry.downloaded_at,
            hlc,
            mjson,
        ],
    )?;

    if let Some(ref remote_name) = entry.remote {
        let ev = json!({
            "op": "upsert",
            "hlc": hlc,
            "device": device,
            "seq": seq,
            "canonical_key": canon_key,
            "media_kind": entry.media.kind,
            "canonical_media_id": canon_mid,
            "video_id": canon_vid,
            "title": entry.title,
            "subtitle": entry.subtitle,
            "path": entry.path.to_string_lossy(),
            "bytes": entry.bytes,
            "downloaded_at": entry.downloaded_at,
            "media": mjson_val,
        });
        conn.execute(
            "INSERT INTO pending_remote_ops(remote, event_json) VALUES(?1, ?2)",
            params![remote_name, ev.to_string()],
        )?;
        trigger_background_sync(remote_name.clone());
    }
    Ok(())
}

pub fn delete_download_entry(id_or_key: &str) -> Result<Vec<(PathBuf, Option<String>)>> {
    let conn = open_db()?;
    let mut stmt = conn.prepare(
        "SELECT id, canonical_key, media_kind, canonical_media_id, video_id, title, subtitle, path, remote, bytes, downloaded_at, media_json, updated_hlc
         FROM downloads
         WHERE deleted = 0 AND (id = ?1 OR canonical_key = ?1)",
    )?;
    let rows = stmt.query_map(params![id_or_key], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, i64>(9)?,
            r.get::<_, i64>(10)?,
            r.get::<_, String>(11)?,
            r.get::<_, String>(12)?,
        ))
    })?;
    let mut targets = Vec::new();
    let mut remotes_to_sync = HashSet::new();
    for item in rows.flatten() {
        let (row_id, canon_key, mkind, cmid, vid, title, subtitle, path_str, remote, bytes, dl_at, mjson, prev_hlc) = item;
        let (hlc, device, seq) = next_hlc_after(&conn, Some(&prev_hlc))?;
        conn.execute(
            "UPDATE downloads SET deleted = 1, updated_hlc = ?2 WHERE id = ?1",
            params![row_id, hlc],
        )?;
        if let Some(ref rname) = remote {
            let media_val: Value = serde_json::from_str(&mjson).unwrap_or_else(|_| json!({}));
            let ev = json!({
                "op": "delete",
                "hlc": hlc,
                "device": device,
                "seq": seq,
                "canonical_key": canon_key,
                "media_kind": mkind,
                "canonical_media_id": cmid,
                "video_id": vid,
                "title": title,
                "subtitle": subtitle,
                "path": path_str,
                "bytes": bytes.max(0) as u64,
                "downloaded_at": dl_at,
                "media": media_val,
            });
            conn.execute(
                "INSERT INTO pending_remote_ops(remote, event_json) VALUES(?1, ?2)",
                params![rname, ev.to_string()],
            )?;
            remotes_to_sync.insert(rname.clone());
        }
        targets.push((PathBuf::from(path_str), remote));
    }
    for r in remotes_to_sync {
        trigger_background_sync(r);
    }
    Ok(targets)
}

/// Remove cached entries when a remote is disconnected/deleted in Settings (never deletes cloud files).
pub fn purge_disconnected_remote(remote_name: &str) {
    let Ok(conn) = open_db() else { return };
    let cursor_prefix = format!("profile_cursor|{remote_name}|");
    let _ = conn.execute("DELETE FROM downloads WHERE remote = ?1", params![remote_name]);
    let _ = conn.execute("DELETE FROM pending_remote_ops WHERE remote = ?1", params![remote_name]);
    let _ = conn.execute("DELETE FROM applied_remote_files WHERE remote = ?1", params![remote_name]);
    let _ = conn.execute("DELETE FROM applied_profile_files WHERE remote = ?1", params![remote_name]);
    let _ = conn.execute("DELETE FROM pending_profile_ops WHERE remote = ?1", params![remote_name]);
    let _ = conn.execute("DELETE FROM profile_kv_hlc WHERE remote = ?1", params![remote_name]);
    let _ = conn.execute(
        "DELETE FROM meta WHERE substr(k, 1, ?2) = ?1",
        params![cursor_prefix, cursor_prefix.len() as i64],
    );
}

pub fn list_all_downloads() -> Vec<DownloadEntry> {
    let Ok(conn) = open_db() else { return Vec::new() };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, canonical_key, video_id, title, subtitle, path, remote, bytes, downloaded_at, media_json
         FROM downloads
         WHERE deleted = 0
         ORDER BY downloaded_at DESC, id ASC",
    ) else {
        return Vec::new();
    };
    stmt.query_map([], row_to_entry)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .flatten()
        .collect()
}

/// Indexed, paginated query for enormous libraries (`LIMIT` + `OFFSET` + optional title filter).
pub fn list_downloads_paged(query: &str, limit: usize, offset: usize) -> (Vec<DownloadEntry>, usize) {
    let Ok(conn) = open_db() else { return (Vec::new(), 0) };
    let q = query.trim();
    if q.is_empty() {
        let total: usize = conn
            .query_row("SELECT COUNT(*) FROM downloads WHERE deleted = 0", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
            .max(0) as usize;
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, canonical_key, video_id, title, subtitle, path, remote, bytes, downloaded_at, media_json
             FROM downloads
             WHERE deleted = 0
             ORDER BY downloaded_at DESC, id ASC
             LIMIT ?1 OFFSET ?2",
        ) else {
            return (Vec::new(), total);
        };
        let items: Vec<DownloadEntry> = stmt
            .query_map(params![limit as i64, offset as i64], row_to_entry)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .flatten()
            .collect();
        (items, total)
    } else {
        let pat = format!("%{}%", q.replace('%', "\\%").replace('_', "\\_"));
        let total: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM downloads WHERE deleted = 0 AND title LIKE ?1 ESCAPE '\\' COLLATE NOCASE",
                params![pat],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            .max(0) as usize;
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, canonical_key, video_id, title, subtitle, path, remote, bytes, downloaded_at, media_json
             FROM downloads
             WHERE deleted = 0 AND title LIKE ?1 ESCAPE '\\' COLLATE NOCASE
             ORDER BY downloaded_at DESC, id ASC
             LIMIT ?2 OFFSET ?3",
        ) else {
            return (Vec::new(), total);
        };
        let items: Vec<DownloadEntry> = stmt
            .query_map(params![pat, limit as i64, offset as i64], row_to_entry)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .flatten()
            .collect();
        (items, total)
    }
}

pub fn find_downloads_for_video(kind: &str, video_id: &str) -> Vec<DownloadEntry> {
    let Ok(conn) = open_db() else { return Vec::new() };
    let norm_vid = if let Some(imdb) = extract_imdb_id(video_id, &Value::Null) {
        if let Some((_, suf)) = video_id.split_once(':') {
            format!("{imdb}:{suf}")
        } else {
            imdb
        }
    } else {
        video_id.trim().to_owned()
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, canonical_key, video_id, title, subtitle, path, remote, bytes, downloaded_at, media_json
         FROM downloads
         WHERE media_kind = ?1 AND (video_id = ?2 OR video_id = ?3) AND deleted = 0
         ORDER BY (remote IS NOT NULL) ASC, downloaded_at DESC",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![kind.trim(), norm_vid, video_id.trim()], row_to_entry)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .flatten()
        .collect()
}

#[cfg(test)]
pub fn find_best_download(canonical_key: &str) -> Option<DownloadEntry> {
    let Ok(conn) = open_db() else { return None };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, canonical_key, video_id, title, subtitle, path, remote, bytes, downloaded_at, media_json
         FROM downloads
         WHERE canonical_key = ?1 AND deleted = 0
         ORDER BY (remote IS NOT NULL) ASC, downloaded_at DESC
         LIMIT 1",
    ) else {
        return None;
    };
    let out = stmt
        .query_map(params![canonical_key], row_to_entry)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .flatten()
        .next();
    out
}

/// Build `Source` entries for every saved copy (local device first, then rclone remotes) of `(kind, video_id)`.
pub fn saved_sources_for(kind: &str, video_id: &str) -> Vec<Source> {
    find_downloads_for_video(kind, video_id)
        .into_iter()
        .map(|entry| {
            let addon = match &entry.remote {
                Some(r) => format!("Rclone ({r})"),
                None => "Local device".into(),
            };
            let badge = if entry.remote.is_some() { "Cloud Saved" } else { "Saved Offline" };
            let file_label = entry
                .remote
                .as_ref()
                .and_then(|_| entry.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| entry.title.clone());
            let quality = sources::find_quality(&entry.subtitle).or(Some("SAVED"));
            let hdr = sources::find_hdr(&entry.subtitle);
            let size = (entry.bytes > 0).then(|| file_size_label(entry.bytes));
            let desc = if entry.subtitle.is_empty() {
                format!("{badge} · {}", entry.title)
            } else {
                format!("{badge} · {} · {}", entry.subtitle, entry.title)
            };
            Source {
                addon,
                name: badge.into(),
                description: desc.clone(),
                url: if entry.remote.is_none() {
                    entry.path.to_string_lossy().into_owned()
                } else {
                    String::new()
                },
                filename: file_label.clone(),
                size,
                size_bytes: (entry.bytes > 0).then_some(entry.bytes),
                declared_size: entry.bytes > 0,
                labelled_bytes: None,
                pack: false,
                quality,
                hdr,
                kind: SourceKind::Direct,
                info_hash: None,
                file_idx: None,
                tracker_sources: Vec::new(),
                seeders: None,
                binge_group: None,
                indexer: None,
                headers: Vec::new(),
                label: format!("{badge} {desc} {file_label}"),
                off_title: false,
                picture: None,
                saved: Some(sources::SavedSource {
                    entry_id: entry.id,
                    path: entry.path,
                    remote: entry.remote,
                }),
            }
        })
        .collect()
}

// ---------------------------------------------------------------- Series bingeGroup locks

fn active_profile_scope(conn: &Connection) -> Option<(String, String)> {
    let cfg = store::load_settings();
    if let Some(rem) = cfg.sync_remote.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        return Some((rem.to_owned(), store::sanitize_profile_name(&cfg.sync_profile)));
    }
    let mut stmt = conn.prepare("SELECT k FROM meta WHERE k LIKE 'profile_cursor|%' LIMIT 2").ok()?;
    let keys: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(0)).ok()?.flatten().collect();
    if keys.len() == 1 {
        let rest = keys[0].strip_prefix("profile_cursor|")?;
        let (rem, prof) = rest.rsplit_once('|')?;
        if !rem.is_empty() && !prof.is_empty() {
            return Some((rem.to_owned(), prof.to_owned()));
        }
    }
    None
}

fn queue_scoped_profile_op(
    conn: &Connection,
    remote: &str,
    profile: &str,
    ns: &str,
    item_key: &str,
    build_ev: impl FnOnce(&str, Option<&Value>) -> Value,
) -> Result<()> {
    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT updated_hlc, payload_json FROM profile_kv_hlc
             WHERE remote = ?1 AND profile = ?2 AND ns = ?3 AND item_key = ?4",
            params![remote, profile, ns, item_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let prev_hlc = existing.as_ref().map(|(h, _)| h.as_str());
    let prev_val = existing
        .as_ref()
        .and_then(|(_, j)| serde_json::from_str::<Value>(j).ok());
    let (hlc, _, _) = next_hlc_after(conn, prev_hlc)?;
    let ev_str = build_ev(&hlc, prev_val.as_ref()).to_string();
    conn.execute(
        "INSERT OR REPLACE INTO profile_kv_hlc(remote, profile, ns, item_key, updated_hlc, payload_json)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![remote, profile, ns, item_key, hlc, ev_str],
    )?;
    conn.execute(
        "INSERT INTO pending_profile_ops(remote, profile, op_key, event_json) VALUES(?1, ?2, ?3, ?4)",
        params![remote, profile, format!("{ns}|{item_key}"), ev_str],
    )?;
    Ok(())
}

pub fn save_binge_lock(media: &Media, lock: &StreamMatcherLock) {
    if media.kind == "movie" {
        return;
    }
    let Ok(conn) = open_db() else { return };
    let key = canonical_series_key(media);
    let now = now_secs();
    let lock_val = lock.to_json();
    let _ = conn.execute(
        "INSERT OR REPLACE INTO binge_locks(series_key, lock_json, updated_at) VALUES(?1, ?2, ?3)",
        params![key, lock_val.to_string(), now],
    );
    if let Some((rem, prof)) = active_profile_scope(&conn) {
        let _ = queue_scoped_profile_op(&conn, &rem, &prof, "binge_lock", &key, |hlc, prev| {
            let prev_at = prev.and_then(|v| v["updated_at"].as_i64()).unwrap_or(0);
            json!({
                "op": "binge_lock",
                "hlc": hlc,
                "key": key,
                "lock": lock_val,
                "updated_at": now.max(prev_at + 1),
            })
        });
        trigger_profile_sync();
    }
}

pub fn load_binge_lock(media: &Media) -> Option<StreamMatcherLock> {
    if media.kind == "movie" {
        return None;
    }
    let conn = open_db().ok()?;
    let key = canonical_series_key(media);
    let raw: String = conn
        .query_row("SELECT lock_json FROM binge_locks WHERE series_key = ?1", params![key], |r| r.get(0))
        .optional()
        .ok()??;
    let v: Value = serde_json::from_str(&raw).ok()?;
    StreamMatcherLock::from_json(&v)
}

// ---------------------------------------------------------------- Append-only remote `.db/` sync & 2-tier compaction

fn normalize_fs(remote_name: &str) -> String {
    let t = remote_name.trim();
    if t.ends_with(':') { t.to_owned() } else { format!("{t}:") }
}

fn trigger_background_sync(remote_name: String) {
    if let Some(rt) = RUNTIME.get() {
        rt.spawn_blocking(move || {
            let _ = sync_remote_db(&remote_name);
        });
    }
}

pub fn trigger_startup_remote_sync() {
    if let Some(rt) = RUNTIME.get() {
        rt.spawn_blocking(|| {
            let _ = sync_all_remotes_blocking();
        });
    }
}

pub fn sync_all_remotes_blocking() -> Result<usize> {
    let raw = rstream_rclone::list_remotes()?;
    let visible = filter_visible_rclone_remotes(raw);
    let mut total = 0usize;
    for r in visible {
        if let Ok(n) = sync_remote_db(&r.name) {
            total += n;
        }
    }
    if let Ok(n) = sync_active_profile_blocking() {
        total += n;
    }
    Ok(total)
}

fn apply_remote_event_line(tx: &rusqlite::Transaction<'_>, remote_name: &str, ev: &Value) -> Result<bool> {
    let op = s(ev, "op");
    if op != "upsert" && op != "delete" {
        return Ok(false);
    }
    let hlc = s(ev, "hlc");
    let canon_key = s(ev, "canonical_key");
    if hlc.is_empty() || canon_key.is_empty() {
        return Ok(false);
    }
    let row_id = entry_row_id(&canon_key, Some(remote_name));
    let existing_hlc: Option<String> = tx
        .query_row(
            "SELECT updated_hlc FROM downloads WHERE id = ?1",
            params![row_id],
            |r| r.get(0),
        )
        .optional()?;
    if existing_hlc.as_deref().is_some_and(|cur| cur >= hlc.as_str()) {
        return Ok(false);
    }
    observe_hlc(tx, &hlc)?;
    let deleted = i32::from(op == "delete");
    let mkind = {
        let k = s(ev, "media_kind");
        if k.is_empty() { "movie".into() } else { k }
    };
    let cmid = s(ev, "canonical_media_id");
    let vid = s(ev, "video_id");
    let title = s(ev, "title");
    let subtitle = s(ev, "subtitle");
    let path = s(ev, "path");
    let bytes = ev["bytes"].as_u64().unwrap_or(0) as i64;
    let dl_at = ev["downloaded_at"].as_i64().unwrap_or_else(now_secs);
    let media_obj = if let Some(m) = Media::from_json(&ev["media"], CINEMETA) {
        sanitized_media_json(&m, if cmid.is_empty() { &m.id } else { &cmid }).to_string()
    } else {
        json!({"id": cmid, "type": mkind, "name": title, "addon_url": safe_sync_addon_url(&cmid)}).to_string()
    };

    tx.execute(
        "INSERT INTO downloads(
            id, canonical_key, media_kind, canonical_media_id, video_id, title, subtitle,
            path, remote, bytes, downloaded_at, updated_hlc, deleted, media_json
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(id) DO UPDATE SET
            canonical_key = excluded.canonical_key,
            media_kind = excluded.media_kind,
            canonical_media_id = excluded.canonical_media_id,
            video_id = excluded.video_id,
            title = excluded.title,
            subtitle = excluded.subtitle,
            path = excluded.path,
            remote = excluded.remote,
            bytes = excluded.bytes,
            downloaded_at = excluded.downloaded_at,
            updated_hlc = excluded.updated_hlc,
            deleted = excluded.deleted,
            media_json = excluded.media_json",
        params![row_id, canon_key, mkind, cmid, vid, title, subtitle, path, remote_name, bytes, dl_at, hlc, deleted, media_obj],
    )?;
    Ok(true)
}

fn parse_and_apply_remote_blob(
    conn: &mut Connection,
    remote_name: &str,
    rel_path: &str,
    bytes: &[u8],
) -> Result<(usize, Vec<String>)> {
    let text = String::from_utf8_lossy(bytes);
    let tx = conn.transaction()?;
    let mut changed = 0usize;
    let mut covered = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("op").and_then(Value::as_str) == Some("meta") {
            for c in v["covered"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                covered.push(c.to_owned());
                let _ = tx.execute(
                    "INSERT OR IGNORE INTO applied_remote_files(remote, rel_path) VALUES(?1, ?2)",
                    params![remote_name, c],
                );
            }
            continue;
        }
        if apply_remote_event_line(&tx, remote_name, &v)? {
            changed += 1;
        }
    }
    tx.execute(
        "INSERT OR IGNORE INTO applied_remote_files(remote, rel_path) VALUES(?1, ?2)",
        params![remote_name, rel_path],
    )?;
    tx.commit()?;
    Ok((changed, covered))
}

fn normalize_rel_under(prefix: &str, file_path: &str) -> String {
    let p = file_path.trim_start_matches('/');
    if p.starts_with(prefix) { p.to_owned() } else { format!("{prefix}/{p}") }
}

fn extract_covered_paths(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(bytes).lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("op").and_then(Value::as_str) == Some("meta") {
                for c in v["covered"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    out.push(c.to_owned());
                }
            }
        }
    }
    out
}

struct TierListing {
    snapshots: Vec<String>,
    deltas: Vec<String>,
    events: Vec<String>,
    blobs: Vec<(String, Vec<u8>)>,
}

fn fetch_unapplied_tiers(
    remote_fs: &str,
    prefix: &str,
    applied: &mut HashSet<String>,
    fetched_events: &mut HashMap<String, Vec<u8>>,
) -> Result<TierListing> {
    let files = rstream_rclone::list_remote_files_recursive(remote_fs, prefix)?;
    let (mut snapshots, mut deltas, mut events) = (Vec::new(), Vec::new(), Vec::new());
    let snap_pfx = format!("{prefix}/snapshots/base-");
    let delta_pfx = format!("{prefix}/deltas/delta-");
    let ev_pfx = format!("{prefix}/events/");
    for f in files {
        if f.is_dir || !f.path.ends_with(".jsonl") { continue; }
        let rel = normalize_rel_under(prefix, &f.path);
        if rel.starts_with(&snap_pfx) { snapshots.push(rel); }
        else if rel.starts_with(&delta_pfx) { deltas.push(rel); }
        else if rel.starts_with(&ev_pfx) { events.push(rel); }
    }
    snapshots.sort();
    deltas.sort();
    events.sort();

    let mut snap_blobs = Vec::new();
    for snap in snapshots.iter().rev() {
        if applied.contains(snap) { continue; }
        let bytes = rstream_rclone::get_remote_bytes(remote_fs, snap)?;
        for c in extract_covered_paths(&bytes) { applied.insert(c); }
        applied.insert(snap.clone());
        snap_blobs.push((snap.clone(), bytes));
    }
    snap_blobs.reverse();
    let mut blobs = snap_blobs;
    for delta in &deltas {
        if applied.contains(delta) { continue; }
        let bytes = rstream_rclone::get_remote_bytes(remote_fs, delta)?;
        for c in extract_covered_paths(&bytes) { applied.insert(c); }
        applied.insert(delta.clone());
        blobs.push((delta.clone(), bytes));
    }
    for ev_path in &events {
        if applied.contains(ev_path) { continue; }
        let bytes = rstream_rclone::get_remote_bytes(remote_fs, ev_path)?;
        applied.insert(ev_path.clone());
        fetched_events.insert(ev_path.clone(), bytes.clone());
        blobs.push((ev_path.clone(), bytes));
    }
    Ok(TierListing { snapshots, deltas, events, blobs })
}

fn write_compacted_blob_and_prune(
    remote_fs: &str,
    target_path: &str,
    covered: &[String],
    lines: impl IntoIterator<Item = String>,
    mark_applied: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    let mut out = serde_json::to_string(&json!({ "op": "meta", "covered": covered }))?;
    out.push('\n');
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    rstream_rclone::put_remote_bytes(remote_fs, target_path, out.as_bytes())?;
    mark_applied(target_path)?;
    for p in covered {
        let _ = rstream_rclone::delete_remote_file(remote_fs, p);
    }
    Ok(())
}

/// Synchronize local SQLite with `<remote_name>:RStream/.db/`.
pub fn sync_remote_db(remote_name: &str) -> Result<usize> {
    static SYNC_LOCK: Mutex<()> = Mutex::new(());
    let _serial = SYNC_LOCK.lock().map_err(|_| anyhow!("remote db sync lock poisoned"))?;
    let _busy = SyncBusyGuard::enter();
    let remote_fs = normalize_fs(remote_name);
    let mut fetched_events: HashMap<String, Vec<u8>> = HashMap::new();

    {
        let conn = open_db()?;
        let device = ensure_device_id(&conn)?;
        let pending: Vec<(i64, String)> = conn
            .prepare("SELECT seq, event_json FROM pending_remote_ops WHERE remote = ?1 ORDER BY seq ASC")?
            .query_map(params![remote_name], |r| Ok((r.get(0)?, r.get(1)?)))?
            .flatten()
            .collect();
        if !pending.is_empty() {
            let (batch_hlc, _, _) = next_hlc(&conn)?;
            let rel_path = format!("RStream/.db/events/{device}/{batch_hlc}.jsonl");
            let mut payload = String::new();
            for (_, line) in &pending {
                payload.push_str(line);
                payload.push('\n');
            }
            rstream_rclone::put_remote_bytes(&remote_fs, &rel_path, payload.as_bytes())?;
            conn.execute("INSERT OR IGNORE INTO applied_remote_files(remote, rel_path) VALUES(?1, ?2)", params![remote_name, rel_path])?;
            let max_seq = pending.last().map(|(s, _)| *s).unwrap_or(0);
            conn.execute("DELETE FROM pending_remote_ops WHERE remote = ?1 AND seq <= ?2", params![remote_name, max_seq])?;
            fetched_events.insert(rel_path, payload.into_bytes());
        }
    }

    let mut conn = open_db()?;
    let mut applied: HashSet<String> = conn
        .prepare("SELECT rel_path FROM applied_remote_files WHERE remote = ?1")?
        .query_map(params![remote_name], |r| r.get::<_, String>(0))?
        .flatten()
        .collect();
    let tiers = fetch_unapplied_tiers(&remote_fs, "RStream/.db", &mut applied, &mut fetched_events)?;
    let mut total_changed = 0usize;
    for (rel, bytes) in &tiers.blobs {
        let (changed, _) = parse_and_apply_remote_blob(&mut conn, remote_name, rel, bytes)?;
        total_changed += changed;
    }

    if tiers.events.len() >= EVENT_COMPACT_THRESHOLD {
        compact_events_into_delta(&conn, remote_name, &remote_fs, &tiers.events, &fetched_events)?;
    } else if tiers.deltas.len() >= DELTA_COMPACT_THRESHOLD {
        compact_into_base_snapshot(&conn, remote_name, &remote_fs, &tiers.snapshots, &tiers.deltas)?;
    }
    Ok(total_changed)
}

fn serialize_remote_rows(conn: &Connection, remote_name: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT canonical_key, media_kind, canonical_media_id, video_id, title, subtitle, path, bytes, downloaded_at, updated_hlc, deleted, media_json
         FROM downloads WHERE remote = ?1 ORDER BY updated_hlc ASC",
    )?;
    let rows = stmt.query_map(params![remote_name], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
            r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?, r.get::<_, String>(9)?, r.get::<_, i32>(10)?, r.get::<_, String>(11)?))
    })?;
    let mut out = Vec::new();
    for item in rows.flatten() {
        let (ckey, mkind, cmid, vid, title, subtitle, path, bytes, dl_at, hlc, deleted, mjson) = item;
        let media_val: Value = serde_json::from_str(&mjson).unwrap_or_else(|_| json!({}));
        out.push(json!({
            "op": if deleted != 0 { "delete" } else { "upsert" }, "hlc": hlc, "canonical_key": ckey,
            "media_kind": mkind, "canonical_media_id": cmid, "video_id": vid, "title": title,
            "subtitle": subtitle, "path": path, "bytes": bytes.max(0) as u64, "downloaded_at": dl_at, "media": media_val,
        }).to_string());
    }
    Ok(out)
}

fn collect_latest_events(
    remote_fs: &str,
    events: &[String],
    cached_events: &HashMap<String, Vec<u8>>,
    key_of: impl Fn(&Value) -> Option<String>,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut latest: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut covered = Vec::with_capacity(events.len());
    for ev_path in events {
        let owned;
        let bytes: &[u8] = match cached_events.get(ev_path) {
            Some(b) => b.as_slice(),
            None => {
                owned = rstream_rclone::get_remote_bytes(remote_fs, ev_path)?;
                owned.as_slice()
            }
        };
        covered.push(ev_path.clone());
        for line in String::from_utf8_lossy(bytes).lines().map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let hlc = s(&v, "hlc");
            let Some(k) = key_of(&v).filter(|_| !hlc.is_empty()) else { continue };
            if latest.get(&k).is_none_or(|(prev_hlc, _)| hlc > *prev_hlc) {
                latest.insert(k, (hlc, line.to_owned()));
            }
        }
    }
    Ok((covered, latest.into_values().map(|(_, line)| line).collect()))
}

fn compact_events_into_delta(
    conn: &Connection,
    remote_name: &str,
    remote_fs: &str,
    events: &[String],
    cached_events: &HashMap<String, Vec<u8>>,
) -> Result<()> {
    let (covered, lines) = collect_latest_events(remote_fs, events, cached_events, |v| {
        let k = s(v, "canonical_key");
        (!k.is_empty()).then_some(k)
    })?;
    if covered.is_empty() { return Ok(()); }
    let (hlc, _, _) = next_hlc(conn)?;
    let delta_path = format!("RStream/.db/deltas/delta-{hlc}.jsonl");
    write_compacted_blob_and_prune(remote_fs, &delta_path, &covered, lines, |p| {
        conn.execute("INSERT OR IGNORE INTO applied_remote_files(remote, rel_path) VALUES(?1, ?2)", params![remote_name, p])?;
        Ok(())
    })
}

fn compact_into_base_snapshot(
    conn: &Connection,
    remote_name: &str,
    remote_fs: &str,
    snapshots: &[String],
    deltas: &[String],
) -> Result<()> {
    let mut covered: Vec<String> = snapshots.iter().chain(deltas.iter()).cloned().collect();
    covered.sort();
    covered.dedup();
    let lines = serialize_remote_rows(conn, remote_name)?;
    let (hlc, _, _) = next_hlc(conn)?;
    let snap_path = format!("RStream/.db/snapshots/base-{hlc}.jsonl");
    write_compacted_blob_and_prune(remote_fs, &snap_path, &covered, lines, |p| {
        conn.execute("INSERT OR IGNORE INTO applied_remote_files(remote, rel_path) VALUES(?1, ?2)", params![remote_name, p])?;
        Ok(())
    })
}

// ---------------------------------------------------------------- Personal Profile Sync (`RStream/.profiles/<profile>/`)
//
// Syncs Library bookmarks (`saved`), watched marks (`watched`), ratings (`liked`), playback
// positions (`position_ms`, `duration_ms`, `watched_ms`, `video_id`), `bingeGroup` locks, and
// per-title audio/subtitle `TrackPref` across devices via immutable per-device HLC event batches
// and 2-tier compaction (`deltas/` + `snapshots/`), keeping personal state separate from shared
// `RStream/.db/` media catalogs.

fn safe_sync_addon_url(canon_id: &str) -> &'static str {
    if canon_id.starts_with("kitsu:") {
        "https://anime-kitsu.strem.fun/manifest.json"
    } else {
        CINEMETA
    }
}

fn scrub_loopback_urls(value: &mut Value) {
    match value {
        Value::String(text) => {
            if text.starts_with("http://127.0.0.1:") || text.starts_with("http://localhost:") {
                text.clear();
            }
        }
        Value::Object(map) => {
            map.remove("streams");
            map.remove("addon_url");
            map.remove("_addon_url");
            for v in map.values_mut() {
                scrub_loopback_urls(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                scrub_loopback_urls(v);
            }
        }
        _ => {}
    }
}

pub(crate) fn canonicalize_track_pref_key(raw_key: &str) -> String {
    let mut parts = raw_key.splitn(3, '|');
    let kind = parts.next().unwrap_or("movie").trim();
    let raw_id = parts.next().unwrap_or(raw_key).trim();
    let canon_id = extract_imdb_id(raw_id, &Value::Null).unwrap_or_else(|| raw_id.to_owned());
    format!("{kind}|{canon_id}|{}", safe_sync_addon_url(&canon_id))
}

pub(crate) fn queue_track_pref_sync(media_key: &str, pref: &TrackPref) {
    let Ok(conn) = open_db() else { return };
    let Some((rem, prof)) = active_profile_scope(&conn) else { return };
    let canon_key = canonicalize_track_pref_key(media_key);
    let now = now_secs();
    let _ = queue_scoped_profile_op(&conn, &rem, &prof, "track_pref", &canon_key, |hlc, prev| {
        let prev_at = prev.and_then(|v| v["updated_at"].as_i64()).unwrap_or(0);
        json!({
            "op": "track_pref",
            "hlc": hlc,
            "key": canon_key,
            "pref": {
                "audio": pref.audio_lang,
                "sub": pref.sub_lang,
                "sub_off": pref.sub_off,
            },
            "updated_at": now.max(prev_at + 1),
        })
    });
    trigger_profile_sync();
}

pub fn queue_media_forget_tombstone(media: &Media) {
    let Ok(conn) = open_db() else { return };
    let Some((rem, prof)) = active_profile_scope(&conn) else { return };
    let canon_id = canonical_media_id(media);
    let key = format!("{}|{}", media.kind.trim(), canon_id);
    let now = now_secs();
    let _ = queue_scoped_profile_op(&conn, &rem, &prof, "media", &key, |hlc, prev| {
        let prev_at = prev.and_then(|v| v["item"]["updated"].as_i64()).unwrap_or(0);
        json!({
            "op": "media",
            "hlc": hlc,
            "key": key,
            "item": {
                "type": media.kind.trim(),
                "id": canon_id,
                "addon_url": safe_sync_addon_url(&canon_id),
                "deleted": true,
                "updated": now.max(prev_at + 1),
            }
        })
    });
}

pub fn queue_episodes_unwatched_sync(media: &Media, video_ids: &[String]) {
    let Ok(conn) = open_db() else { return };
    let Some((rem, prof)) = active_profile_scope(&conn) else { return };
    let canon_id = canonical_media_id(media);
    let addon = safe_sync_addon_url(&canon_id);
    let now = now_secs();
    for raw_vid in video_ids {
        let canon_vid = canonical_video_id(&media.id, &canon_id, raw_vid);
        let key = format!("{}|{}|{}", media.kind.trim(), canon_id, canon_vid);
        let _ = queue_scoped_profile_op(&conn, &rem, &prof, "playback", &key, |hlc, prev| {
            let prev_at = prev.and_then(|v| v["item"]["updated"].as_i64()).unwrap_or(0);
            json!({
                "op": "playback",
                "hlc": hlc,
                "key": key,
                "item": {
                    "type": media.kind.trim(),
                    "id": canon_id,
                    "addon_url": addon,
                    "video_id": canon_vid,
                    "position_ms": 0,
                    "duration_ms": 0,
                    "watched_ms": 0,
                    "watched": false,
                    "updated": now.max(prev_at + 1),
                }
            })
        });
    }
}

/// Coalesced background profile sync (waits 1.2s to batch rapid UI toggles into a single upload).
pub fn trigger_profile_sync() {
    let Some(rt) = RUNTIME.get() else { return };
    if BACKEND.get().is_none() {
        return;
    }
    if load_settings().sync_remote.is_none() {
        return;
    }
    if PROFILE_DEBOUNCE_QUEUED.swap(true, Ordering::SeqCst) {
        return;
    }
    rt.spawn(async move {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        PROFILE_DEBOUNCE_QUEUED.store(false, Ordering::SeqCst);
        let _ = tokio::task::spawn_blocking(sync_active_profile_blocking).await;
    });
}

/// Immediate background profile sync (used on player finish/close, window hide-to-tray, and Android pause/resume).
pub fn trigger_profile_sync_now() {
    let Some(rt) = RUNTIME.get() else { return };
    if BACKEND.get().is_none() {
        return;
    }
    if load_settings().sync_remote.is_none() {
        return;
    }
    rt.spawn_blocking(|| {
        let _ = sync_active_profile_blocking();
    });
}

/// Awaitable manual sync for Settings & System Tray "Sync now".
pub async fn sync_profile_now() -> Result<usize> {
    let cfg = load_settings();
    let remote = cfg
        .sync_remote
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .context("Select a sync remote in Settings first")?
        .to_owned();
    let profile = sanitize_profile_name(&cfg.sync_profile);
    on_runtime(async move {
        tokio::task::spawn_blocking(move || sync_profile_remote(&remote, &profile))
            .await
            .context("profile sync worker failed")?
    })
    .await?
}

pub fn sync_active_profile_blocking() -> Result<usize> {
    if BACKEND.get().is_none() {
        return Ok(0);
    }
    let cfg = load_settings();
    let Some(remote) = cfg.sync_remote.as_deref().map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(0);
    };
    let profile = sanitize_profile_name(&cfg.sync_profile);
    sync_profile_remote(remote, &profile)
}

fn exec_core_blocking(core: &Core, req: Value) -> Result<Value> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
            return tokio::task::block_in_place(|| handle.block_on(core.execute(req)));
        }
        return std::thread::scope(|s| {
            s.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(core.execute(req))
            })
            .join()
            .map_err(|_| anyhow!("core worker panicked"))?
        });
    }
    if let Some(rt) = RUNTIME.get() {
        return rt.block_on(core.execute(req));
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(core.execute(req))
}

fn capture_local_core_changes(
    conn: &mut Connection,
    remote_name: &str,
    profile: &str,
    core: Option<&Core>,
) -> Result<()> {
    let cursor_key = format!("profile_cursor|{remote_name}|{profile}");
    let since: i64 = conn
        .query_row("SELECT v FROM meta WHERE k = ?1", params![cursor_key], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut media_pages: Vec<Value> = Vec::new();
    let mut playback_pages: Vec<Value> = Vec::new();
    let mut max_seen = since;
    let mut last_now = since;

    if let Some(core) = core {
        let mut page_since = since.saturating_sub(1);
        let mut seen_media = HashSet::new();
        let mut seen_playback = HashSet::new();
        loop {
            let changes = exec_core_blocking(
                core,
                json!({
                    "op": "sync_changes",
                    "since": page_since,
                }),
            )?;
            let media_arr = changes["media"].as_array().cloned().unwrap_or_default();
            let playback_arr = changes["playback"].as_array().cloned().unwrap_or_default();
            let media_full = media_arr.len() == 5000;
            let playback_full = playback_arr.len() == 20000;
            let mut batch_media_max = page_since;
            for m in media_arr {
                let u = m["updated"].as_i64().unwrap_or(0);
                batch_media_max = batch_media_max.max(u);
                if seen_media.insert((s(&m, "key"), u)) {
                    media_pages.push(m);
                }
            }
            let mut batch_playback_max = page_since;
            for p in playback_arr {
                let u = p["updated"].as_i64().unwrap_or(0);
                batch_playback_max = batch_playback_max.max(u);
                if seen_playback.insert((format!("{}|{}", s(&p, "key"), s(&p, "video_id")), u)) {
                    playback_pages.push(p);
                }
            }
            max_seen = max_seen.max(batch_media_max).max(batch_playback_max);
            last_now = changes["now"].as_i64().unwrap_or(max_seen);

            if !media_full && !playback_full {
                break;
            }
            // Step to `max - 1` when possible so rows sharing the page-boundary second are not skipped.
            let step = |m: i64| if m - 1 > page_since { m - 1 } else { m };
            let next_since = match (media_full, playback_full) {
                (true, true) => step(batch_media_max).min(step(batch_playback_max)),
                (true, false) => step(batch_media_max),
                (false, true) => step(batch_playback_max),
                (false, false) => break,
            };
            if next_since <= page_since {
                break;
            }
            page_since = next_since;
        }
    }

    let seed_prefs = since == 0 && conn
        .query_row(
            "SELECT 1 FROM profile_kv_hlc WHERE remote = ?1 AND profile = ?2 LIMIT 1",
            params![remote_name, profile],
            |_| Ok(()),
        )
        .optional()?
        .is_none();
    let track_prefs_seed = if seed_prefs {
        store::all_track_prefs_snapshot()
    } else {
        HashMap::new()
    };

    let tx = conn.transaction()?;

    // Seed binge_locks & track_prefs onto a newly connected `(remote, profile)` (`since == 0`).
    if seed_prefs {
        let mut stmt = tx.prepare("SELECT series_key, lock_json, updated_at FROM binge_locks")?;
        let locks: Vec<(String, String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .flatten()
            .collect();
        drop(stmt);
        for (skey, ljson, uat) in locks {
            if let Ok(lval) = serde_json::from_str::<Value>(&ljson) {
                queue_scoped_profile_op(&tx, remote_name, profile, "binge_lock", &skey, |hlc, _| {
                    json!({
                        "op": "binge_lock",
                        "hlc": hlc,
                        "key": skey,
                        "lock": lval,
                        "updated_at": uat,
                    })
                })?;
            }
        }
        for (mkey, pref) in track_prefs_seed {
            let canon_key = canonicalize_track_pref_key(&mkey);
            queue_scoped_profile_op(&tx, remote_name, profile, "track_pref", &canon_key, |hlc, _| {
                json!({
                    "op": "track_pref",
                    "hlc": hlc,
                    "key": canon_key,
                    "pref": {
                        "audio": pref.audio_lang,
                        "sub": pref.sub_lang,
                        "sub_off": pref.sub_off,
                    },
                    "updated_at": now_secs(),
                })
            })?;
        }
    }

    for m in &media_pages {
        let updated = m["updated"].as_i64().unwrap_or(0);
        if since == 0 && m["history"] != true {
            continue;
        }
        let kind = s(m, "type");
        let raw_id = s(m, "id");
        if kind.is_empty() || raw_id.is_empty() || kind == "other" {
            continue;
        }
        let mut meta = m["meta"].clone();
        let canon_id = extract_imdb_id(&raw_id, &meta).unwrap_or_else(|| raw_id.clone());
        scrub_loopback_urls(&mut meta);
        if let Some(obj) = meta.as_object_mut() {
            obj.remove("videos");
            obj.insert("id".into(), json!(canon_id));
            obj.insert("type".into(), json!(kind));
        }
        let canon_vid = m["video_id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(|v| canonical_video_id(&raw_id, &canon_id, v));
        let item_key = format!("{kind}|{canon_id}");
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT updated_hlc, payload_json FROM profile_kv_hlc
                 WHERE remote = ?1 AND profile = ?2 AND ns = 'media' AND item_key = ?3",
                params![remote_name, profile, item_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let mut prev_updated = 0i64;
        if let Some((_, ref existing_raw)) = existing {
            if let Ok(prev) = serde_json::from_str::<Value>(existing_raw) {
                let prev_item = &prev["item"];
                prev_updated = prev_item["updated"].as_i64().unwrap_or(0);
                if prev_updated >= updated
                    && prev_item["saved"] == m["saved"]
                    && prev_item["watched"] == m["watched"]
                    && prev_item["position_ms"] == m["position_ms"]
                    && prev_item["liked"] == m["liked"]
                    && prev_item["video_id"].as_str() == canon_vid.as_deref()
                    && prev_item["deleted"] != true
                {
                    continue;
                }
            }
        }
        let prev_hlc = existing.as_ref().map(|(h, _)| h.as_str());
        let (hlc, _, _) = next_hlc_after(&tx, prev_hlc)?;
        let out_updated = if prev_updated > 0 {
            updated.max(prev_updated + 1)
        } else {
            updated.max(1)
        };
        let ev = json!({
            "op": "media",
            "hlc": hlc,
            "key": item_key,
            "item": {
                "type": kind,
                "id": canon_id,
                "addon_url": safe_sync_addon_url(&canon_id),
                "meta": meta,
                "saved": m["saved"] == true,
                "watched": m["watched"] == true,
                "position_ms": m["position_ms"].as_i64().unwrap_or(0),
                "duration_ms": m["duration_ms"].as_i64().unwrap_or(0),
                "watched_ms": m["watched_ms"].as_i64().unwrap_or(0),
                "liked": m["liked"].as_i64().unwrap_or(0),
                "video_id": canon_vid,
                "updated": out_updated,
                "deleted": false,
            }
        });
        let ev_str = ev.to_string();
        tx.execute(
            "INSERT OR REPLACE INTO profile_kv_hlc(remote, profile, ns, item_key, updated_hlc, payload_json)
             VALUES(?1, ?2, 'media', ?3, ?4, ?5)",
            params![remote_name, profile, item_key, hlc, ev_str],
        )?;
        tx.execute(
            "INSERT INTO pending_profile_ops(remote, profile, op_key, event_json) VALUES(?1, ?2, ?3, ?4)",
            params![remote_name, profile, format!("media|{item_key}"), ev_str],
        )?;
    }

    for p in &playback_pages {
        let updated = p["updated"].as_i64().unwrap_or(0);
        let kind = s(p, "type");
        let raw_id = s(p, "id");
        let raw_vid = s(p, "video_id");
        if kind.is_empty() || raw_id.is_empty() || raw_vid.is_empty() || kind == "other" {
            continue;
        }
        let canon_id = extract_imdb_id(&raw_id, &Value::Null).unwrap_or_else(|| raw_id.clone());
        let canon_vid = canonical_video_id(&raw_id, &canon_id, &raw_vid);
        let item_key = format!("{kind}|{canon_id}|{canon_vid}");
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT updated_hlc, payload_json FROM profile_kv_hlc
                 WHERE remote = ?1 AND profile = ?2 AND ns = 'playback' AND item_key = ?3",
                params![remote_name, profile, item_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let mut prev_updated = 0i64;
        if let Some((_, ref existing_raw)) = existing {
            if let Ok(prev) = serde_json::from_str::<Value>(existing_raw) {
                let prev_item = &prev["item"];
                prev_updated = prev_item["updated"].as_i64().unwrap_or(0);
                if prev_updated >= updated
                    && prev_item["position_ms"] == p["position_ms"]
                    && prev_item["watched"] == p["watched"]
                {
                    continue;
                }
            }
        }
        let prev_hlc = existing.as_ref().map(|(h, _)| h.as_str());
        let (hlc, _, _) = next_hlc_after(&tx, prev_hlc)?;
        let out_updated = if prev_updated > 0 {
            updated.max(prev_updated + 1)
        } else {
            updated.max(1)
        };
        let ev = json!({
            "op": "playback",
            "hlc": hlc,
            "key": item_key,
            "item": {
                "type": kind,
                "id": canon_id,
                "addon_url": safe_sync_addon_url(&canon_id),
                "video_id": canon_vid,
                "position_ms": p["position_ms"].as_i64().unwrap_or(0),
                "duration_ms": p["duration_ms"].as_i64().unwrap_or(0),
                "watched_ms": p["watched_ms"].as_i64().unwrap_or(0),
                "watched": p["watched"] == true,
                "updated": out_updated,
            }
        });
        let ev_str = ev.to_string();
        tx.execute(
            "INSERT OR REPLACE INTO profile_kv_hlc(remote, profile, ns, item_key, updated_hlc, payload_json)
             VALUES(?1, ?2, 'playback', ?3, ?4, ?5)",
            params![remote_name, profile, item_key, hlc, ev_str],
        )?;
        tx.execute(
            "INSERT INTO pending_profile_ops(remote, profile, op_key, event_json) VALUES(?1, ?2, ?3, ?4)",
            params![remote_name, profile, format!("playback|{item_key}"), ev_str],
        )?;
    }

    if core.is_some() {
        let next_cursor = last_now.max(max_seen);
        tx.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES(?1, ?2)",
            params![cursor_key, next_cursor.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stage_profile_blob_in_tx(
    tx: &rusqlite::Transaction<'_>,
    remote_name: &str,
    profile: &str,
    rel_path: &str,
    bytes: &[u8],
    media_apply: &mut BTreeMap<String, Value>,
    playback_apply: &mut BTreeMap<String, Value>,
    track_prefs_apply: &mut BTreeMap<String, TrackPref>,
) -> Result<usize> {
    let text = String::from_utf8_lossy(bytes);
    let mut changed = 0usize;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let op = s(&v, "op");
        if op == "meta" {
            for c in v["covered"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                tx.execute(
                    "INSERT OR IGNORE INTO applied_profile_files(remote, profile, rel_path) VALUES(?1, ?2, ?3)",
                    params![remote_name, profile, c],
                )?;
            }
            continue;
        }
        let hlc = s(&v, "hlc");
        let item_key = s(&v, "key");
        if hlc.is_empty() || item_key.is_empty() || !matches!(op.as_str(), "media" | "playback" | "binge_lock" | "track_pref") {
            continue;
        }
        let cur_hlc: Option<String> = tx
            .query_row(
                "SELECT updated_hlc FROM profile_kv_hlc WHERE remote = ?1 AND profile = ?2 AND ns = ?3 AND item_key = ?4",
                params![remote_name, profile, op, item_key],
                |r| r.get(0),
            )
            .optional()?;
        if cur_hlc.as_deref().is_some_and(|cur| cur >= hlc.as_str()) {
            continue;
        }
        observe_hlc(tx, &hlc)?;
        tx.execute(
            "INSERT OR REPLACE INTO profile_kv_hlc(remote, profile, ns, item_key, updated_hlc, payload_json)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![remote_name, profile, op, item_key, hlc, line],
        )?;
        match op.as_str() {
            "media" => {
                if v["item"].is_object() {
                    media_apply.insert(item_key, v["item"].clone());
                    changed += 1;
                }
            }
            "playback" => {
                if v["item"].is_object() {
                    playback_apply.insert(item_key, v["item"].clone());
                    changed += 1;
                }
            }
            "binge_lock" => {
                if let Some(lock) = StreamMatcherLock::from_json(&v["lock"]) {
                    let uat = v["updated_at"].as_i64().unwrap_or_else(now_secs);
                    tx.execute(
                        "INSERT OR REPLACE INTO binge_locks(series_key, lock_json, updated_at) VALUES(?1, ?2, ?3)",
                        params![item_key, lock.to_json().to_string(), uat],
                    )?;
                    changed += 1;
                }
            }
            "track_pref" => {
                let p = &v["pref"];
                let pref = TrackPref {
                    audio_lang: p["audio"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
                    sub_lang: p["sub"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
                    sub_off: p["sub_off"].as_bool().unwrap_or(false),
                };
                track_prefs_apply.insert(item_key, pref);
                changed += 1;
            }
            _ => {}
        }
    }
    tx.execute(
        "INSERT OR IGNORE INTO applied_profile_files(remote, profile, rel_path) VALUES(?1, ?2, ?3)",
        params![remote_name, profile, rel_path],
    )?;
    Ok(changed)
}

pub fn sync_profile_remote(remote_name: &str, profile_raw: &str) -> Result<usize> {
    sync_profile_remote_with_core(remote_name, profile_raw, BACKEND.get().map(|b| &b.core))
}

pub(crate) fn sync_profile_remote_with_core(
    remote_name: &str,
    profile_raw: &str,
    core: Option<&Core>,
) -> Result<usize> {
    static PROFILE_LOCK: Mutex<()> = Mutex::new(());
    let _serial = PROFILE_LOCK.lock().map_err(|_| anyhow!("profile sync lock poisoned"))?;
    let _busy = SyncBusyGuard::enter();
    let profile = sanitize_profile_name(profile_raw);
    let prefix = format!("RStream/.profiles/{profile}");
    let remote_fs = normalize_fs(remote_name);
    let mut fetched_events: HashMap<String, Vec<u8>> = HashMap::new();

    // Phase 1: Capture local Core changes & flush coalesced pending profile ops to `<prefix>/events/<device>/<hlc>.jsonl`.
    {
        let mut conn = open_db()?;
        capture_local_core_changes(&mut conn, remote_name, &profile, core)?;
        let device = ensure_device_id(&conn)?;
        let mut stmt = conn.prepare(
            "SELECT seq, op_key, event_json FROM pending_profile_ops
             WHERE remote = ?1 AND profile = ?2 ORDER BY seq ASC",
        )?;
        let pending: Vec<(i64, String, String)> = stmt
            .query_map(params![remote_name, profile], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .flatten()
            .collect();
        drop(stmt);
        if !pending.is_empty() {
            let mut dedup: BTreeMap<String, String> = BTreeMap::new();
            for (_, op_key, ev_json) in &pending {
                dedup.insert(op_key.clone(), ev_json.clone());
            }
            let (batch_hlc, _, _) = next_hlc(&conn)?;
            let rel_path = format!("{prefix}/events/{device}/{batch_hlc}.jsonl");
            let mut payload = String::new();
            for line in dedup.values() {
                payload.push_str(line);
                payload.push('\n');
            }
            rstream_rclone::put_remote_bytes(&remote_fs, &rel_path, payload.as_bytes())?;
            conn.execute(
                "INSERT OR IGNORE INTO applied_profile_files(remote, profile, rel_path) VALUES(?1, ?2, ?3)",
                params![remote_name, profile, rel_path],
            )?;
            let max_seq = pending.last().map(|(s, _, _)| *s).unwrap_or(0);
            conn.execute(
                "DELETE FROM pending_profile_ops WHERE remote = ?1 AND profile = ?2 AND seq <= ?3",
                params![remote_name, profile, max_seq],
            )?;
            fetched_events.insert(rel_path, payload.into_bytes());
        }
    }

    // Phase 2: Pull remote snapshots, deltas, and peer event files from `<prefix>`.
    let mut conn = open_db()?;
    let mut applied: HashSet<String> = conn
        .prepare("SELECT rel_path FROM applied_profile_files WHERE remote = ?1 AND profile = ?2")?
        .query_map(params![remote_name, profile], |r| r.get::<_, String>(0))?
        .flatten()
        .collect();
    let tiers = fetch_unapplied_tiers(&remote_fs, &prefix, &mut applied, &mut fetched_events)?;

    let mut media_apply = BTreeMap::new();
    let mut playback_apply = BTreeMap::new();
    let mut track_prefs_apply = BTreeMap::new();
    let mut total_changed = 0usize;

    if !tiers.blobs.is_empty() {
        let tx = conn.transaction()?;
        for (rel, bytes) in &tiers.blobs {
            total_changed += stage_profile_blob_in_tx(
                &tx,
                remote_name,
                &profile,
                rel,
                bytes,
                &mut media_apply,
                &mut playback_apply,
                &mut track_prefs_apply,
            )?;
        }
        if !media_apply.is_empty() || !playback_apply.is_empty() {
            if let Some(core) = core {
                exec_core_blocking(
                    core,
                    json!({
                        "op": "sync_apply",
                        "media": media_apply.into_values().collect::<Vec<_>>(),
                        "playback": playback_apply.into_values().collect::<Vec<_>>(),
                    }),
                )?;
            }
        }
        // Defer committing `applied_profile_files` & `profile_kv_hlc` until AFTER `sync_apply` succeeds.
        tx.commit()?;
    }

    for (k, pref) in track_prefs_apply {
        store::apply_synced_track_pref(&k, pref);
    }

    // Phase 3: 2-tier compaction (`events -> delta`, `deltas -> base snapshot`).
    if tiers.events.len() >= EVENT_COMPACT_THRESHOLD {
        compact_profile_events_into_delta(&conn, remote_name, &profile, &prefix, &remote_fs, &tiers.events, &fetched_events)?;
    } else if tiers.deltas.len() >= DELTA_COMPACT_THRESHOLD {
        compact_profile_into_snapshot(&conn, remote_name, &profile, &prefix, &remote_fs, &tiers.snapshots, &tiers.deltas)?;
    }

    if total_changed > 0 {
        PROFILE_SYNC_EPOCH.fetch_add(1, Ordering::SeqCst);
    }
    Ok(total_changed)
}

fn compact_profile_events_into_delta(
    conn: &Connection,
    remote_name: &str,
    profile: &str,
    prefix: &str,
    remote_fs: &str,
    events: &[String],
    cached_events: &HashMap<String, Vec<u8>>,
) -> Result<()> {
    let (covered, lines) = collect_latest_events(remote_fs, events, cached_events, |v| {
        let (op, key) = (s(v, "op"), s(v, "key"));
        (!op.is_empty() && !key.is_empty() && op != "meta").then(|| format!("{op}|{key}"))
    })?;
    if covered.is_empty() { return Ok(()); }
    let (hlc, _, _) = next_hlc(conn)?;
    let delta_path = format!("{prefix}/deltas/delta-{hlc}.jsonl");
    write_compacted_blob_and_prune(remote_fs, &delta_path, &covered, lines, |p| {
        conn.execute("INSERT OR IGNORE INTO applied_profile_files(remote, profile, rel_path) VALUES(?1, ?2, ?3)", params![remote_name, profile, p])?;
        Ok(())
    })
}

fn compact_profile_into_snapshot(
    conn: &Connection,
    remote_name: &str,
    profile: &str,
    prefix: &str,
    remote_fs: &str,
    snapshots: &[String],
    deltas: &[String],
) -> Result<()> {
    let mut covered: Vec<String> = snapshots.iter().chain(deltas.iter()).cloned().collect();
    covered.sort();
    covered.dedup();
    let lines: Vec<String> = conn
        .prepare("SELECT payload_json FROM profile_kv_hlc WHERE remote = ?1 AND profile = ?2 ORDER BY updated_hlc ASC")?
        .query_map(params![remote_name, profile], |r| r.get::<_, String>(0))?
        .flatten()
        .collect();
    let (hlc, _, _) = next_hlc(conn)?;
    let snap_path = format!("{prefix}/snapshots/base-{hlc}.jsonl");
    write_compacted_blob_and_prune(remote_fs, &snap_path, &covered, lines, |p| {
        conn.execute("INSERT OR IGNORE INTO applied_profile_files(remote, profile, rel_path) VALUES(?1, ?2, ?3)", params![remote_name, profile, p])?;
        Ok(())
    })
}
