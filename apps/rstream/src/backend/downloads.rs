//! Offline saves: stream HTTP or BitTorrent sources either into the app's private `downloads/`
//! folder or onto an rclone remote (`SaveTarget::Remote`, including encrypted `crypt:` remotes),
//! with persistent queue state (`downloads-queue.json`, `0600`) and full Pause / Resume support.
use super::*;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveTarget {
    Local,
    Remote(String),
}

impl SaveTarget {
    pub fn label(&self) -> String {
        match self {
            SaveTarget::Local => "Local device".into(),
            SaveTarget::Remote(name) => format!("Rclone ({name})"),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            SaveTarget::Local => json!({"kind": "local"}),
            SaveTarget::Remote(name) => json!({"kind": "remote", "name": name}),
        }
    }

    fn from_json(v: &Value) -> Self {
        match v["kind"].as_str() {
            Some("remote") => {
                let name = s(v, "name");
                if name.is_empty() { SaveTarget::Local } else { SaveTarget::Remote(name) }
            }
            _ => SaveTarget::Local,
        }
    }
}

pub fn default_save_target() -> SaveTarget {
    match load_settings().default_save_target {
        Some(r) if !r.trim().is_empty() => SaveTarget::Remote(r),
        _ => SaveTarget::Local,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DownloadEntry {
    pub id: String,
    pub key: String,
    pub media: Media,
    pub video_id: String,
    pub title: String,
    pub subtitle: String,
    pub path: PathBuf,
    pub remote: Option<String>,
    pub bytes: u64,
    pub downloaded_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadState {
    Queued,
    Running,
    Paused,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct DownloadProgress {
    pub key: String,
    pub title: String,
    pub target: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub state: DownloadState,
}

#[derive(Clone)]
struct JobSpec {
    key: String,
    media: Media,
    video_id: String,
    episode_hint: Option<String>,
    title: String,
    subtitle: String,
    source: Option<Source>,
    matcher_lock: Option<StreamMatcherLock>,
    target: SaveTarget,
}

struct ActiveJob {
    spec: JobSpec,
    progress: DownloadProgress,
    attempt_id: u64,
    stop_flag: Arc<AtomicBool>,
    stop_token: CancellationToken,
    keep_partial: Arc<AtomicBool>,
}

#[derive(Debug)]
enum StopReason {
    Paused,
    Cancelled,
    Failed(anyhow::Error),
}

static DOWNLOADS_LOCK: Mutex<()> = Mutex::new(());
static ACTIVE_DOWNLOADS: Mutex<Option<HashMap<String, ActiveJob>>> = Mutex::new(None);
static MAX_PARALLEL_DOWNLOADS: AtomicUsize = AtomicUsize::new(3);
static ACTIVE_DOWNLOAD_SLOTS: AtomicUsize = AtomicUsize::new(0);
static SLOT_NOTIFY: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();
static RCLONE_UPLOAD_SEMAPHORE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
static LAST_ACTIVITY_PUSH_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static TEST_DOWNLOADS_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

fn slot_notify() -> Arc<tokio::sync::Notify> {
    SLOT_NOTIFY.get_or_init(|| Arc::new(tokio::sync::Notify::new())).clone()
}

fn rclone_upload_semaphore() -> Arc<tokio::sync::Semaphore> {
    RCLONE_UPLOAD_SEMAPHORE
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(3)))
        .clone()
}

pub(crate) fn set_parallel_downloads_limit(limit: usize) {
    MAX_PARALLEL_DOWNLOADS.store(limit.clamp(1, 6), Ordering::SeqCst);
    slot_notify().notify_waiters();
}

struct DownloadSlotGuard;

impl Drop for DownloadSlotGuard {
    fn drop(&mut self) {
        ACTIVE_DOWNLOAD_SLOTS.fetch_sub(1, Ordering::SeqCst);
        slot_notify().notify_waiters();
    }
}

async fn acquire_download_slot(
    stop_token: &CancellationToken,
    keep_partial: &AtomicBool,
) -> std::result::Result<DownloadSlotGuard, StopReason> {
    let notify = slot_notify();
    loop {
        if stop_token.is_cancelled() {
            return Err(classify_stop(keep_partial));
        }
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let max = MAX_PARALLEL_DOWNLOADS.load(Ordering::SeqCst).clamp(1, 6);
        let cur = ACTIVE_DOWNLOAD_SLOTS.load(Ordering::SeqCst);
        if cur < max {
            if ACTIVE_DOWNLOAD_SLOTS
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(DownloadSlotGuard);
            }
            continue;
        }
        tokio::select! {
            biased;
            _ = stop_token.cancelled() => return Err(classify_stop(keep_partial)),
            _ = &mut notified => {}
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackgroundActivity {
    pub running_downloads: usize,
    pub queued_downloads: usize,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub primary_title: Option<String>,
    pub db_syncing: bool,
}

pub fn background_activity() -> BackgroundActivity {
    let mut act = BackgroundActivity {
        db_syncing: remote_db::is_sync_busy(),
        ..Default::default()
    };
    if let Ok(g) = ACTIVE_DOWNLOADS.lock() {
        if let Some(map) = g.as_ref() {
            let mut running_titles = Vec::new();
            for job in map.values() {
                match job.progress.state {
                    DownloadState::Running => {
                        act.running_downloads += 1;
                        act.downloaded_bytes = act.downloaded_bytes.saturating_add(job.progress.downloaded);
                        if let Some(t) = job.progress.total {
                            act.total_bytes = act.total_bytes.saturating_add(t);
                        }
                        running_titles.push(job.progress.title.clone());
                    }
                    DownloadState::Queued => {
                        act.queued_downloads += 1;
                        if let Some(t) = job.progress.total {
                            act.total_bytes = act.total_bytes.saturating_add(t);
                        }
                    }
                    _ => {}
                }
            }
            running_titles.sort();
            act.primary_title = running_titles.into_iter().next();
        }
    }
    act
}

pub(crate) fn notify_background_activity(force: bool) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if !force {
        let prev = LAST_ACTIVITY_PUSH_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < 400 {
            return;
        }
    }
    LAST_ACTIVITY_PUSH_MS.store(now_ms, Ordering::Relaxed);
    #[cfg(target_os = "android")]
    {
        let act = background_activity();
        crate::android::sync_transfer_service(&act);
    }
}

#[cfg(test)]
pub fn download_key(media_key: &str, video_id: &str) -> String {
    let mut parts = media_key.splitn(3, '|');
    let kind = parts.next().unwrap_or("movie");
    let raw_id = parts.next().unwrap_or(media_key);
    remote_db::canonical_download_key_parts(kind, raw_id, &Value::Null, video_id).0
}

pub fn download_key_for_media(media: &Media, video_id: &str) -> String {
    remote_db::canonical_download_key_parts(&media.kind, &media.id, &media.raw, video_id).0
}

pub(super) fn data_root_for_db() -> PathBuf {
    data_root_dir()
}

fn data_root_dir() -> PathBuf {
    #[cfg(test)]
    if let Ok(g) = TEST_DOWNLOADS_ROOT.lock() {
        if let Some(root) = g.as_ref() {
            return root.clone();
        }
    }
    BACKEND.get().map(|b| b.data_dir.clone()).unwrap_or_else(data_dir)
}

fn downloads_dir() -> PathBuf { data_root_dir().join("downloads") }
fn downloads_queue_path() -> PathBuf { data_root_dir().join("downloads-queue.json") }

fn spawn_on_backend<F>(fut: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    if let Some(rt) = RUNTIME.get() {
        rt.spawn(fut);
        return Ok(());
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
        return Ok(());
    }
    Err(anyhow!("Backend not started"))
}

const VIDEO_EXTS: [&str; 7] = ["mkv", "mp4", "webm", "m4v", "mov", "avi", "ts"];

fn key_hash_hex(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(key.as_bytes());
    hash.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

fn safe_filename(key: &str, hint: &str) -> String {
    let hex = key_hash_hex(key);
    let path_part = hint.split(['?', '#']).next().unwrap_or("");
    let ext = path_part.rsplit('.').next()
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| VIDEO_EXTS.contains(&e.as_str()))
        .unwrap_or_else(|| "mkv".into());
    format!("{hex}.{ext}")
}

fn hint_for_source(src: &Source) -> String {
    if !src.filename.is_empty() { src.filename.clone() } else { src.url.clone() }
}

fn partial_paths_for_job(dir: &std::path::Path, key: &str) -> Vec<PathBuf> {
    let hex = key_hash_hex(key);
    let mut out = Vec::with_capacity(1 + VIDEO_EXTS.len() * 3);
    out.push(dir.join(format!("{hex}.part")));
    for ext in VIDEO_EXTS {
        let base = format!("{hex}.{ext}");
        out.push(dir.join(format!("stage-{base}")));
        out.push(dir.join(format!("stage-{base}.part")));
        out.push(dir.join(format!("tmp-{base}")));
    }
    out
}

fn cleanup_job_partials(dir: &std::path::Path, key: &str) {
    for p in partial_paths_for_job(dir, key) {
        let _ = std::fs::remove_file(p);
    }
    if let Some(b) = BACKEND.get() {
        let _ = b.download_torrent.cleanup_resumable(key);
    } else {
        let _ = stremio_torrent::TorrentEngine::new(data_root_dir().join("torrent-downloads"))
            .cleanup_resumable(key);
    }
}

fn remote_object_path(media: &Media, video_id: &str, title: &str, hint: &str) -> String {
    remote_db::human_remote_object_path(media, video_id, title, hint)
}

fn serialize_queue_locked(map: &HashMap<String, ActiveJob>) -> Vec<Value> {
    map.values().map(|job| {
        let state_val = match &job.progress.state {
            DownloadState::Queued | DownloadState::Running | DownloadState::Paused => json!({"kind": "paused"}),
            DownloadState::Failed(err) => json!({"kind": "failed", "error": err}),
        };
        json!({
            "key": job.spec.key,
            "media": media_json(&job.spec.media),
            "video_id": job.spec.video_id,
            "episode_hint": job.spec.episode_hint,
            "title": job.spec.title,
            "subtitle": job.spec.subtitle,
            "source": job.spec.source.as_ref().map(Source::to_json),
            "matcher_lock": job.spec.matcher_lock.as_ref().map(StreamMatcherLock::to_json),
            "target": job.spec.target.to_json(),
            "downloaded": job.progress.downloaded,
            "total": job.progress.total,
            "state": state_val,
        })
    }).collect()
}

fn persist_queue_snapshot(items: Vec<Value>) {
    let path = downloads_queue_path();
    let _ = write_private_file(&path, Value::Array(items).to_string().as_bytes());
}

pub(crate) fn load_persisted_downloads_queue() {
    let path = downloads_queue_path();
    let Ok(raw) = std::fs::read_to_string(&path) else { return };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else { return };
    let Some(arr) = v.as_array() else { return };
    let dir = downloads_dir();
    let Ok(mut g) = ACTIVE_DOWNLOADS.lock() else { return };
    let map = g.get_or_insert_with(HashMap::new);
    for item in arr {
        let key = s(item, "key");
        if key.is_empty() || map.contains_key(&key) { continue; }
        let Some(media) = Media::from_json(&item["media"], "") else { continue };
        let source = item.get("source").filter(|x| !x.is_null()).and_then(Source::from_json);
        let matcher_lock = item.get("matcher_lock").filter(|x| !x.is_null()).and_then(StreamMatcherLock::from_json);
        if source.is_none() && matcher_lock.is_none() { continue; }
        let target = SaveTarget::from_json(&item["target"]);
        let title = s(item, "title");
        let disk_bytes = partial_paths_for_job(&dir, &key)
            .into_iter()
            .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .max()
            .unwrap_or(0);
        let downloaded = item["downloaded"].as_u64().unwrap_or(0).max(disk_bytes);
        let state = match item["state"]["kind"].as_str() {
            Some("failed") => DownloadState::Failed(s(&item["state"], "error")),
            _ => DownloadState::Paused,
        };
        let spec = JobSpec {
            key: key.clone(),
            media,
            video_id: s(item, "video_id"),
            episode_hint: item["episode_hint"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            title: title.clone(),
            subtitle: s(item, "subtitle"),
            source,
            matcher_lock,
            target: target.clone(),
        };
        map.insert(key.clone(), ActiveJob {
            spec,
            progress: DownloadProgress {
                key,
                title,
                target: target.label(),
                downloaded,
                total: item["total"].as_u64(),
                state,
            },
            attempt_id: 0,
            stop_flag: Arc::new(AtomicBool::new(false)),
            stop_token: CancellationToken::new(),
            keep_partial: Arc::new(AtomicBool::new(true)),
        });
    }
}

pub fn list_downloads() -> Vec<DownloadEntry> {
    let _g = DOWNLOADS_LOCK.lock();
    remote_db::list_all_downloads()
}

#[cfg(test)]
pub fn find_download(media_key: &str, video_id: &str) -> Option<DownloadEntry> {
    let _g = DOWNLOADS_LOCK.lock();
    let want = download_key(media_key, video_id);
    remote_db::find_best_download(&want)
}

pub async fn download_stream_url(entry: &DownloadEntry) -> Result<String> {
    resolve_saved_path_url(&entry.path, entry.remote.as_deref()).await
}

pub async fn saved_source_stream_url(saved: &SavedSource) -> Result<String> {
    resolve_saved_path_url(&saved.path, saved.remote.as_deref()).await
}

async fn resolve_saved_path_url(path: &std::path::Path, remote: Option<&str>) -> Result<String> {
    match remote {
        None => Ok(path.to_string_lossy().into_owned()),
        Some(remote) => {
            let remote_fs = normalize_remote_fs(remote);
            let remote_path = path.to_string_lossy().into_owned();
            on_runtime(async move {
                tokio::task::spawn_blocking(move || rstream_rclone::file_stream_url(&remote_fs, &remote_path))
                    .await
                    .context("rclone worker failed")?
            }).await?
        }
    }
}

pub fn active_downloads() -> Vec<DownloadProgress> {
    let mut list: Vec<DownloadProgress> = ACTIVE_DOWNLOADS.lock().ok()
        .and_then(|g| g.as_ref().map(|m| m.values().map(|j| j.progress.clone()).collect()))
        .unwrap_or_default();
    list.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.key.cmp(&b.key)));
    list
}

pub fn pause_download(key: &str) {
    let mut snapshot = None;
    if let Ok(mut g) = ACTIVE_DOWNLOADS.lock() {
        if let Some(job) = g.as_mut().and_then(|m| m.get_mut(key)) {
            if matches!(job.progress.state, DownloadState::Running | DownloadState::Queued) {
                job.keep_partial.store(true, Ordering::SeqCst);
                job.stop_flag.store(true, Ordering::SeqCst);
                job.stop_token.cancel();
                job.attempt_id = job.attempt_id.wrapping_add(1);
                job.progress.state = DownloadState::Paused;
            }
        }
        if let Some(map) = g.as_ref() {
            snapshot = Some(serialize_queue_locked(map));
        }
    }
    if let Some(items) = snapshot {
        persist_queue_snapshot(items);
    }
    notify_background_activity(true);
}

pub fn pause_all_downloads() {
    let mut snapshot = None;
    if let Ok(mut g) = ACTIVE_DOWNLOADS.lock() {
        if let Some(map) = g.as_mut() {
            for job in map.values_mut() {
                if matches!(job.progress.state, DownloadState::Running | DownloadState::Queued) {
                    job.keep_partial.store(true, Ordering::SeqCst);
                    job.stop_flag.store(true, Ordering::SeqCst);
                    job.stop_token.cancel();
                    job.attempt_id = job.attempt_id.wrapping_add(1);
                    job.progress.state = DownloadState::Paused;
                }
            }
            snapshot = Some(serialize_queue_locked(map));
        }
    }
    if let Some(items) = snapshot {
        persist_queue_snapshot(items);
    }
    notify_background_activity(true);
}

pub fn resume_download(key: &str) -> Result<()> {
    let (spec, attempt_id, stop_flag, stop_token, keep_partial, snapshot) = {
        let mut g = ACTIVE_DOWNLOADS.lock().map_err(|_| anyhow!("download state busy"))?;
        let map = g.as_mut().context("Download job not found")?;
        let job = map.get_mut(key).context("Download job not found")?;
        if matches!(job.progress.state, DownloadState::Running | DownloadState::Queued) {
            return Ok(());
        }
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_token = CancellationToken::new();
        let keep_partial = Arc::new(AtomicBool::new(false));
        job.attempt_id = job.attempt_id.wrapping_add(1);
        job.stop_flag = stop_flag.clone();
        job.stop_token = stop_token.clone();
        job.keep_partial = keep_partial.clone();
        job.progress.state = DownloadState::Queued;
        let spec = job.spec.clone();
        let attempt_id = job.attempt_id;
        let snapshot = serialize_queue_locked(map);
        (spec, attempt_id, stop_flag, stop_token, keep_partial, snapshot)
    };
    persist_queue_snapshot(snapshot);
    notify_background_activity(true);
    spawn_job_worker(spec, attempt_id, stop_flag, stop_token, keep_partial)
}

pub fn cancel_download(key: &str) {
    let mut snapshot = None;
    if let Ok(mut g) = ACTIVE_DOWNLOADS.lock() {
        if let Some(map) = g.as_mut() {
            if let Some(mut job) = map.remove(key) {
                job.keep_partial.store(false, Ordering::SeqCst);
                job.stop_flag.store(true, Ordering::SeqCst);
                job.stop_token.cancel();
                job.attempt_id = job.attempt_id.wrapping_add(1);
            }
            snapshot = Some(serialize_queue_locked(map));
        }
    }
    cleanup_job_partials(&downloads_dir(), key);
    if let Some(items) = snapshot {
        persist_queue_snapshot(items);
    }
    notify_background_activity(true);
}

pub fn cancel_all_downloads() {
    let keys: Vec<String> = active_downloads().into_iter().map(|p| p.key).collect();
    for k in keys { cancel_download(&k); }
}

pub fn remove_download(id_or_key: &str) -> Result<()> {
    let _g = DOWNLOADS_LOCK.lock();
    let removed = remote_db::delete_download_entry(id_or_key)?;
    for (p, remote) in removed {
        if let Some(remote_name) = remote {
            let remote_fs = normalize_remote_fs(&remote_name);
            let remote_path = p.to_string_lossy().into_owned();
            if let Some(rt) = RUNTIME.get() {
                rt.spawn_blocking(move || { let _ = rstream_rclone::delete_remote_file(&remote_fs, &remote_path); });
            } else {
                let _ = rstream_rclone::delete_remote_file(&remote_fs, &remote_path);
            }
        } else {
            let _ = std::fs::remove_file(p);
        }
    }
    Ok(())
}

fn normalize_remote_fs(name: &str) -> String {
    let t = name.trim();
    if t.ends_with(':') { t.to_owned() } else { format!("{t}:") }
}

fn update_job_progress(key: &str, attempt_id: u64, f: impl FnOnce(&mut DownloadProgress)) {
    let mut state_changed = false;
    if let Ok(mut g) = ACTIVE_DOWNLOADS.lock() {
        if let Some(job) = g.as_mut().and_then(|m| m.get_mut(key)) {
            if job.attempt_id == attempt_id {
                let prev = job.progress.state.clone();
                f(&mut job.progress);
                state_changed = job.progress.state != prev;
            }
        }
    }
    notify_background_activity(state_changed);
}

fn store_resolved_source(key: &str, attempt_id: u64, source: &Source, subtitle: &str) {
    let mut snapshot = None;
    if let Ok(mut g) = ACTIVE_DOWNLOADS.lock() {
        if let Some(map) = g.as_mut() {
            if let Some(job) = map.get_mut(key) {
                if job.attempt_id == attempt_id {
                    job.spec.source = Some(source.clone());
                    job.spec.subtitle = subtitle.to_owned();
                    snapshot = Some(serialize_queue_locked(map));
                }
            }
        }
    }
    if let Some(items) = snapshot {
        persist_queue_snapshot(items);
    }
    notify_background_activity(true);
}

fn finish_job(key: &str, attempt_id: u64, res: std::result::Result<(), StopReason>) {
    let mut snapshot = None;
    if let Ok(mut g) = ACTIVE_DOWNLOADS.lock() {
        if let Some(map) = g.as_mut() {
            let matches_attempt = map.get(key).is_some_and(|j| j.attempt_id == attempt_id);
            if matches_attempt {
                match res {
                    Ok(()) | Err(StopReason::Cancelled) => {
                        map.remove(key);
                    }
                    Err(StopReason::Paused) => {
                        if let Some(job) = map.get_mut(key) {
                            job.progress.state = DownloadState::Paused;
                        }
                    }
                    Err(StopReason::Failed(e)) => {
                        if let Some(job) = map.get_mut(key) {
                            job.progress.state = DownloadState::Failed(format!("{e}"));
                        }
                    }
                }
                snapshot = Some(serialize_queue_locked(map));
            }
        }
    }
    if let Some(items) = snapshot {
        persist_queue_snapshot(items);
    }
    notify_background_activity(true);
}

fn record_completed_entry(entry: DownloadEntry) -> Result<()> {
    let _g = DOWNLOADS_LOCK.lock();
    remote_db::upsert_download_entry(entry)
}

fn subtitle_for_source(source: &Source) -> String {
    let sub = [source.quality.map(str::to_owned), source.hdr.map(str::to_owned)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    if sub.is_empty() { source.addon.clone() } else { sub }
}

pub fn episode_code_hint(season: i64, number: i64) -> String {
    if season > 0 && number > 0 {
        format!("S{:02}E{:02}", season, number)
    } else if number > 0 {
        format!("E{:02}", number)
    } else {
        String::new()
    }
}

/// Save a specific user-chosen source (Direct HTTP or Torrent) to Local storage or an Rclone remote.
pub fn save_source(
    media: Media,
    video_id: String,
    title: String,
    subtitle: String,
    source: Source,
    episode_hint: Option<String>,
    target: SaveTarget,
) -> Result<()> {
    let key = download_key_for_media(&media, &video_id);
    let subtitle = if subtitle.trim().is_empty() { subtitle_for_source(&source) } else { subtitle };
    let spec = JobSpec {
        key,
        media,
        video_id,
        episode_hint,
        title,
        subtitle,
        source: Some(source),
        matcher_lock: None,
        target,
    };
    enqueue_job(spec)
}

/// Queue an episode using a `StreamMatcherLock` (for Season / Full Show batch downloads).
pub fn save_episode_with_lock(
    media: Media,
    episode: Episode,
    lock: StreamMatcherLock,
    target: SaveTarget,
) -> Result<()> {
    let key = download_key_for_media(&media, &episode.id);
    let title = format!("{} {}", media.name, episode.label());
    let hint = episode_code_hint(episode.season, episode.number);
    let episode_hint = (!hint.is_empty()).then_some(hint).or_else(|| Some(episode.id.clone()));
    let spec = JobSpec {
        key,
        media,
        video_id: episode.id,
        episode_hint,
        title,
        subtitle: lock.quality.clone().unwrap_or_else(|| lock.addon.clone()),
        source: None,
        matcher_lock: Some(lock),
        target,
    };
    enqueue_job(spec)
}

/// Missing episodes in `episodes` for the default destination (excluding saved copies there and active jobs).
pub fn missing_episodes_for_batch(media: &Media, episodes: &[Episode]) -> Vec<Episode> {
    missing_episodes_for_batch_target(media, episodes, &default_save_target())
}

pub fn missing_episodes_for_batch_target(
    media: &Media,
    episodes: &[Episode],
    target: &SaveTarget,
) -> Vec<Episode> {
    let existing: HashSet<String> = list_downloads()
        .into_iter()
        .filter(|d| match target {
            SaveTarget::Local => d.remote.is_none(),
            SaveTarget::Remote(r) => d.remote.as_deref() == Some(r.as_str()),
        })
        .map(|d| d.key)
        .collect();
    let active: HashSet<String> = ACTIVE_DOWNLOADS
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref().map(|m| {
                m.iter()
                    .filter(|(_, j)| matches!(j.progress.state, DownloadState::Running | DownloadState::Queued | DownloadState::Paused))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
        })
        .unwrap_or_default();
    episodes
        .iter()
        .filter(|ep| {
            let k = download_key_for_media(media, &ep.id);
            !existing.contains(&k) && !active.contains(&k)
        })
        .cloned()
        .collect()
}

/// Queue a multi-episode batch (Season or Full Show): the reference episode uses `reference_source`
/// verbatim; remaining missing episodes resolve against `StreamMatcherLock::from_source(&reference_source)`.
pub fn queue_batch_with_lock(
    media: &Media,
    reference_ep: &Episode,
    reference_source: Source,
    episodes: &[Episode],
    target: &SaveTarget,
) -> usize {
    let missing = missing_episodes_for_batch_target(media, episodes, target);
    let lock = StreamMatcherLock::from_source(&reference_source);
    let mut queued = 0usize;
    for ep in missing {
        let res = if ep.id == reference_ep.id {
            let title = format!("{} {}", media.name, ep.label());
            let sub = subtitle_for_source(&reference_source);
            let hint = episode_code_hint(ep.season, ep.number);
            let ep_hint = (!hint.is_empty()).then_some(hint).or_else(|| Some(ep.id.clone()));
            save_source(
                media.clone(),
                ep.id.clone(),
                title,
                sub,
                reference_source.clone(),
                ep_hint,
                target.clone(),
            )
        } else {
            save_episode_with_lock(media.clone(), ep, lock.clone(), target.clone())
        };
        if res.is_ok() {
            queued += 1;
        }
    }
    queued
}

fn enqueue_job(spec: JobSpec) -> Result<()> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_token = CancellationToken::new();
    let keep_partial = Arc::new(AtomicBool::new(false));
    let (attempt_id, cleanup_old, snapshot) = {
        let mut g = ACTIVE_DOWNLOADS.lock().map_err(|_| anyhow!("download state busy"))?;
        let map = g.get_or_insert_with(HashMap::new);
        if map.get(&spec.key).is_some_and(|j| matches!(j.progress.state, DownloadState::Running | DownloadState::Queued)) {
            anyhow::bail!("Already queued or saving");
        }
        let same_src = |a: Option<&Source>, b: Option<&Source>| match (a, b) {
            (Some(x), Some(y)) => {
                x.url == y.url && x.info_hash == y.info_hash && x.file_idx == y.file_idx
            }
            (None, None) => true,
            _ => false,
        };
        let cleanup_old = map.get(&spec.key).is_some_and(|old| {
            old.spec.target != spec.target
                || !same_src(old.spec.source.as_ref(), spec.source.as_ref())
                || (spec.source.is_none() && old.spec.matcher_lock != spec.matcher_lock)
        });
        let next_attempt = map.get(&spec.key).map_or(1, |j| j.attempt_id.wrapping_add(1));
        map.insert(spec.key.clone(), ActiveJob {
            progress: DownloadProgress {
                key: spec.key.clone(),
                title: spec.title.clone(),
                target: spec.target.label(),
                downloaded: 0,
                total: spec.source.as_ref().and_then(|s| s.size_bytes),
                state: DownloadState::Queued,
            },
            spec: spec.clone(),
            attempt_id: next_attempt,
            stop_flag: stop_flag.clone(),
            stop_token: stop_token.clone(),
            keep_partial: keep_partial.clone(),
        });
        (next_attempt, cleanup_old, serialize_queue_locked(map))
    };
    if cleanup_old {
        cleanup_job_partials(&downloads_dir(), &spec.key);
    }
    persist_queue_snapshot(snapshot);
    notify_background_activity(true);
    spawn_job_worker(spec, attempt_id, stop_flag, stop_token, keep_partial)
}

fn spawn_job_worker(
    spec: JobSpec,
    attempt_id: u64,
    stop_flag: Arc<AtomicBool>,
    stop_token: CancellationToken,
    keep_partial: Arc<AtomicBool>,
) -> Result<()> {
    let dir = downloads_dir();
    spawn_on_backend(async move {
        let key = spec.key.clone();
        let res = run_job_attempt(&dir, spec, attempt_id, stop_flag, stop_token, keep_partial).await;
        finish_job(&key, attempt_id, res);
    })
}

fn classify_stop(keep_partial: &AtomicBool) -> StopReason {
    if keep_partial.load(Ordering::SeqCst) {
        StopReason::Paused
    } else {
        StopReason::Cancelled
    }
}

async fn run_job_attempt(
    dir: &std::path::Path,
    mut spec: JobSpec,
    attempt_id: u64,
    stop_flag: Arc<AtomicBool>,
    stop_token: CancellationToken,
    keep_partial: Arc<AtomicBool>,
) -> std::result::Result<(), StopReason> {
    let dl_slot = acquire_download_slot(&stop_token, &keep_partial).await?;
    if stop_flag.load(Ordering::SeqCst) || stop_token.is_cancelled() {
        return Err(classify_stop(&keep_partial));
    }
    update_job_progress(&spec.key, attempt_id, |p| p.state = DownloadState::Running);

    let source = match spec.source.take() {
        Some(s) => s,
        None => {
            let lock = spec.matcher_lock.as_ref().ok_or_else(|| StopReason::Failed(anyhow!("Missing stream matcher")))?;
            let (candidates, _) = tokio::select! {
                biased;
                _ = stop_token.cancelled() => return Err(classify_stop(&keep_partial)),
                res = sources(&spec.media.kind, &spec.video_id) => res.map_err(StopReason::Failed)?,
            };
            let matched = match_episode_source(lock, &candidates)
                .ok_or_else(|| StopReason::Failed(anyhow!("No matching stream found for {}", spec.title)))?;
            let sub = subtitle_for_source(&matched);
            store_resolved_source(&spec.key, attempt_id, &matched, &sub);
            spec.subtitle = sub;
            matched
        }
    };

    save_with_source(
        dir,
        &spec.key,
        attempt_id,
        spec.media,
        spec.video_id,
        spec.episode_hint,
        spec.title,
        spec.subtitle,
        source,
        spec.target,
        dl_slot,
        stop_flag,
        stop_token,
        keep_partial,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn download_source_to_part(
    key: &str,
    attempt_id: u64,
    source: &Source,
    part_path: &std::path::Path,
    ep_hint: Option<&str>,
    stop_flag: &AtomicBool,
    stop_token: &CancellationToken,
    keep_partial: &Arc<AtomicBool>,
) -> std::result::Result<u64, StopReason> {
    match source.kind {
        SourceKind::Direct => {
            download_http_to_path(
                key,
                attempt_id,
                &source.url,
                &source.headers,
                part_path,
                stop_flag,
                stop_token,
                keep_partial,
            )
            .await
        }
        SourceKind::Torrent => {
            let hash = source
                .info_hash
                .as_deref()
                .ok_or_else(|| StopReason::Failed(anyhow!("Torrent source missing infoHash")))?;
            download_torrent_file(
                key,
                attempt_id,
                hash,
                source.file_idx,
                &source.tracker_sources,
                part_path,
                ep_hint,
                stop_token.clone(),
                keep_partial.clone(),
            )
            .await
        }
        SourceKind::External => Err(StopReason::Failed(anyhow!("External links cannot be saved"))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn save_with_source(
    dir: &std::path::Path,
    key: &str,
    attempt_id: u64,
    media: Media,
    video_id: String,
    episode_hint: Option<String>,
    title: String,
    subtitle: String,
    source: Source,
    target: SaveTarget,
    dl_slot: DownloadSlotGuard,
    stop_flag: Arc<AtomicBool>,
    stop_token: CancellationToken,
    keep_partial: Arc<AtomicBool>,
) -> std::result::Result<(), StopReason> {
    if source.kind == SourceKind::External {
        return Err(StopReason::Failed(anyhow!("External links cannot be saved")));
    }
    std::fs::create_dir_all(dir).map_err(|e| StopReason::Failed(e.into()))?;
    let hint = hint_for_source(&source);
    let ep_hint = episode_hint.or_else(|| (video_id != media.id).then(|| video_id.clone()));
    let base = safe_filename(key, &hint);

    match target {
        SaveTarget::Local => {
            let final_path = dir.join(&base);
            let part_path = final_path.with_extension("part");
            let bytes = download_source_to_part(
                key,
                attempt_id,
                &source,
                &part_path,
                ep_hint.as_deref(),
                &stop_flag,
                &stop_token,
                &keep_partial,
            )
            .await?;
            std::fs::rename(&part_path, &final_path).map_err(|e| StopReason::Failed(e.into()))?;
            record_completed_entry(DownloadEntry {
                id: String::new(),
                key: key.to_owned(),
                media,
                video_id,
                title,
                subtitle,
                path: final_path,
                remote: None,
                bytes,
                downloaded_at: now_secs(),
            })
            .map_err(StopReason::Failed)
        }
        SaveTarget::Remote(remote_name) => {
            let staged_complete = dir.join(format!("stage-{base}"));
            let staged_part = dir.join(format!("stage-{base}.part"));
            let staged_bytes = if staged_complete.is_file() {
                std::fs::metadata(&staged_complete).map(|m| m.len()).unwrap_or(0)
            } else {
                let n = download_source_to_part(
                    key,
                    attempt_id,
                    &source,
                    &staged_part,
                    ep_hint.as_deref(),
                    &stop_flag,
                    &stop_token,
                    &keep_partial,
                )
                .await?;
                std::fs::rename(&staged_part, &staged_complete)
                    .map_err(|e| StopReason::Failed(e.into()))?;
                n
            };
            // Free the download slot so the next queued episode can download while this file uploads.
            drop(dl_slot);
            let _upload_permit = tokio::select! {
                biased;
                _ = stop_token.cancelled() => {
                    if !keep_partial.load(Ordering::SeqCst) {
                        let _ = std::fs::remove_file(&staged_complete);
                        let _ = std::fs::remove_file(&staged_part);
                    }
                    return Err(classify_stop(&keep_partial));
                }
                p = rclone_upload_semaphore().acquire_owned() => p.map_err(|_| StopReason::Failed(anyhow!("Upload queue closed")))?,
            };
            let remote_fs = normalize_remote_fs(&remote_name);
            let remote_path = remote_object_path(&media, &video_id, &title, &hint);
            match upload_local_to_rclone(
                key,
                attempt_id,
                &staged_complete,
                &remote_fs,
                &remote_path,
                staged_bytes,
                &stop_flag,
                &stop_token,
                &keep_partial,
            )
            .await
            {
                Ok(bytes) => {
                    let _ = std::fs::remove_file(&staged_complete);
                    record_completed_entry(DownloadEntry {
                        id: String::new(),
                        key: key.to_owned(),
                        media,
                        video_id,
                        title,
                        subtitle,
                        path: PathBuf::from(remote_path),
                        remote: Some(remote_name),
                        bytes,
                        downloaded_at: now_secs(),
                    })
                    .map_err(StopReason::Failed)
                }
                Err(StopReason::Paused) => Err(StopReason::Paused),
                Err(StopReason::Cancelled) => {
                    let _ = std::fs::remove_file(&staged_complete);
                    let _ = std::fs::remove_file(&staged_part);
                    Err(StopReason::Cancelled)
                }
                Err(StopReason::Failed(e)) => Err(StopReason::Failed(e)),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_torrent_file(
    key: &str,
    attempt_id: u64,
    info_hash: &str,
    file_idx: Option<u32>,
    tracker_sources: &[String],
    dest_path: &std::path::Path,
    episode_hint: Option<&str>,
    stop_token: CancellationToken,
    keep_partial: Arc<AtomicBool>,
) -> std::result::Result<u64, StopReason> {
    let mut payload = json!({ "infoHash": info_hash });
    if let Some(idx) = file_idx { payload["fileIdx"] = json!(idx); }
    if !tracker_sources.is_empty() { payload["sources"] = json!(tracker_sources); }
    if let Some(hint) = episode_hint.filter(|h| !h.is_empty()) {
        payload["episodeHint"] = json!(hint);
    }
    let key_owned = key.to_owned();
    let on_progress: Arc<dyn Fn(u64, Option<u64>) + Send + Sync> = Arc::new(move |done, total| {
        update_job_progress(&key_owned, attempt_id, |p| {
            p.downloaded = done;
            if total.is_some() { p.total = total; }
        });
    });
    let res = backend()
        .download_torrent
        .download_resumable(
            key,
            payload,
            dest_path,
            episode_hint,
            on_progress,
            stop_token.clone(),
            keep_partial.clone(),
        )
        .await;
    match res {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            if stop_token.is_cancelled() {
                if !keep_partial.load(Ordering::SeqCst) {
                    let _ = std::fs::remove_file(dest_path);
                }
                Err(classify_stop(&keep_partial))
            } else {
                Err(StopReason::Failed(e))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_local_to_rclone(
    key: &str,
    attempt_id: u64,
    local_file: &std::path::Path,
    remote_fs: &str,
    remote_path: &str,
    expected_bytes: u64,
    stop_flag: &AtomicBool,
    stop_token: &CancellationToken,
    keep_partial: &AtomicBool,
) -> std::result::Result<u64, StopReason> {
    update_job_progress(key, attempt_id, |p| {
        p.downloaded = 0;
        p.total = Some(expected_bytes);
    });
    let (lf, rfs, rpath) = (local_file.to_path_buf(), remote_fs.to_owned(), remote_path.to_owned());
    let jobid = tokio::task::spawn_blocking(move || rstream_rclone::copy_local_to_remote(&lf, &rfs, &rpath))
        .await
        .context("rclone upload worker failed")
        .and_then(|r| r)
        .map_err(StopReason::Failed)?;
    wait_rclone_job(key, attempt_id, jobid, Some(expected_bytes), stop_flag, stop_token, keep_partial).await
}

fn stop_and_wait_rclone_job_blocking(jobid: i64) {
    let _ = rstream_rclone::stop_job(jobid);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match rstream_rclone::job_progress(jobid) {
            Ok(p) if p.finished => break,
            Err(_) => break,
            Ok(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_rclone_job(
    key: &str,
    attempt_id: u64,
    jobid: i64,
    size_hint: Option<u64>,
    stop_flag: &AtomicBool,
    stop_token: &CancellationToken,
    keep_partial: &AtomicBool,
) -> std::result::Result<u64, StopReason> {
    loop {
        if stop_flag.load(Ordering::SeqCst) || stop_token.is_cancelled() {
            let _ = tokio::task::spawn_blocking(move || stop_and_wait_rclone_job_blocking(jobid)).await;
            return Err(classify_stop(keep_partial));
        }
        let jp = tokio::task::spawn_blocking(move || rstream_rclone::job_progress(jobid))
            .await
            .context("rclone poll worker failed")
            .and_then(|r| r)
            .map_err(StopReason::Failed)?;
        update_job_progress(key, attempt_id, |p| {
            p.downloaded = jp.bytes;
            p.total = jp.total.or(size_hint);
        });
        if jp.finished {
            if jp.success {
                return Ok(jp.bytes.max(jp.total.or(size_hint).unwrap_or(0)));
            }
            return Err(StopReason::Failed(anyhow!("{}", jp.error.unwrap_or_else(|| "Rclone transfer failed".into()))));
        }
        tokio::select! {
            biased;
            _ = stop_token.cancelled() => {
                let _ = tokio::task::spawn_blocking(move || stop_and_wait_rclone_job_blocking(jobid)).await;
                return Err(classify_stop(keep_partial));
            }
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
    }
}

fn parse_content_range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let raw = headers.get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    let slash = raw.rsplit('/').next()?.trim();
    if slash == "*" { None } else { slash.parse().ok() }
}

fn verify_content_range_206(
    headers: &reqwest::header::HeaderMap,
    expected_offset: u64,
) -> std::result::Result<Option<u64>, StopReason> {
    let raw = headers
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| StopReason::Failed(anyhow!("Missing Content-Range on HTTP 206")))?;
    let rest = raw
        .get(..6)
        .filter(|p| p.eq_ignore_ascii_case("bytes "))
        .and_then(|_| raw.get(6..))
        .map(str::trim)
        .ok_or_else(|| StopReason::Failed(anyhow!("Invalid Content-Range unit on HTTP 206")))?;
    let after_start = rest
        .strip_prefix(&format!("{expected_offset}-"))
        .ok_or_else(|| StopReason::Failed(anyhow!("Content-Range offset mismatch on HTTP 206")))?;
    let (end_str, total_str) = after_start
        .split_once('/')
        .ok_or_else(|| StopReason::Failed(anyhow!("Malformed Content-Range on HTTP 206")))?;
    let end: u64 = end_str
        .trim()
        .parse()
        .map_err(|_| StopReason::Failed(anyhow!("Malformed Content-Range end on HTTP 206")))?;
    if end < expected_offset {
        return Err(StopReason::Failed(anyhow!("Invalid Content-Range bounds on HTTP 206")));
    }
    match total_str.trim() {
        "*" => Ok(None),
        s => {
            let t: u64 = s
                .parse()
                .map_err(|_| StopReason::Failed(anyhow!("Malformed Content-Range total on HTTP 206")))?;
            if t <= end {
                return Err(StopReason::Failed(anyhow!("Invalid Content-Range total on HTTP 206")));
            }
            Ok(Some(t))
        }
    }
}

fn download_http_client() -> std::result::Result<&'static reqwest::Client, StopReason> {
    static CLIENT: OnceLock<std::result::Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15))
                .read_timeout(Duration::from_secs(45))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| StopReason::Failed(anyhow!("{e}")))
}

fn same_origin_url(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme() && a.host() == b.host() && a.port_or_known_default() == b.port_or_known_default()
}

fn is_loopback_url(u: &url::Url) -> bool {
    match u.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
        }
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.');
            d.eq_ignore_ascii_case("localhost")
                || d.to_ascii_lowercase().ends_with(".localhost")
        }
        None => true,
    }
}

fn is_playlist_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".m3u8") || lower.ends_with(".m3u")
}

#[allow(clippy::too_many_arguments)]
async fn download_http_to_path(
    key: &str,
    attempt_id: u64,
    url: &str,
    headers: &[(String, String)],
    part_path: &std::path::Path,
    stop_flag: &AtomicBool,
    stop_token: &CancellationToken,
    keep_partial: &AtomicBool,
) -> std::result::Result<u64, StopReason> {
    let initial_url = url::Url::parse(url)
        .map_err(|_| StopReason::Failed(anyhow!("Invalid download URL")))?;
    if !matches!(initial_url.scheme(), "http" | "https") {
        return Err(StopReason::Failed(anyhow!("Unsupported download URL scheme")));
    }
    if is_playlist_path(initial_url.path()) {
        return Err(StopReason::Failed(anyhow!("HLS playlists cannot be saved as a single file")));
    }
    if let Some(parent) = part_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| StopReason::Failed(e.into()))?;
    }
    let offset = tokio::fs::metadata(part_path).await.map(|m| m.len()).unwrap_or(0);
    if offset > 0 {
        update_job_progress(key, attempt_id, |p| p.downloaded = offset);
    }

    let client = download_http_client()?;
    let mut cur_url = initial_url.clone();
    let mut hop = 0usize;
    let mut resp = loop {
        let mut req = client.get(cur_url.as_str());
        if same_origin_url(&initial_url, &cur_url) {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }
        if offset > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        }
        let r = tokio::select! {
            biased;
            _ = stop_token.cancelled() => {
                if !keep_partial.load(Ordering::SeqCst) {
                    let _ = tokio::fs::remove_file(part_path).await;
                }
                return Err(classify_stop(keep_partial));
            }
            r = req.send() => r.map_err(|_| StopReason::Failed(anyhow!("Could not reach download source")))?,
        };
        if r.status().is_redirection() {
            if hop >= 8 {
                return Err(StopReason::Failed(anyhow!("Too many redirects")));
            }
            hop += 1;
            let loc = r
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| StopReason::Failed(anyhow!("Redirect missing Location header")))?;
            let next = cur_url
                .join(loc)
                .map_err(|_| StopReason::Failed(anyhow!("Invalid redirect Location")))?;
            if next.scheme() != "https"
                || !next.username().is_empty()
                || next.password().is_some()
                || is_loopback_url(&next)
            {
                return Err(StopReason::Failed(anyhow!("Unsafe redirect blocked")));
            }
            if is_playlist_path(next.path()) {
                return Err(StopReason::Failed(anyhow!("HLS playlists cannot be saved as a single file")));
            }
            cur_url = next;
            continue;
        }
        break r;
    };

    let status = resp.status();
    if offset > 0 && status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        if let Some(total) = parse_content_range_total(resp.headers()) {
            if offset == total && total > 0 {
                update_job_progress(key, attempt_id, |p| {
                    p.downloaded = total;
                    p.total = Some(total);
                });
                return Ok(total);
            }
        }
    }

    if !status.is_success() {
        return Err(StopReason::Failed(anyhow!("Download failed (HTTP {})", status.as_u16())));
    }

    let ctype_is_playlist = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("mpegurl"));
    if ctype_is_playlist || is_playlist_path(cur_url.path()) {
        return Err(StopReason::Failed(anyhow!("HLS playlists cannot be saved as a single file")));
    }

    let (mut file, mut written, total) = if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let cr_total = verify_content_range_206(resp.headers(), offset)?;
        let total = cr_total.or_else(|| resp.content_length().map(|rem| offset + rem));
        let f = if offset > 0 {
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(part_path)
                .await
                .map_err(|e| StopReason::Failed(e.into()))?
        } else {
            tokio::fs::File::create(part_path)
                .await
                .map_err(|e| StopReason::Failed(e.into()))?
        };
        (f, offset, total)
    } else {
        let total = resp.content_length();
        let f = tokio::fs::File::create(part_path)
            .await
            .map_err(|e| StopReason::Failed(e.into()))?;
        (f, 0u64, total)
    };

    update_job_progress(key, attempt_id, |p| {
        p.downloaded = written;
        p.total = total;
    });

    use tokio::io::AsyncWriteExt;
    let mut last_update = std::time::Instant::now();
    loop {
        if stop_flag.load(Ordering::SeqCst) || stop_token.is_cancelled() {
            let _ = file.flush().await;
            drop(file);
            if !keep_partial.load(Ordering::SeqCst) {
                let _ = tokio::fs::remove_file(part_path).await;
            }
            return Err(classify_stop(keep_partial));
        }
        let next_chunk = tokio::select! {
            biased;
            _ = stop_token.cancelled() => {
                let _ = file.flush().await;
                drop(file);
                if !keep_partial.load(Ordering::SeqCst) {
                    let _ = tokio::fs::remove_file(part_path).await;
                }
                return Err(classify_stop(keep_partial));
            }
            c = resp.chunk() => c.map_err(|_| StopReason::Failed(anyhow!("Connection interrupted")))?,
        };
        let Some(chunk) = next_chunk else { break };
        if written == 0 && chunk.starts_with(b"#EXTM3U") {
            drop(file);
            let _ = tokio::fs::remove_file(part_path).await;
            return Err(StopReason::Failed(anyhow!("HLS playlists cannot be saved as a single file")));
        }
        file.write_all(&chunk).await.map_err(|e| StopReason::Failed(e.into()))?;
        written += chunk.len() as u64;
        if last_update.elapsed() >= Duration::from_millis(200) {
            last_update = std::time::Instant::now();
            update_job_progress(key, attempt_id, |p| p.downloaded = written);
        }
    }
    file.flush().await.map_err(|e| StopReason::Failed(e.into()))?;
    drop(file);
    if written == 0 {
        let _ = tokio::fs::remove_file(part_path).await;
        return Err(StopReason::Failed(anyhow!("Empty download response")));
    }
    if let Some(t) = total {
        if written != t {
            if written > t {
                let _ = tokio::fs::remove_file(part_path).await;
            }
            return Err(StopReason::Failed(anyhow!("Incomplete download ({written} of {t} bytes)")));
        }
    }
    update_job_progress(key, attempt_id, |p| p.downloaded = written);
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn http_range_pause_resume_completion_and_cancel_cleanup() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rstream-dl-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(dir.clone());
        if let Ok(mut g) = ACTIVE_DOWNLOADS.lock() { *g = Some(HashMap::new()); }

        let payload: Vec<u8> = (0..65_536u32).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let first_chunk_sent = Arc::new(tokio::sync::Notify::new());
            let notify_srv = first_chunk_sent.clone();
            let seen_ranges = Arc::new(Mutex::new(Vec::<String>::new()));
            let ranges_srv = seen_ranges.clone();
            let srv_data = payload.clone();

            let server = tokio::spawn(async move {
                // Connection 1: send first 16 KiB, then stall so client pauses cleanly mid-stream.
                let (mut s1, _) = listener.accept().await.unwrap();
                let mut req1 = [0u8; 2048];
                let n1 = s1.read(&mut req1).await.unwrap_or(0);
                ranges_srv.lock().unwrap().push(String::from_utf8_lossy(&req1[..n1]).to_ascii_lowercase());
                let hdr1 = format!(
                    "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    srv_data.len()
                );
                let _ = s1.write_all(hdr1.as_bytes()).await;
                let _ = s1.write_all(&srv_data[..16_384]).await;
                let _ = s1.flush().await;
                notify_srv.notify_one();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    drop(s1);
                });

                // Connection 2: verify `Range: bytes=16384-` and serve 206 Partial Content!
                let (mut s2, _) = listener.accept().await.unwrap();
                let mut req2 = [0u8; 2048];
                let n2 = s2.read(&mut req2).await.unwrap_or(0);
                let req2_str = String::from_utf8_lossy(&req2[..n2]).to_ascii_lowercase();
                ranges_srv.lock().unwrap().push(req2_str.clone());
                let offset: usize = req2_str
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|r| r.trim().trim_end_matches('-').parse().ok())
                    .unwrap_or(0);
                let rem = &srv_data[offset..];
                let hdr2 = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    offset,
                    srv_data.len() - 1,
                    srv_data.len(),
                    rem.len()
                );
                let _ = s2.write_all(hdr2.as_bytes()).await;
                let _ = s2.write_all(rem).await;
                let _ = s2.flush().await;

                // Connection 3: for cancel_download partial cleanup test.
                let (mut s3, _) = listener.accept().await.unwrap();
                let mut req3 = [0u8; 2048];
                let _ = s3.read(&mut req3).await;
                let hdr3 = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    srv_data.len()
                );
                let _ = s3.write_all(hdr3.as_bytes()).await;
                let _ = s3.write_all(&srv_data[..8_192]).await;
                let _ = s3.flush().await;
                notify_srv.notify_one();
                tokio::time::sleep(Duration::from_secs(5)).await;
            });

            let media = Media::from_json(&json!({"id":"tt1375666","type":"movie","name":"Inception"}), CINEMETA).unwrap();
            let key = download_key(&media.key(), "tt1375666");
            let url = format!("http://{addr}/inception.mkv");
            let src = source_of("TestAddon", &json!({"title": "Inception 1080p", "url": url}));

            save_source(media.clone(), "tt1375666".into(), "Inception".into(), "1080p".into(), src, None, SaveTarget::Local).unwrap();
            first_chunk_sent.notified().await;
            tokio::time::sleep(Duration::from_millis(80)).await;

            // Pause must keep the `.part` file on disk and transition state to `Paused`.
            pause_download(&key);
            tokio::time::sleep(Duration::from_millis(80)).await;
            let active = active_downloads();
            assert_eq!(active.len(), 1);
            assert_eq!(active[0].state, DownloadState::Paused);

            let part_file = downloads_dir().join(safe_filename(&key, &url)).with_extension("part");
            assert_eq!(std::fs::metadata(&part_file).unwrap().len(), 16_384);

            // Queue file is persisted with 0600 permissions.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(downloads_queue_path()).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
            }

            // Resume must send `Range: bytes=16384-`, append remaining bytes, and finalize!
            resume_download(&key).unwrap();
            for _ in 0..50 {
                if list_downloads().len() == 1 { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let completed = list_downloads();
            assert_eq!(completed.len(), 1);
            assert_eq!(completed[0].bytes, expected.len() as u64);
            assert_eq!(std::fs::read(&completed[0].path).unwrap(), expected);
            assert!(!part_file.exists());
            assert!(seen_ranges.lock().unwrap()[1].contains("range: bytes=16384-"));

            // Now test `cancel_download` cleans up partial `.part` files completely.
            let media2 = Media::from_json(&json!({"id":"tt0133093","type":"movie","name":"The Matrix"}), CINEMETA).unwrap();
            let key2 = download_key(&media2.key(), "tt0133093");
            let url2 = format!("http://{addr}/matrix.mkv");
            let src2 = source_of("TestAddon", &json!({"title": "The Matrix 1080p", "url": url2}));
            save_source(media2, "tt0133093".into(), "The Matrix".into(), "1080p".into(), src2, None, SaveTarget::Local).unwrap();
            first_chunk_sent.notified().await;
            tokio::time::sleep(Duration::from_millis(80)).await;

            let part_file2 = downloads_dir().join(safe_filename(&key2, &url2)).with_extension("part");
            assert!(part_file2.exists());
            cancel_download(&key2);
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(!part_file2.exists(), "cancel_download must delete partial .part file");
            assert!(active_downloads().iter().all(|j| j.key != key2));

            server.abort();
        });

        *TEST_DOWNLOADS_ROOT.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cinemeta_canonical_keys_human_paths_and_multi_device_crypt_db_sync() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root_a = std::env::temp_dir().join(format!("rstream-sync-a-{}", std::process::id()));
        let root_b = std::env::temp_dir().join(format!("rstream-sync-b-{}", std::process::id()));
        let root_c = std::env::temp_dir().join(format!("rstream-sync-c-{}", std::process::id()));
        let cloud_backing = std::env::temp_dir().join(format!("rstream-sync-cloud-{}", std::process::id()));
        for p in [&root_a, &root_b, &root_c, &cloud_backing] {
            let _ = std::fs::remove_dir_all(p);
            std::fs::create_dir_all(p).unwrap();
        }

        // 1. Human-readable remote paths & Cinemeta canonicalization (stripping private addon_url).
        let trakt_show = Media::from_json(
            &json!({
                "id": "tmdb:1396",
                "imdb_id": "tt0903747",
                "type": "series",
                "name": "The Breaking Bad",
                "releaseInfo": "2008–2013",
            }),
            "https://private.addon.example/my-secret-token/manifest.json",
        )
        .unwrap();
        let canon = remote_db::canonicalize_catalog_media(trakt_show.clone());
        assert_eq!(canon.id, "tt0903747");
        assert_eq!(canon.addon_url, CINEMETA);
        assert_eq!(
            download_key_for_media(&trakt_show, "tmdb:1396:1:2"),
            "series|tt0903747|tt0903747:1:2"
        );
        assert_eq!(
            remote_object_path(&canon, "tt0903747:1:2", "Breaking Bad S1 E2", "ep.mkv"),
            "RStream/Series/B/The Breaking Bad (2008)/Season 01/The Breaking Bad - S01E02.mkv"
        );
        let num_movie = Media::from_json(
            &json!({"id": "tt8579674", "type": "movie", "name": "1917", "releaseInfo": "2019"}),
            CINEMETA,
        )
        .unwrap();
        assert_eq!(
            remote_object_path(&num_movie, "tt8579674", "1917", "1917.mp4"),
            "RStream/Movies/#/1917 (2019)/1917 (2019).mp4"
        );
        let kitsu_ova = Media::from_json(
            &json!({"id": "kitsu:1264", "type": "series", "name": "Open Special", "releaseInfo": "2008"}),
            "https://catalog.example/manifest.json",
        )
        .unwrap();
        assert_eq!(
            remote_object_path(&kitsu_ova, "kitsu:1264:1", "Open Special E1", "ova.mkv"),
            "RStream/Series/O/Open Special (2008)/Open Special - Episode 01.mkv"
        );

        // 2. Multi-device append-only sync + compaction over an encrypted `rclone crypt` remote.
        let rclone_conf = cloud_backing.join("rclone.conf");
        let cipher_dir = cloud_backing.join("cipher");
        std::fs::create_dir_all(&cipher_dir).unwrap();
        rstream_rclone::init(&rclone_conf).unwrap();
        let mut params = serde_json::Map::new();
        params.insert("Root".into(), json!(cipher_dir.to_string_lossy()));
        rstream_rclone::setup_remote(rstream_rclone::RemoteSetupRequest {
            name: "shared_vault".into(),
            provider: "local".into(),
            parameters: params,
            mode: rstream_rclone::RemoteSetupMode::CreateNew,
            crypt: Some(rstream_rclone::CryptOptions {
                password: "shared-vault-password".into(),
                salt: Some("shared-vault-salt".into()),
                folder: "RStreamVault".into(),
            }),
        })
        .unwrap();

        // Device A writes 42 episodes (crossing the 40-event delta compaction threshold!).
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_a.clone());
        for ep in 1..=41u32 {
            let vid = format!("tt0903747:1:{ep}");
            remote_db::upsert_download_entry(DownloadEntry {
                id: String::new(),
                key: String::new(),
                media: trakt_show.clone(),
                video_id: format!("tmdb:1396:1:{ep}"),
                title: format!("The Breaking Bad S1 E{ep}"),
                subtitle: "1080p HDR".into(),
                path: PathBuf::from(remote_object_path(&canon, &vid, "", "ep.mkv")),
                remote: Some("shared_vault".into()),
                bytes: 500_000_000 + u64::from(ep),
                downloaded_at: 1_700_000_000 + i64::from(ep),
            })
            .unwrap();
            remote_db::sync_remote_db("shared_vault").unwrap();
        }

        // Verify delta compaction ran on `shared_vault:RStream/.db` and no secret token leaked!
        let db_files = rstream_rclone::list_remote_files_recursive("shared_vault:", "RStream/.db").unwrap();
        assert!(
            db_files.iter().any(|f| f.path.contains("deltas/delta-")),
            "expected delta rollup after >40 events, got {db_files:?}"
        );
        for f in &db_files {
            if !f.is_dir && f.path.ends_with(".jsonl") {
                let rel = if f.path.starts_with("RStream/.db/") {
                    f.path.clone()
                } else {
                    format!("RStream/.db/{}", f.path.trim_start_matches('/'))
                };
                let raw = String::from_utf8_lossy(&rstream_rclone::get_remote_bytes("shared_vault:", &rel).unwrap()).into_owned();
                assert!(!raw.contains("my-secret-token"), "remote DB must never leak custom addon URLs");
            }
        }

        // Device B (separate SQLite DB!) concurrently adds a movie AND deletes S1 E3, then syncs.
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_b.clone());
        remote_db::sync_remote_db("shared_vault").unwrap();
        assert_eq!(list_downloads().len(), 41);

        remote_db::upsert_download_entry(DownloadEntry {
            id: String::new(),
            key: String::new(),
            media: num_movie.clone(),
            video_id: "tt8579674".into(),
            title: "1917".into(),
            subtitle: "4K HDR10+".into(),
            path: PathBuf::from(remote_object_path(&num_movie, "tt8579674", "1917", "1917.mp4")),
            remote: Some("shared_vault".into()),
            bytes: 8_000_000_000,
            downloaded_at: 1_700_001_000,
        })
        .unwrap();
        remote_db::delete_download_entry("series|tt0903747|tt0903747:1:3").unwrap();
        remote_db::sync_remote_db("shared_vault").unwrap();

        // Device C (brand-new install connecting to existing remote) pulls delta + peer event batch:
        // sees 40 episodes + 1 movie = 41 items, S1 E3 deleted, AND saved stream options appear!
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_c.clone());
        // Also create one local saved copy of S1 E1 on Device C so BOTH Local and Rclone stream options appear!
        let local_ep1 = root_c.join("downloads").join("local_ep1.mkv");
        std::fs::create_dir_all(local_ep1.parent().unwrap()).unwrap();
        std::fs::write(&local_ep1, b"local video bytes").unwrap();
        remote_db::upsert_download_entry(DownloadEntry {
            id: String::new(),
            key: String::new(),
            media: canon.clone(),
            video_id: "tt0903747:1:1".into(),
            title: "The Breaking Bad S1 E1".into(),
            subtitle: "1080p".into(),
            path: local_ep1,
            remote: None,
            bytes: 17,
            downloaded_at: 1_700_002_000,
        })
        .unwrap();

        remote_db::sync_remote_db("shared_vault").unwrap();
        let (_, total_c) = remote_db::list_downloads_paged("", 10, 0);
        assert_eq!(total_c, 42, "40 remote episodes + 1 remote movie + 1 local episode");
        assert!(find_download("series|tt0903747|https://v3-cinemeta.strem.io/manifest.json", "tt0903747:1:3").is_none());

        // Stream options for S1 E1 include BOTH Local device (first) and Rclone (shared_vault) (second)!
        let stream_opts = remote_db::saved_sources_for("series", "tt0903747:1:1");
        assert_eq!(stream_opts.len(), 2);
        assert_eq!(stream_opts[0].addon, "Local device");
        assert_eq!(stream_opts[1].addon, "Rclone (shared_vault)");
        assert_eq!(stream_opts[1].quality, Some("1080p"));
        assert_eq!(stream_opts[1].hdr, Some("HDR"));

        let _ = rstream_rclone::delete_remote("shared_vault");
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = None;
        for p in [&root_a, &root_b, &root_c, &cloud_backing] {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    #[test]
    fn dynamic_parallel_download_slots_and_profile_sync_isolation() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("rstream-profile-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root_a = base.join("dev_a");
        let root_b = base.join("dev_b");
        let root_friend = base.join("dev_friend");
        let cloud_dir = base.join("cloud");
        for p in [&root_a, &root_b, &root_friend, &cloud_dir] {
            std::fs::create_dir_all(p).unwrap();
        }

        // 1. Verify dynamic download concurrency limiter wakes waiters when raised from 2 -> 4.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            set_parallel_downloads_limit(2);
            let tok = CancellationToken::new();
            let keep = AtomicBool::new(false);
            let s1 = acquire_download_slot(&tok, &keep).await.unwrap();
            let s2 = acquire_download_slot(&tok, &keep).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(30), acquire_download_slot(&tok, &keep))
                    .await
                    .is_err(),
                "3rd download must wait while limit is 2"
            );
            set_parallel_downloads_limit(4);
            let s3 = tokio::time::timeout(Duration::from_millis(200), acquire_download_slot(&tok, &keep))
                .await
                .expect("raising limit must wake queued download")
                .unwrap();
            let s4 = acquire_download_slot(&tok, &keep).await.unwrap();
            drop((s1, s2, s3, s4));
            set_parallel_downloads_limit(3);
        });

        // 2. Verify personal profile sync (`RStream/.profiles/<profile>/`) syncs binge_locks &
        //    media/playback LWW state across devices on `"default"` while keeping `"friend"` isolated.
        let rclone_conf = base.join("rclone.conf");
        if rstream_rclone::init(&rclone_conf).is_ok() {
            let mut params = serde_json::Map::new();
            params.insert("Root".into(), json!(cloud_dir.to_string_lossy()));
            rstream_rclone::setup_remote(rstream_rclone::RemoteSetupRequest {
                name: "profile_cloud".into(),
                provider: "local".into(),
                parameters: params,
                mode: rstream_rclone::RemoteSetupMode::CreateNew,
                crypt: Some(rstream_rclone::CryptOptions {
                    password: "profile-sync-password".into(),
                    salt: Some("profile-sync-salt".into()),
                    folder: "ProfileVault".into(),
                }),
            })
            .unwrap();

            *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_a.clone());
            let show = Media::from_json(
                &json!({"id": "tt1748166", "type": "series", "name": "Pioneer One"}),
                CINEMETA,
            )
            .unwrap();
            let src = sources::source_of(
                "StreamAddon",
                &json!({
                    "name": "StreamAddon\n1080p",
                    "title": "[EncoderA] Pioneer One\n💾 216.71 MB",
                    "infoHash": "1111000011110000111100001111000011110000",
                    "fileIdx": 1,
                    "behaviorHints": { "bingeGroup": "addon-a|1111000011110000111100001111000011110000" }
                }),
            );
            let lock = StreamMatcherLock::from_source_with_pack(&src, Some(24));
            remote_db::save_binge_lock(&show, &lock);
            remote_db::sync_profile_remote("profile_cloud", "default").unwrap();

            // Device B on profile `"default"` receives the Pioneer One `[EncoderA]` bingeGroup lock!
            *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_b.clone());
            let changed_b = remote_db::sync_profile_remote("profile_cloud", "default").unwrap();
            assert!(changed_b >= 1);
            let synced_lock = remote_db::load_binge_lock(&show).expect("binge_lock synced to Device B");
            assert_eq!(synced_lock.binge_group, lock.binge_group);
            assert_eq!(synced_lock.release_group.as_deref(), Some("EncoderA"));

            // Friend device on profile `"friend"` sharing the exact same remote stays isolated!
            *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_friend.clone());
            let changed_friend = remote_db::sync_profile_remote("profile_cloud", "friend").unwrap();
            assert_eq!(changed_friend, 0);
            assert!(remote_db::load_binge_lock(&show).is_none());

            let _ = rstream_rclone::delete_remote("profile_cloud");
        }

        *TEST_DOWNLOADS_ROOT.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn http_source_switch_cleans_partials_and_rejects_bad_206_redirects_playlists_truncation() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rstream-dl-safety-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(dir.clone());
        if let Ok(mut g) = ACTIVE_DOWNLOADS.lock() {
            *g = Some(HashMap::new());
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let first_paused = Arc::new(tokio::sync::Notify::new());
            let notify_srv = first_paused.clone();
            let v1_bytes = vec![b'A'; 32_768];
            let v2_bytes = vec![b'B'; 24_576];
            let v2_expected = v2_bytes.clone();

            let server = tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else { break };
                    let v1 = v1_bytes.clone();
                    let v2 = v2_bytes.clone();
                    let ntf = notify_srv.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        let first_line = req.lines().next().unwrap_or("");
                        if first_line.contains("/v1.mkv") {
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                v1.len()
                            );
                            let _ = sock.write_all(hdr.as_bytes()).await;
                            let _ = sock.write_all(&v1[..16_384]).await;
                            let _ = sock.flush().await;
                            ntf.notify_one();
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        } else if first_line.contains("/v2.mkv") {
                            assert!(
                                !req.to_ascii_lowercase().contains("range:"),
                                "source switch must not send stale Range header: {req}"
                            );
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                v2.len()
                            );
                            let _ = sock.write_all(hdr.as_bytes()).await;
                            let _ = sock.write_all(&v2).await;
                        } else if first_line.contains("/bad206.mkv") {
                            let hdr = "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-99/200\r\nContent-Length: 100\r\nConnection: close\r\n\r\n";
                            let _ = sock.write_all(hdr.as_bytes()).await;
                            let _ = sock.write_all(&[b'Z'; 100]).await;
                        } else if first_line.contains("/redir_http") {
                            let hdr = format!(
                                "HTTP/1.1 302 Found\r\nLocation: http://{addr}/v2.mkv\r\nConnection: close\r\n\r\n"
                            );
                            let _ = sock.write_all(hdr.as_bytes()).await;
                        } else if first_line.contains("/redir_loopback") {
                            let hdr = "HTTP/1.1 302 Found\r\nLocation: https://127.0.0.1:443/v2.mkv\r\nConnection: close\r\n\r\n";
                            let _ = sock.write_all(hdr.as_bytes()).await;
                        } else if first_line.contains("/playlist_ct.mkv") {
                            let body = b"#EXTM3U\n#EXT-X-VERSION:3\n";
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = sock.write_all(hdr.as_bytes()).await;
                            let _ = sock.write_all(body).await;
                        } else if first_line.contains("/empty.mkv") {
                            let hdr = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                            let _ = sock.write_all(hdr.as_bytes()).await;
                        } else if first_line.contains("/truncated.mkv") {
                            let hdr = "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n";
                            let _ = sock.write_all(hdr.as_bytes()).await;
                            let _ = sock.write_all(&[b'T'; 200]).await;
                        }
                        let _ = sock.flush().await;
                    });
                }
            });

            // 1. Start v1, pause at 16 KiB, also plant a dummy resumable-* dir & stage-*.part, then switch to v2.
            let media = Media::from_json(&json!({"id":"tt0816692","type":"movie","name":"Interstellar"}), CINEMETA).unwrap();
            let key = download_key_for_media(&media, "tt0816692");
            let url1 = format!("http://{addr}/v1.mkv");
            let url2 = format!("http://{addr}/v2.mkv");
            save_source(
                media.clone(),
                "tt0816692".into(),
                "Interstellar".into(),
                "1080p".into(),
                source_of("Addon", &json!({"title": "v1", "url": url1})),
                None,
                SaveTarget::Local,
            )
            .unwrap();
            first_paused.notified().await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            pause_download(&key);
            tokio::time::sleep(Duration::from_millis(60)).await;

            let part_path = downloads_dir().join(safe_filename(&key, &url1)).with_extension("part");
            assert_eq!(std::fs::metadata(&part_path).unwrap().len(), 16_384);
            let stage_mp4 = downloads_dir().join(format!("stage-{}.mp4", key_hash_hex(&key)));
            std::fs::write(&stage_mp4, b"stale-stage").unwrap();
            let key_sha = {
                use sha2::{Digest, Sha256};
                let d = Sha256::digest(key.as_bytes());
                d.iter().map(|b| format!("{b:02x}")).collect::<String>()
            };
            let resumable_dir = dir
                .join("torrent-downloads/torrent-v1")
                .join(format!("resumable-{}", &key_sha[..48]));
            std::fs::create_dir_all(&resumable_dir).unwrap();

            // Replace paused job with different URL (`v2.mkv`): old `.part`, `stage-*`, and `resumable-*` must be wiped!
            save_source(
                media,
                "tt0816692".into(),
                "Interstellar".into(),
                "1080p".into(),
                source_of("Addon", &json!({"title": "v2", "url": url2})),
                None,
                SaveTarget::Local,
            )
            .unwrap();
            for _ in 0..50 {
                if list_downloads().len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            assert!(!stage_mp4.exists(), "stale stage file must be cleaned on source switch");
            assert!(!resumable_dir.exists(), "stale resumable-* dir must be cleaned on source switch");
            let saved = list_downloads();
            assert_eq!(saved.len(), 1);
            assert_eq!(
                std::fs::read(&saved[0].path).unwrap(),
                v2_expected,
                "switched HTTP source must never splice old .part bytes"
            );

            // 2. Direct unit checks on download_http_to_path edge rejections.
            let stop_flag = AtomicBool::new(false);
            let stop_tok = CancellationToken::new();
            let keep = AtomicBool::new(true);
            let test_part = downloads_dir().join("edge.part");

            // Seed 50 bytes so `offset = 50`, then server answers 206 with `bytes 0-99/200` -> must fail without splicing!
            std::fs::write(&test_part, [b'X'; 50]).unwrap();
            assert!(download_http_to_path("k", 1, &format!("http://{addr}/bad206.mkv"), &[], &test_part, &stop_flag, &stop_tok, &keep).await.is_err());
            assert_eq!(std::fs::metadata(&test_part).unwrap().len(), 50, "mismatched 206 must not append");
            let _ = std::fs::remove_file(&test_part);

            for bad_url in [
                format!("http://{addr}/redir_http"),
                format!("http://{addr}/redir_loopback"),
                format!("http://{addr}/stream.m3u8"),
                format!("http://{addr}/playlist_ct.mkv"),
                format!("http://{addr}/empty.mkv"),
                format!("http://{addr}/truncated.mkv"),
            ] {
                let _ = std::fs::remove_file(&test_part);
                assert!(
                    download_http_to_path("k", 1, &bad_url, &[], &test_part, &stop_flag, &stop_tok, &keep).await.is_err(),
                    "expected error for `{bad_url}`"
                );
            }

            server.abort();
        });

        *TEST_DOWNLOADS_ROOT.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn core_profile_sync_hlc_deletes_isolation_compaction_abort_and_pagination() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("rstream-core-sync-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root_a = base.join("dev_a");
        let root_b = base.join("dev_b");
        let root_c = base.join("dev_c");
        let cloud_1 = base.join("cloud_1");
        let cloud_2 = base.join("cloud_2");
        for p in [&root_a, &root_b, &root_c, &cloud_1, &cloud_2] {
            std::fs::create_dir_all(p).unwrap();
        }

        // Verify path sanitization & kitsu addon URL invariants first.
        let kitsu_m = Media::from_json(&json!({"id": "kitsu:1264", "type": "series", "name": "NUL"}), "https://priv/manifest.json").unwrap();
        assert_eq!(
            remote_db::sanitized_media_json(&kitsu_m, "kitsu:1264")["addon_url"],
            "https://anime-kitsu.strem.fun/manifest.json"
        );
        assert_eq!(
            remote_object_path(&kitsu_m, "kitsu:1264:1", "CON . . . ", "a.mkv"),
            "RStream/Series/N/_NUL/_NUL - Episode 01.mkv"
        );
        let long_cjk = "あ".repeat(60); // 180 UTF-8 bytes
        let long_m = Media::from_json(&json!({"id": "tt1234567", "type": "movie", "name": format!(" {long_cjk} . . ")}), CINEMETA).unwrap();
        let long_path = remote_object_path(&long_m, "tt1234567", "", "x.mkv");
        for seg in long_path.split('/') {
            // Folder/file stem must respect <=100 UTF-8 bytes (+ ".mkv").
            assert!(seg.len() <= 104, "segment too long ({}B): {seg}", seg.len());
        }

        let rclone_conf = base.join("rclone.conf");
        rstream_rclone::init(&rclone_conf).unwrap();
        for (name, root) in [("vault_1", &cloud_1), ("vault_2", &cloud_2)] {
            let mut params = serde_json::Map::new();
            params.insert("Root".into(), json!(root.to_string_lossy()));
            rstream_rclone::setup_remote(rstream_rclone::RemoteSetupRequest {
                name: name.into(),
                provider: "local".into(),
                parameters: params,
                mode: rstream_rclone::RemoteSetupMode::CreateNew,
                crypt: Some(rstream_rclone::CryptOptions {
                    password: format!("pw-{name}"),
                    salt: Some(format!("salt-{name}")),
                    folder: "Vault".into(),
                }),
            })
            .unwrap();
        }

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core_a = Core::open(&root_a).unwrap();
        let core_b = Core::open(&root_b).unwrap();

        // 1. Fast-clock HLC causal delete on real `Core` + drift clamping + cnt=999_999 rollover.
        let future_sec = now_secs() + 600;
        rt.block_on(core_b.execute(json!({
            "op": "sync_apply",
            "media": [{
                "type": "movie",
                "id": "tt0133093",
                "addon_url": CINEMETA,
                "meta": {"id": "tt0133093", "type": "movie", "name": "The Matrix"},
                "saved": true,
                "watched": false,
                "position_ms": 0,
                "duration_ms": 0,
                "watched_ms": 0,
                "liked": 1,
                "updated": future_sec,
            }],
            "playback": [],
        })))
        .unwrap();

        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_b.clone());
        write_private_file(
            &root_b.join("settings.json"),
            br#"{"sync_remote":"vault_1","sync_profile":"default"}"#,
        )
        .unwrap();
        // Force Device B's HLC clock 10 minutes ahead AND at cnt = 999_999 to verify rollover!
        {
            let conn_b = rusqlite::Connection::open(root_b.join("downloads.sqlite")).unwrap();
            remote_db::sync_profile_remote_with_core("vault_1", "default", None).unwrap();
            let fast_ms = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64) + 600_000;
            conn_b.execute("INSERT OR REPLACE INTO meta(k, v) VALUES('hlc_ms', ?1)", [fast_ms.to_string()]).unwrap();
            conn_b.execute("INSERT OR REPLACE INTO meta(k, v) VALUES('hlc_cnt', '999999')", []).unwrap();
        }
        remote_db::sync_profile_remote_with_core("vault_1", "default", Some(&core_b)).unwrap();
        // Verify rollover kept fixed 15-6-device width!
        {
            let conn_b = rusqlite::Connection::open(root_b.join("downloads.sqlite")).unwrap();
            let hlc_b: String = conn_b
                .query_row("SELECT updated_hlc FROM profile_kv_hlc WHERE item_key = 'movie|tt0133093'", [], |r| r.get(0))
                .unwrap();
            let parts: Vec<&str> = hlc_b.split('-').collect();
            assert_eq!(parts[0].len(), 15);
            assert_eq!(parts[1].len(), 6);
        }

        // Device A (normal clock) pulls fast-clock item, then forgets it -> tombstone MUST beat Device B's fast HLC!
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_a.clone());
        write_private_file(
            &root_a.join("settings.json"),
            br#"{"sync_remote":"vault_1","sync_profile":"default"}"#,
        )
        .unwrap();
        remote_db::sync_profile_remote_with_core("vault_1", "default", Some(&core_a)).unwrap();
        let lib_a = rt.block_on(core_a.execute(json!({"op": "library"}))).unwrap();
        assert_eq!(lib_a["items"].as_array().unwrap().len(), 1);

        let matrix = Media::from_json(&json!({"id": "tt0133093", "type": "movie", "name": "The Matrix"}), CINEMETA).unwrap();
        rt.block_on(core_a.execute(json!({"op": "forget", "type": "movie", "id": "tt0133093", "addon_url": CINEMETA}))).unwrap();
        remote_db::queue_media_forget_tombstone(&matrix);
        remote_db::sync_profile_remote_with_core("vault_1", "default", Some(&core_a)).unwrap();

        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_b.clone());
        remote_db::sync_profile_remote_with_core("vault_1", "default", Some(&core_b)).unwrap();
        let lib_b = rt.block_on(core_b.execute(json!({"op": "library"}))).unwrap();
        assert!(lib_b["items"].as_array().unwrap().is_empty(), "fast-clock peer must apply causal forget tombstone");

        // Verify extreme drift (>24h) is clamped in `meta` while still advancing per-row HLC on mutation.
        rstream_rclone::put_remote_bytes(
            "vault_1:",
            "RStream/.profiles/default/events/peer99/999999999999999-999999-ffffffffffffffff.jsonl",
            format!(
                "{}\n",
                json!({
                    "op": "media",
                    "hlc": "999999999999999-000005-ffffffffffffffff",
                    "key": "movie|tt0468569",
                    "item": {"type": "movie", "id": "tt0468569", "addon_url": CINEMETA, "meta": {"id": "tt0468569", "type": "movie", "name": "TDK"}, "saved": true, "updated": 100, "deleted": false}
                })
            )
            .as_bytes(),
        )
        .unwrap();
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_a.clone());
        remote_db::sync_profile_remote_with_core("vault_1", "default", Some(&core_a)).unwrap();
        {
            let conn_a = rusqlite::Connection::open(root_a.join("downloads.sqlite")).unwrap();
            let clamped_ms: u64 = conn_a
                .query_row("SELECT v FROM meta WHERE k = 'hlc_ms'", [], |r| r.get::<_, String>(0))
                .unwrap()
                .parse()
                .unwrap();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
            assert!(clamped_ms <= now + 86_400_000 + 10_000, "global HLC must clamp 24h drift, got {clamped_ms}");
        }

        // 2. `(remote, profile)` isolation + multi-remote full re-sync + `purge_disconnected_remote`.
        let core_alice = Core::open(base.join("core_alice")).unwrap();
        let core_bob = Core::open(base.join("core_bob")).unwrap();
        // Seed >5000 items into `core_alice` to ALSO verify `sync_changes` pagination (>5000 media limit)!
        let alice_items: Vec<Value> = (1..=5_050u32)
            .map(|i| {
                let id = format!("tt{:07}", 1_000_000 + i);
                json!({
                    "type": "movie",
                    "id": id,
                    "addon_url": CINEMETA,
                    "meta": {"id": id, "type": "movie", "name": format!("Film {i}")},
                    "saved": true,
                    "watched": false,
                    "position_ms": 0,
                    "duration_ms": 0,
                    "watched_ms": 0,
                    "liked": 0,
                    "updated": 1_000 + i64::from(i / 500),
                })
            })
            .collect();
        rt.block_on(core_alice.execute(json!({"op": "sync_apply", "media": alice_items, "playback": []}))).unwrap();

        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_c.clone());
        write_private_file(&root_c.join("settings.json"), br#"{"sync_remote":"vault_1","sync_profile":"alice"}"#).unwrap();
        remote_db::sync_profile_remote_with_core("vault_1", "alice", Some(&core_alice)).unwrap();

        // Switch same device (`root_c`) to profile `"bob"` and trigger snapshot compaction (15 deltas):
        // `"bob"`'s snapshot must NEVER contain Alice's 5,050 movies!
        rt.block_on(core_bob.execute(json!({
            "op": "sync_apply",
            "media": [{"type": "movie", "id": "tt9999999", "addon_url": CINEMETA, "meta": {"id": "tt9999999", "type": "movie", "name": "Bob Movie"}, "saved": true, "updated": 500}],
            "playback": [],
        })))
        .unwrap();
        write_private_file(&root_c.join("settings.json"), br#"{"sync_remote":"vault_1","sync_profile":"bob"}"#).unwrap();
        for d in 0..15u32 {
            rstream_rclone::put_remote_bytes(
                "vault_1:",
                &format!("RStream/.profiles/bob/deltas/delta-000000000000100-{d:06}-peer.jsonl"),
                b"{\"op\":\"meta\",\"covered\":[]}\n",
            )
            .unwrap();
        }
        remote_db::sync_profile_remote_with_core("vault_1", "bob", Some(&core_bob)).unwrap();
        let bob_files = rstream_rclone::list_remote_files_recursive("vault_1:", "RStream/.profiles/bob/snapshots").unwrap();
        assert!(!bob_files.is_empty(), "expected compacted snapshot for bob");
        let snap_rel = format!("RStream/.profiles/bob/snapshots/{}", bob_files[0].name);
        let bob_snap = String::from_utf8_lossy(&rstream_rclone::get_remote_bytes("vault_1:", &snap_rel).unwrap()).into_owned();
        assert!(bob_snap.contains("tt9999999"), "bob snapshot must contain bob's movie");
        assert!(!bob_snap.contains("tt1000001"), "bob snapshot leaked alice's profile_kv_hlc history!");

        // Switch `sync_remote` to `"vault_2"` for `"alice"`: ALL 5,050 paginated items must sync to `"vault_2"`!
        write_private_file(&root_c.join("settings.json"), br#"{"sync_remote":"vault_2","sync_profile":"alice"}"#).unwrap();
        remote_db::sync_profile_remote_with_core("vault_2", "alice", Some(&core_alice)).unwrap();

        let verify_dir = base.join("verify_alice_v2");
        std::fs::create_dir_all(&verify_dir).unwrap();
        let core_verify = Core::open(&verify_dir).unwrap();
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(verify_dir.clone());
        remote_db::sync_profile_remote_with_core("vault_2", "alice", Some(&core_verify)).unwrap();
        let v2_lib = rt.block_on(core_verify.execute(json!({"op": "library"}))).unwrap();
        assert_eq!(v2_lib["items"].as_array().unwrap().len(), 5_050, "all 5,050 paginated items must sync to new remote");

        // `purge_disconnected_remote("vault_1")` cleans vault_1 rows while keeping vault_2 intact.
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(root_c.clone());
        remote_db::purge_disconnected_remote("vault_1");
        {
            let conn_c = rusqlite::Connection::open(root_c.join("downloads.sqlite")).unwrap();
            let v1_cnt: i64 = conn_c.query_row("SELECT COUNT(*) FROM profile_kv_hlc WHERE remote = 'vault_1'", [], |r| r.get(0)).unwrap();
            let v2_cnt: i64 = conn_c.query_row("SELECT COUNT(*) FROM profile_kv_hlc WHERE remote = 'vault_2'", [], |r| r.get(0)).unwrap();
            assert_eq!(v1_cnt, 0);
            assert_eq!(v2_cnt, 5_050);
        }

        // 3. Uncovered concurrent snapshots + failed `get_remote_bytes` aborting BEFORE compaction!
        for (i, mid) in [(100u32, "tt2000001"), (200u32, "tt2000002")] {
            let line = json!({
                "op": "media",
                "hlc": format!("000000000000{i}-000000-dev{i}"),
                "key": format!("movie|{mid}"),
                "item": {"type": "movie", "id": mid, "addon_url": CINEMETA, "meta": {"id": mid, "type": "movie", "name": mid}, "saved": true, "updated": i, "deleted": false}
            });
            rstream_rclone::put_remote_bytes(
                "vault_2:",
                &format!("RStream/.profiles/multi_snap/snapshots/base-000000000000{i}-000000-dev{i}.jsonl"),
                format!("{{\"op\":\"meta\",\"covered\":[]}}\n{line}\n").as_bytes(),
            )
            .unwrap();
        }
        let snap_dir = base.join("multi_snap_client");
        std::fs::create_dir_all(&snap_dir).unwrap();
        let core_snap = Core::open(&snap_dir).unwrap();
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(snap_dir.clone());
        remote_db::sync_profile_remote_with_core("vault_2", "multi_snap", Some(&core_snap)).unwrap();
        let snap_lib = rt.block_on(core_snap.execute(json!({"op": "library"}))).unwrap();
        assert_eq!(snap_lib["items"].as_array().unwrap().len(), 2, "both uncovered snapshots must be applied");

        // Write 40 event files under `abort_prof`, corrupt one underlying cipher file on `cloud_2`, and verify sync aborts before compaction!
        for idx in 1..=40u32 {
            let mid = format!("tt{:07}", 3_000_000 + idx);
            let ev = json!({
                "op": "media",
                "hlc": format!("000000000001000-{idx:06}-peer"),
                "key": format!("movie|{mid}"),
                "item": {"type": "movie", "id": mid, "addon_url": CINEMETA, "meta": {"id": mid, "type": "movie", "name": mid}, "saved": true, "updated": 100 + idx, "deleted": false}
            });
            rstream_rclone::put_remote_bytes(
                "vault_2:",
                &format!("RStream/.profiles/abort_prof/events/peer/000000000001000-{idx:06}-peer.jsonl"),
                format!("{ev}\n").as_bytes(),
            )
            .unwrap();
        }
        // Narrow search to the newest directory in `cloud_2/Vault` by finding a small cipher file (<600B).
        fn collect_small_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        collect_small_files(&p, out);
                    } else if p.metadata().map(|m| m.len() < 600).unwrap_or(false) {
                        out.push(p);
                    }
                }
            }
        }
        let mut candidates = Vec::new();
        collect_small_files(&cloud_2.join("Vault"), &mut candidates);
        candidates.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
        let target_cipher = candidates.last().cloned().expect("expected encrypted event file");
        let good_cipher = std::fs::read(&target_cipher).unwrap();
        std::fs::write(&target_cipher, b"corrupt").unwrap();

        let abort_dir = base.join("abort_client");
        std::fs::create_dir_all(&abort_dir).unwrap();
        let core_abort = Core::open(&abort_dir).unwrap();
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = Some(abort_dir.clone());
        assert!(
            remote_db::sync_profile_remote_with_core("vault_2", "abort_prof", Some(&core_abort)).is_err(),
            "corrupted remote event must abort sync instead of compacting partial state"
        );
        let deltas_after_err = rstream_rclone::list_remote_files_recursive("vault_2:", "RStream/.profiles/abort_prof/deltas").unwrap();
        assert!(deltas_after_err.is_empty(), "compaction must NOT run when get_remote_bytes fails");
        let events_after_err = rstream_rclone::list_remote_files_recursive("vault_2:", "RStream/.profiles/abort_prof/events").unwrap();
        assert_eq!(events_after_err.iter().filter(|f| !f.is_dir).count(), 40, "no event files may be deleted on failed sync");

        // Restore ciphertext -> retry must succeed, apply all 40 items, and compact into a delta!
        std::fs::write(&target_cipher, &good_cipher).unwrap();
        remote_db::sync_profile_remote_with_core("vault_2", "abort_prof", Some(&core_abort)).unwrap();
        let abort_lib = rt.block_on(core_abort.execute(json!({"op": "library"}))).unwrap();
        assert_eq!(abort_lib["items"].as_array().unwrap().len(), 40);
        let deltas_after_ok = rstream_rclone::list_remote_files_recursive("vault_2:", "RStream/.profiles/abort_prof/deltas").unwrap();
        assert_eq!(deltas_after_ok.len(), 1, "delta compaction must succeed once all 40 events read cleanly");

        let _ = rstream_rclone::delete_remote("vault_1");
        let _ = rstream_rclone::delete_remote("vault_2");
        *TEST_DOWNLOADS_ROOT.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&base);
    }
}
