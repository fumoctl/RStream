use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Durable metadata budget per row. A resource response is capped at 8 MiB and
/// merged field-by-field, so repeated responses with new large fields could grow
/// one saved/history row without bound; that row is never evicted.
const META_BUDGET_BYTES: usize = 1 << 20;

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub(crate) fn open(path: &Path) -> Result<Connection> {
    std::fs::create_dir_all(path)?;
    let db_file = path.join("core.sqlite3");
    let mut db = Connection::open(&db_file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let p = path.join(format!("core.sqlite3{suffix}"));
            if p.exists() {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA foreign_keys=ON;")?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > 3 {
        bail!("Database was created by a newer core");
    }
    if version == 0 {
        let tx = db.transaction()?;
        tx.execute_batch("
          CREATE TABLE addons(url TEXT PRIMARY KEY, manifest TEXT NOT NULL, allow_http INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE cache(key TEXT PRIMARY KEY, body TEXT NOT NULL, expires INTEGER NOT NULL, updated INTEGER NOT NULL);
          CREATE TABLE media(
            key TEXT PRIMARY KEY, type TEXT NOT NULL, raw_id TEXT NOT NULL, addon_url TEXT NOT NULL,
            meta TEXT NOT NULL, saved INTEGER NOT NULL DEFAULT 0, watched INTEGER NOT NULL DEFAULT 0,
            position_ms INTEGER NOT NULL DEFAULT 0, duration_ms INTEGER NOT NULL DEFAULT 0,
            watched_ms INTEGER NOT NULL DEFAULT 0, liked INTEGER NOT NULL DEFAULT 0,
            video_id TEXT, updated INTEGER NOT NULL
          );
          CREATE TABLE playback(
            media_key TEXT NOT NULL REFERENCES media(key) ON DELETE CASCADE, video_id TEXT NOT NULL,
            position_ms INTEGER NOT NULL, duration_ms INTEGER NOT NULL, watched_ms INTEGER NOT NULL,
            updated INTEGER NOT NULL, PRIMARY KEY(media_key,video_id)
          );
          CREATE INDEX media_activity ON media(saved,watched,liked);
          PRAGMA user_version=1;
        ")?;
        for (url, manifest) in defaults() {
            tx.execute(
                "INSERT INTO addons(url,manifest) VALUES(?1,?2)",
                params![url, manifest.to_string()],
            )?;
        }
        tx.commit()?;
    }
    if version < 2 {
        let tx = db.transaction()?;
        tx.execute_batch(
            "
          ALTER TABLE playback ADD COLUMN watched INTEGER NOT NULL DEFAULT 0;
          UPDATE playback SET watched=1 WHERE duration_ms>0
            AND watched_ms*5>=duration_ms*4 AND position_ms*10>=duration_ms*9;
          ALTER TABLE addons ADD COLUMN generation TEXT NOT NULL DEFAULT '';
          UPDATE addons SET generation=lower(hex(randomblob(16)));
          PRAGMA user_version=2;
        ",
        )?;
        // v1 conflated manual title marks with automatic episode completion. Preserve
        // those title flags rather than silently discard an explicit user decision.
        tx.commit()?;
    }
    if version < 3 {
        // Before v3 the app marked a film or episode watched once playback passed 80%, without
        // measured time (watched_ms 0) and sometimes without the mark landing. Keep what it showed.
        let tx = db.transaction()?;
        tx.execute_batch(
            "
          UPDATE playback SET watched=1 WHERE watched=0 AND watched_ms=0
            AND duration_ms>0 AND position_ms*5>=duration_ms*4;
          UPDATE media SET watched=1 WHERE watched=0 AND type='movie' AND watched_ms=0
            AND duration_ms>0 AND position_ms*5>=duration_ms*4;
          -- Playback before v3 was never measured: count what was reached as watched, so a
          -- film half-seen then is completed by watching the rest now.
          UPDATE playback SET watched_ms=position_ms WHERE watched_ms=0 AND position_ms>0;
          UPDATE media SET watched_ms=(SELECT COALESCE(SUM(watched_ms),0) FROM playback WHERE media_key=media.key)
            WHERE watched_ms=0 AND EXISTS (SELECT 1 FROM playback WHERE media_key=media.key AND watched_ms>0);
          PRAGMA user_version=3;
        ",
        )?;
        tx.commit()?;
    }
    // Idempotent indexes for hot sync, lookup, and cache eviction queries.
    db.execute_batch(
        "CREATE INDEX IF NOT EXISTS media_updated ON media(updated DESC, key);
         CREATE INDEX IF NOT EXISTS playback_updated ON playback(updated);
         CREATE INDEX IF NOT EXISTS media_lookup ON media(type, raw_id);
         CREATE INDEX IF NOT EXISTS cache_updated ON cache(updated DESC, key);",
    )?;
    Ok(db)
}

fn defaults() -> Vec<(&'static str, Value)> {
    vec![
        (
            "https://v3-cinemeta.strem.io/manifest.json",
            json!({
                "id":"com.linvo.cinemeta","name":"Cinemeta","version":"3.0.0",
                "description":"Public movie and series metadata", "types":["movie","series"],
                "resources":["catalog",{"name":"meta","types":["movie","series"],"idPrefixes":["tt"]}],
                "catalogs":[
                    {"type":"movie","id":"top","name":"Movies","extra":[{"name":"search","isRequired":false},{"name":"skip","isRequired":false}]},
                    {"type":"series","id":"top","name":"Series","extra":[{"name":"search","isRequired":false},{"name":"skip","isRequired":false}]}
                ], "bootstrap":true
            }),
        ),
        (
            "https://anime-kitsu.strem.fun/manifest.json",
            json!({
                "id":"com.stremio.kitsu","name":"Kitsu","version":"1.0.0",
                "description":"Anime metadata with distinct Kitsu identities", "types":["anime","movie","series"],
                "resources":["catalog",{"name":"meta","types":["anime","movie","series"],"idPrefixes":["kitsu:"]}],
                "catalogs":[{"type":"anime","id":"kitsu-anime-trending","name":"Anime","extra":[{"name":"search","isRequired":false},{"name":"skip","isRequired":false}]}],
                "bootstrap":true
            }),
        ),
        (
            "https://opensubtitles-v3.strem.io/manifest.json",
            json!({
                "id":"org.stremio.opensubtitlesv3","name":"OpenSubtitles v3","version":"1.0.0",
                "description":"Official OpenSubtitles subtitle add-on","types":["movie","series"],
                "resources":["subtitles"],"catalogs":[],"bootstrap":true
            }),
        ),
    ]
}

/// Only established namespace formats cross addon boundaries. No anime/IMDb inference.
pub(crate) fn identity(kind: &str, id: &str, addon: &str) -> String {
    let head = id.split(':').next().unwrap_or(id);
    let imdb = head
        .strip_prefix("tt")
        .is_some_and(|x| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit()));
    let kitsu = id
        .strip_prefix("kitsu:")
        .and_then(|s| s.split(':').next())
        .is_some_and(|x| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit()));
    json!([kind, id, if imdb || kitsu { "" } else { addon }]).to_string()
}

#[derive(Clone, Debug)]
pub(crate) struct Media {
    pub kind: String,
    pub id: String,
    pub addon: String,
    pub meta: Value,
    pub saved: bool,
    pub watched: bool,
    pub video_watched: bool,
    pub position: i64,
    pub duration: i64,
    pub watched_ms: i64,
    pub liked: i64,
    pub video_id: Option<String>,
}
impl Media {
    pub fn value(&self) -> Value {
        let mut v = if self.meta.is_object() {
            self.meta.clone()
        } else {
            json!({})
        };
        let o = v.as_object_mut().unwrap();
        o.insert("id".into(), json!(self.id));
        o.insert("type".into(), json!(self.kind));
        o.insert("addon_url".into(), json!(self.addon));
        o.insert("library".into(), json!(self.saved));
        o.insert("watched".into(), json!(self.watched));
        o.insert("video_watched".into(), json!(self.video_watched));
        o.insert("position_ms".into(), json!(self.position));
        o.insert("duration_ms".into(), json!(self.duration));
        o.insert("watched_ms".into(), json!(self.watched_ms));
        o.insert("liked".into(), json!(self.liked));
        if let Some(id) = &self.video_id {
            o.insert("video_id".into(), json!(id));
        }
        v
    }
}
// Keep every media projection on the same mapper, including per-video completion.
const MEDIA_SELECT: &str = "SELECT type,raw_id,addon_url,meta,saved,watched,position_ms,duration_ms,watched_ms,liked,video_id,COALESCE((SELECT watched FROM playback WHERE media_key=media.key AND video_id=media.video_id),0) FROM media";

fn media_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Media> {
    Ok(Media {
        kind: r.get(0)?,
        id: r.get(1)?,
        addon: r.get(2)?,
        meta: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(json!({})),
        saved: r.get(4)?,
        watched: r.get(5)?,
        position: r.get(6)?,
        duration: r.get(7)?,
        watched_ms: r.get(8)?,
        liked: r.get(9)?,
        video_id: r.get(10)?,
        video_watched: r.get(11)?,
    })
}

fn media_list(db: &Connection, filter: &str) -> Result<Vec<Media>> {
    let mut stmt =
        db.prepare_cached(&format!("{MEDIA_SELECT}{filter} ORDER BY updated DESC,key"))?;
    let rows = stmt.query_map([], media_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn library(db: &Connection) -> Result<Vec<Media>> {
    // Match Media::in_library without loading/parsing inactive catalog metadata.
    media_list(
        db,
        " WHERE saved<>0 OR watched<>0 OR position_ms>0 OR liked<>0 OR watched_ms>0",
    )
}

pub(crate) fn media_item(db: &Connection, key: &str) -> Result<Option<Media>> {
    // media.key is already a primary key; no sort, scan, or schema migration needed.
    Ok(db
        .prepare_cached(&format!("{MEDIA_SELECT} WHERE key=?1"))?
        .query_row([key], media_row)
        .optional()?)
}
pub(crate) fn upsert_meta(
    db: &Connection,
    kind: &str,
    id: &str,
    addon: &str,
    value: &Value,
) -> Result<String> {
    let key = identity(kind, id, addon);
    if value.as_object().is_some_and(|object| object.is_empty()) {
        // Empty metadata is a merge of no fields, NOT a request to clear metadata
        // or replace its source. Avoid reading/rewriting the old JSON on progress.
        db.execute(
            "INSERT INTO media(key,type,raw_id,addon_url,meta,updated) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(key) DO NOTHING",
            params![
                key,
                kind,
                id,
                addon,
                json!({"id":id,"type":kind}).to_string(),
                now()
            ],
        )?;
        return Ok(key);
    }
    let old: Option<(String, String)> = db
        .query_row(
            "SELECT meta,addon_url FROM media WHERE key=?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let replace_source = old.as_ref().is_some_and(|(_, source)| source != addon)
        && value
            .as_object()
            .is_some_and(|object| object.keys().any(|key| key != "id" && key != "type"));
    let mut merged = old
        .and_then(|(s, _)| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object)
        .unwrap_or(json!({}));
    if replace_source {
        // Explicit relationships belong to their supplying addon, not an ID alone.
        for field in ["collection", "belongsToCollection", "franchise"] {
            merged.as_object_mut().unwrap().remove(field);
        }
    }
    let mut size = serde_json::to_vec(&merged)?.len();
    if let Some(new) = value.as_object() {
        for (k, v) in new {
            if v.is_null() {
                continue;
            }
            // Bound the durable row: keep the fields that fit, drop the rest.
            let new_bytes = serde_json::to_vec(v)?.len();
            let next_size = if let Some(previous) = merged.get(k) {
                // A replacement changes the value cost, not the key or separators.
                size.saturating_sub(serde_json::to_vec(previous)?.len())
                    .saturating_add(new_bytes)
            } else {
                size.saturating_add(
                    serde_json::to_vec(k)?.len()
                        + 1
                        + new_bytes
                        + usize::from(!merged.as_object().unwrap().is_empty()),
                )
            };
            if next_size > META_BUDGET_BYTES {
                continue;
            }
            size = next_size;
            merged[k] = v.clone();
        }
    }
    strip_streams(&mut merged);
    merged["id"] = json!(id);
    merged["type"] = json!(kind);
    db.execute(
        "INSERT INTO media(key,type,raw_id,addon_url,meta,updated) VALUES(?1,?2,?3,?4,?5,?6)
        ON CONFLICT(key) DO UPDATE SET meta=excluded.meta, addon_url=CASE WHEN ?7 THEN excluded.addon_url ELSE media.addon_url END,
        -- Recency for cache eviction. Rows with history keep their activity time.
        updated=CASE WHEN media.saved=0 AND media.watched=0 AND media.position_ms=0
          AND media.watched_ms=0 AND media.liked=0 THEN excluded.updated ELSE media.updated END",
        params![key, kind, id, addon, merged.to_string(), now(),replace_source],
    )?;
    Ok(key)
}
/// Sync between devices: titles changed after `since` (seconds) and the per-video records changed
/// after it. A title with no history left (unsaved, unrated, its progress gone) is listed too,
/// flagged, without its metadata: the caller sends it only where the other side knows it.
pub(crate) fn sync_changes(db: &Connection, since: i64) -> Result<Value> {
    let mut stmt = db.prepare_cached(
        "SELECT key,type,raw_id,addon_url,meta,saved,watched,position_ms,duration_ms,watched_ms,liked,video_id,updated,
           (saved<>0 OR watched<>0 OR position_ms>0 OR liked<>0 OR watched_ms>0)
         FROM media WHERE updated>?1 ORDER BY updated,key LIMIT 5000",
    )?;
    let media = stmt
        .query_map([since], |r| {
            let history: bool = r.get(13)?;
            let meta = if history { serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or(json!({})) } else { json!({}) };
            Ok(json!({"key":r.get::<_,String>(0)?,"type":r.get::<_,String>(1)?,"id":r.get::<_,String>(2)?,"addon_url":r.get::<_,String>(3)?,
                "meta":meta,"saved":r.get::<_,bool>(5)?,"watched":r.get::<_,bool>(6)?,"position_ms":r.get::<_,i64>(7)?,
                "duration_ms":r.get::<_,i64>(8)?,"watched_ms":r.get::<_,i64>(9)?,"liked":r.get::<_,i64>(10)?,
                "video_id":r.get::<_,Option<String>>(11)?,"updated":r.get::<_,i64>(12)?,"history":history}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stmt = db.prepare_cached(
        "SELECT p.media_key,m.type,m.raw_id,m.addon_url,p.video_id,p.position_ms,p.duration_ms,p.watched_ms,p.watched,p.updated
         FROM playback p JOIN media m ON m.key=p.media_key WHERE p.updated>?1 ORDER BY p.updated LIMIT 20000",
    )?;
    let playback = stmt
        .query_map([since], |r| {
            Ok(json!({"key":r.get::<_,String>(0)?,"type":r.get::<_,String>(1)?,"id":r.get::<_,String>(2)?,"addon_url":r.get::<_,String>(3)?,
                "video_id":r.get::<_,String>(4)?,"position_ms":r.get::<_,i64>(5)?,"duration_ms":r.get::<_,i64>(6)?,
                "watched_ms":r.get::<_,i64>(7)?,"watched":r.get::<_,bool>(8)?,"updated":r.get::<_,i64>(9)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let max_m = media.last().and_then(|m| m["updated"].as_i64()).unwrap_or(0);
    let max_p = playback.last().and_then(|p| p["updated"].as_i64()).unwrap_or(0);
    Ok(json!({"media":media,"playback":playback,"now":now().max(max_m).max(max_p)}))
}

/// Another device's changes (as `sync_changes` lists them, `deleted` for a title it forgot): each
/// title and per-video record replaces this one's only when it is newer, and keeps its own time
/// so it isn't listed again as a change of this device's. A title known here only as catalogue
/// metadata (no history) always takes the other device's history.
pub(crate) fn sync_apply(db: &mut Connection, request: &Value) -> Result<Value> {
    let tx = db.transaction()?;
    let mut applied = 0;
    let mut deleted = std::collections::HashMap::<String, i64>::new();
    let field = |v: &Value, k: &str| {
        v[k].as_str()
            .filter(|s| !s.is_empty() && s.len() <= 2048)
            .map(str::to_owned)
    };
    for m in request["media"].as_array().into_iter().flatten() {
        let (Some(kind), Some(id), Some(addon)) = (
            field(m, "type"),
            field(m, "id"),
            m["addon_url"].as_str().map(str::to_owned),
        ) else {
            continue;
        };
        let Some(updated) = m["updated"].as_i64() else {
            continue;
        };
        let key = identity(&kind, &id, &addon);
        let local: Option<(i64, bool)> = tx
            .query_row(
                "SELECT updated,(saved<>0 OR watched<>0 OR position_ms>0 OR liked<>0 OR watched_ms>0) FROM media WHERE key=?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if local.is_some_and(|(at, history)| history && at >= updated) {
            continue;
        }
        if m["deleted"] == true {
            deleted
                .entry(key.clone())
                .and_modify(|prev| *prev = (*prev).max(updated))
                .or_insert(updated);
            tx.execute("DELETE FROM playback WHERE media_key=?1", [&key])?;
            tx.execute("DELETE FROM media WHERE key=?1", [&key])?;
            applied += 1;
            continue;
        }
        let meta = if m["meta"].is_object() {
            m["meta"].clone()
        } else {
            json!({})
        };
        upsert_meta(&tx, &kind, &id, &addon, &meta)?;
        tx.execute(
            "UPDATE media SET saved=?2,watched=?3,position_ms=?4,duration_ms=?5,watched_ms=?6,liked=?7,video_id=?8,updated=?9 WHERE key=?1",
            params![key, m["saved"] == true, m["watched"] == true, m["position_ms"].as_i64().unwrap_or(0).max(0),
                m["duration_ms"].as_i64().unwrap_or(0).max(0), m["watched_ms"].as_i64().unwrap_or(0).max(0),
                m["liked"].as_i64().unwrap_or(0).clamp(-1, 1), m["video_id"].as_str(), updated],
        )?;
        applied += 1;
    }
    for p in request["playback"].as_array().into_iter().flatten() {
        let (Some(kind), Some(id), Some(addon), Some(video)) = (
            field(p, "type"),
            field(p, "id"),
            p["addon_url"].as_str().map(str::to_owned),
            field(p, "video_id"),
        ) else {
            continue;
        };
        let Some(updated) = p["updated"].as_i64() else {
            continue;
        };
        let key = identity(&kind, &id, &addon);
        if deleted.get(&key).is_some_and(|&del_at| updated <= del_at) {
            continue;
        }
        upsert_meta(&tx, &kind, &id, &addon, &json!({}))?;
        applied += tx.execute(
            "INSERT INTO playback(media_key,video_id,position_ms,duration_ms,watched_ms,watched,updated) VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(media_key,video_id) DO UPDATE SET position_ms=excluded.position_ms,duration_ms=excluded.duration_ms,
               watched_ms=excluded.watched_ms,watched=excluded.watched,updated=excluded.updated WHERE playback.updated<excluded.updated",
            params![key, video, p["position_ms"].as_i64().unwrap_or(0).max(0), p["duration_ms"].as_i64().unwrap_or(0).max(0),
                p["watched_ms"].as_i64().unwrap_or(0).max(0), p["watched"] == true, updated],
        )?;
    }
    tx.commit()?;
    Ok(json!({"applied":applied}))
}
pub(crate) fn cache_metas(
    db: &mut Connection,
    body: &Value,
    kind: &str,
    addon: &str,
) -> Result<()> {
    let tx = db.transaction()?;
    let listed = body
        .get("metas")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|metas| metas.iter().take(2000));
    let single = body.get("meta").filter(|m| m.is_object());
    for meta in listed.chain(single) {
        if let Some(id) = meta.get("id").and_then(Value::as_str) {
            let kind = meta.get("type").and_then(Value::as_str).unwrap_or(kind);
            upsert_meta(&tx, kind, id, addon, meta)?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Inline video streams may contain bearer URLs; only non-stream metadata reaches disk.
pub(crate) fn strip_streams(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("streams");
            for value in object.values_mut() {
                strip_streams(value);
            }
        }
        Value::Array(array) => {
            for value in array {
                strip_streams(value);
            }
        }
        _ => {}
    }
}

pub(crate) fn contains_streams(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key("streams") || object.values().any(contains_streams)
        }
        Value::Array(array) => array.iter().any(contains_streams),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_merge_refreshes_eviction_time_only_without_history() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/test-data/db-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = open(&dir).unwrap();
        let addon = "https://v3-cinemeta.strem.io/manifest.json";
        let seen = upsert_meta(&db, "movie", "tt1", addon, &json!({"name":"A"})).unwrap();
        let saved = upsert_meta(&db, "movie", "tt2", addon, &json!({"name":"A"})).unwrap();
        db.execute("UPDATE media SET saved=1 WHERE key=?1", [&saved])
            .unwrap();
        db.execute("UPDATE media SET updated=1", []).unwrap();
        for id in ["tt1", "tt2"] {
            upsert_meta(&db, "movie", id, addon, &json!({"name":"B"})).unwrap();
        }
        let updated = |key: &str| -> i64 {
            db.query_row("SELECT updated FROM media WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert!(
            updated(&seen) > 1,
            "catalog row must count as recently seen"
        );
        assert_eq!(updated(&saved), 1, "history order must not change");
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bootstrap_defaults_include_cinemeta_kitsu_and_opensubtitles_only() {
        let list = defaults();
        let urls: Vec<&str> = list.iter().map(|(u, _)| *u).collect();
        assert_eq!(
            urls,
            [
                "https://v3-cinemeta.strem.io/manifest.json",
                "https://anime-kitsu.strem.fun/manifest.json",
                "https://opensubtitles-v3.strem.io/manifest.json",
            ]
        );
        assert_eq!(list[2].1["id"], "org.stremio.opensubtitlesv3");
        assert_eq!(list[2].1["resources"], json!(["subtitles"]));
    }
}
