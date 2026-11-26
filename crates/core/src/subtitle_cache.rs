//! Bounded app-private subtitle files. Only filenames owned by this cache are removed.
use anyhow::{bail, ensure, Context, Result};
#[cfg(test)]
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const LIMIT: usize = 4 * 1024 * 1024;
const EXTENSIONS: [&str; 5] = ["vtt", "srt", "ass", "ssa", "txt"];
const BYTE_BUDGET: u64 = 64 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Validate UTF-8 / UTF-16 / Latin-1 subtitle text (non-empty, non-binary, non-HTML)
/// and return its canonical file extension (`"vtt"`, `"ass"`, `"ssa"`, or `"srt"`).
pub(crate) fn validate_subtitle(url: &str, bytes: &[u8]) -> Result<&'static str> {
    ensure!(!bytes.is_empty(), "Empty subtitle response");
    ensure!(bytes.len() <= LIMIT, "Subtitle file too large");

    let decoded = if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        decode_utf16(rest, u16::from_le_bytes)?
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        decode_utf16(rest, u16::from_be_bytes)?
    } else {
        let raw = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        ensure!(
            raw.iter()
                .all(|&b| b != 0 && (b >= 0x20 || matches!(b, b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))),
            "Subtitle contains binary data"
        );
        match std::str::from_utf8(raw) {
            Ok(s) => s.to_owned(),
            Err(_) => raw.iter().map(|&b| b as char).collect(),
        }
    };

    let trimmed = decoded.trim_start_matches('\u{feff}').trim_start();
    ensure!(!trimmed.is_empty(), "Subtitle has no readable text");

    let head: String = trimmed.chars().take(512).collect::<String>().to_ascii_lowercase();
    ensure!(
        !head.starts_with("<!doctype")
            && !head.starts_with("<html")
            && !(head.starts_with("<!--") && head.contains("<html")),
        "Subtitle response is HTML"
    );

    let url_ext = url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path()
                .rsplit_once('.')
                .map(|(_, ext)| ext.to_ascii_lowercase())
        })
        .unwrap_or_default();

    if head.starts_with("webvtt") {
        return Ok("vtt");
    }
    let first_section = trimmed
        .lines()
        .map(|l| l.trim_start_matches('\u{feff}').trim())
        .find(|l| !l.is_empty() && !l.starts_with(';'))
        .unwrap_or("");
    if first_section
        .get(..13)
        .is_some_and(|p| p.eq_ignore_ascii_case("[script info]"))
    {
        return Ok(if url_ext == "ssa" { "ssa" } else { "ass" });
    }
    Ok(match url_ext.as_str() {
        "ass" => "ass",
        "ssa" => "ssa",
        "vtt" => "vtt",
        _ => "srt",
    })
}

fn decode_utf16(bytes: &[u8], to_u16: fn([u8; 2]) -> u16) -> Result<String> {
    if bytes.is_empty() || bytes.len() % 2 != 0 {
        bail!("Invalid UTF-16 subtitle");
    }
    let units = bytes
        .chunks_exact(2)
        .map(|c| to_u16([c[0], c[1]]));
    let mut out = String::with_capacity(bytes.len() / 2);
    for ch in char::decode_utf16(units) {
        let ch = ch.map_err(|_| anyhow::anyhow!("Invalid UTF-16 subtitle"))?;
        ensure!(
            ch != '\0' && (!ch.is_ascii_control() || matches!(ch, '\t' | '\n' | '\r' | '\x0b' | '\x0c')),
            "Subtitle contains binary data"
        );
        out.push(ch);
    }
    Ok(out)
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
const TEMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Write beside the destination and replace it atomically. A concurrent export
/// that already opened the old file retains its complete inode, while a new
/// open sees the complete replacement. Temp names are private cache names.
fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(
        ".tmp-{:08x}-{:016x}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = TempFile(PathBuf::from(name));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp.0)?;
    file.write_all(bytes)?;
    drop(file);
    fs::rename(&temp.0, path)?;
    Ok(())
}

pub(crate) struct SubtitleCache {
    directory: PathBuf,
    budget: u64,
    max_age: Duration,
    lock: Mutex<()>,
}
impl SubtitleCache {
    pub fn new(root: &Path) -> Self {
        Self {
            directory: root.join("subtitles"),
            budget: BYTE_BUDGET,
            max_age: MAX_AGE,
            lock: Mutex::new(()),
        }
    }
    fn stem(url: &str) -> String {
        let hash = url.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        format!("{hash:016x}")
    }
    fn owned(path: &Path) -> bool {
        path.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            && path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| EXTENSIONS.contains(&ext))
    }
    fn metadata_path(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".expiry");
        PathBuf::from(name)
    }
    fn remove_entry(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        match fs::remove_file(Self::metadata_path(path)) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    fn entries(&self) -> Result<Vec<(PathBuf, u64, SystemTime)>> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if !Self::owned(&path) {
                // A crash can leave a temporary replacement. Do not remove a
                // recent file another Core instance might still be writing.
                if let Some((name, id)) = path.to_str().and_then(|s| s.rsplit_once(".tmp-")) {
                    let safe_id = id.split_once('-').is_some_and(|(pid, seq)| {
                        pid.len() == 8
                            && seq.len() == 16
                            && pid
                                .bytes()
                                .chain(seq.bytes())
                                .all(|b| b.is_ascii_hexdigit())
                    });
                    if safe_id
                        && Self::owned(Path::new(name))
                        && entry.metadata()?.modified()?.elapsed().unwrap_or_default()
                            > TEMP_MAX_AGE
                    {
                        fs::remove_file(&path)?;
                    }
                }
                // Clean only our own orphaned expiry files after interrupted deletion.
                if let Some(name) = path.to_str().and_then(|s| s.strip_suffix(".expiry")) {
                    let source = Path::new(name);
                    if Self::owned(source) && !source.exists() {
                        fs::remove_file(&path)?;
                    }
                }
                continue;
            }
            let meta = entry.metadata()?;
            let metadata_size = fs::metadata(Self::metadata_path(&entry.path()))
                .map(|m| m.len())
                .unwrap_or(0);
            entries.push((
                entry.path(),
                meta.len().saturating_add(metadata_size),
                meta.modified()?,
            ));
        }
        Ok(entries)
    }
    fn prune(&self, available: u64, protected: Option<&Path>) -> Result<()> {
        let mut entries = Vec::new();
        let mut total = 0u64;
        for (path, size, modified) in self.entries()? {
            if protected == Some(path.as_path()) {
                continue;
            }
            if modified.elapsed().unwrap_or_default() > self.max_age {
                Self::remove_entry(&path)?;
            } else {
                total = total.saturating_add(size);
                entries.push((path, size, modified));
            }
        }
        entries.sort_by_key(|e| e.2);
        for (path, size, _) in entries {
            if total <= available {
                break;
            }
            Self::remove_entry(&path)?;
            total = total.saturating_sub(size);
        }
        Ok(())
    }
    pub fn read(&self, url: &str) -> Option<(Vec<u8>, PathBuf, &'static str)> {
        // No prune here: freshness is checked per file below, and the budget is
        // enforced by store(). A lookup must not scan the directory.
        let _lock = self.lock.lock().ok()?;
        let stem = Self::stem(url);
        for extension in EXTENSIONS {
            let path = self.directory.join(format!("{stem}.{extension}"));
            // Legacy entries and interrupted writes have no trustworthy
            // freshness metadata. Keep them eligible for quota/clear, but
            // never reuse them as a network response.
            let Ok(marker) = fs::read_to_string(Self::metadata_path(&path)) else {
                continue;
            };
            let fresh = if marker == "never" {
                true
            } else {
                marker.parse::<u64>().ok().is_some_and(|expires| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .ok()
                        .is_some_and(|now| now.as_secs() < expires)
                })
            };
            if !fresh {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            // The age limit prune() applies, checked for this one file only.
            if meta
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > self.max_age)
            {
                let _ = Self::remove_entry(&path);
                continue;
            }
            if meta.len() == 0 || meta.len() > LIMIT as u64 {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(format) = validate_subtitle(url, &bytes) else {
                let _ = Self::remove_entry(&path);
                continue;
            };
            if let Ok(file) = fs::File::open(&path) {
                let _ = file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()));
            }
            return Some((bytes, path, format));
        }
        None
    }
    pub fn remove(&self, url: &str) {
        let Ok(_lock) = self.lock.lock() else {
            return;
        };
        for ext in EXTENSIONS {
            let path = self.directory.join(format!("{}.{ext}", Self::stem(url)));
            let _ = Self::remove_entry(&path);
        }
    }
    pub fn store(&self, url: &str, bytes: &[u8], expiry: Option<SystemTime>) -> Result<PathBuf> {
        let extension = validate_subtitle(url, bytes)?;
        ensure!(
            bytes.len() as u64 <= self.budget,
            "Subtitle cache budget exceeded"
        );
        let _lock = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("Subtitle cache lock poisoned"))?;
        fs::create_dir_all(&self.directory).context("Subtitle cache directory")?;
        let stem = Self::stem(url);
        let path = self.directory.join(format!("{stem}.{extension}"));
        let marker = match expiry {
            Some(time) => time
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string(),
            None => "never".to_owned(),
        };
        let needed = (bytes.len() as u64).saturating_add(marker.len() as u64);
        ensure!(needed <= self.budget, "Subtitle cache budget exceeded");
        self.prune(self.budget - needed, Some(&path))?;
        // Invalidate first so interrupted writes cannot reuse stale metadata.
        match fs::remove_file(Self::metadata_path(&path)) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        replace_file(&path, bytes).context("Subtitle cache write")?;
        fs::write(Self::metadata_path(&path), marker).context("Subtitle cache expiry write")?;
        // Remove a legacy wrong-extension file only after the replacement exists.
        for old in EXTENSIONS {
            if old != extension {
                let _ = Self::remove_entry(&self.directory.join(format!("{stem}.{old}")));
            }
        }
        Ok(path)
    }
    #[cfg(test)]
    pub fn stats(&self) -> Result<Value> {
        let _lock = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("Subtitle cache lock poisoned"))?;
        self.prune(self.budget, None)?;
        let entries = self.entries()?;
        Ok(
            json!({"files":entries.len(),"bytes":entries.iter().map(|e|e.1).sum::<u64>(),
            "max_bytes":self.budget,"max_age_days":self.max_age.as_secs()/86400}),
        )
    }
    #[cfg(test)]
    pub fn clear(&self) -> Result<()> {
        let _lock = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("Subtitle cache lock poisoned"))?;
        for (path, _, _) in self.entries()? {
            Self::remove_entry(&path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Data(PathBuf);
    impl Drop for Data {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn fixture() -> (Data, SubtitleCache) {
        let root = std::env::temp_dir().join(format!(
            "veylo-subtitle-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(!root.exists());
        fs::create_dir_all(&root).unwrap();
        let mut cache = SubtitleCache::new(&root);
        cache.budget = 35;
        cache.max_age = Duration::from_secs(30);
        (Data(root), cache)
    }
    #[test]
    fn byte_budget_evicts_oldest_and_clear_preserves_unowned_files() {
        let (_data, cache) = fixture();
        let old = cache
            .store("https://fixture/old", b"1234567890", None)
            .unwrap();
        fs::File::open(old)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(10)),
            )
            .unwrap();
        cache
            .store("https://fixture/recent", b"1234567890", None)
            .unwrap();
        cache
            .store("https://fixture/new", b"1234567890", None)
            .unwrap();
        assert!(cache.read("https://fixture/old").is_none());
        assert!(cache.read("https://fixture/recent").is_some());
        assert_eq!(cache.stats().unwrap()["bytes"], 30);
        fs::write(cache.directory.join("user-notes.txt"), b"keep").unwrap();
        cache.clear().unwrap();
        assert_eq!(cache.stats().unwrap()["files"], 0);
        assert!(cache.directory.join("user-notes.txt").exists());
    }
    #[test]
    fn bom_webvtt_has_correct_extension_and_replaces_legacy_entry() {
        let (_data, cache) = fixture();
        let url = "https://fixture/bom";
        let legacy = cache.store(url, b"old srt", None).unwrap();
        assert_eq!(legacy.extension().unwrap(), "srt");
        let content = b"\xef\xbb\xbfWEBVTT";
        let path = cache.store(url, content, None).unwrap();
        assert_eq!(path.extension().unwrap(), "vtt");
        assert_eq!(cache.read(url).unwrap().0, content);
        assert!(!legacy.exists());
    }
    #[test]
    fn replacing_a_file_preserves_an_already_open_export() {
        use std::io::Read;
        let (_data, cache) = fixture();
        let url = "https://fixture/replace";
        let path = cache.store(url, b"old file", None).unwrap();
        let mut opened = fs::File::open(&path).unwrap();
        cache.store(url, b"new file", None).unwrap();
        let mut old = Vec::new();
        opened.read_to_end(&mut old).unwrap();
        assert_eq!(old, b"old file");
        assert_eq!(fs::read(path).unwrap(), b"new file");
    }

    #[test]
    fn freshness_deadline_does_not_move_on_access_and_legacy_is_not_reused() {
        let (_data, cache) = fixture();
        let url = "https://fixture/deadline";
        let path = cache
            .store(
                url,
                b"subtitle",
                Some(SystemTime::now() + Duration::from_secs(3600)),
            )
            .unwrap();
        assert!(cache.read(url).is_some());
        fs::write(SubtitleCache::metadata_path(&path), "1").unwrap();
        assert!(cache.read(url).is_none());
        assert!(
            path.exists(),
            "stale file remains available for immediate export"
        );
        fs::remove_file(SubtitleCache::metadata_path(&path)).unwrap();
        assert!(
            cache.read(url).is_none(),
            "legacy files lack trusted expiry"
        );
        cache.clear().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn expired_files_are_not_reused() {
        let (_data, cache) = fixture();
        let path = cache
            .store("https://fixture/expired", b"subtitle", None)
            .unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(60)),
            )
            .unwrap();
        assert!(cache.read("https://fixture/expired").is_none());
        assert!(!path.exists());
    }

    #[test]
    fn caches_ass_ssa_utf16_and_latin1_and_rejects_html_or_binary() {
        let (_data, mut cache) = fixture();
        cache.budget = 4096;
        let ass = b"\xef\xbb\xbf  ; Script generated by Aegisub\r\n[script info]\r\nTitle: Anime\r\n[Events]\r\nDialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello\r\n";
        let ass_path = cache.store("https://fixture/anime", ass, None).unwrap();
        assert_eq!(ass_path.extension().unwrap(), "ass");
        assert_eq!(cache.read("https://fixture/anime").unwrap().2, "ass");

        let ssa_path = cache.store("https://fixture/anime.ssa?dl=1", b"[Script Info]\nScriptType: v4.00\n", None).unwrap();
        assert_eq!(ssa_path.extension().unwrap(), "ssa");

        let mut utf16 = vec![0xFF, 0xFE];
        for u in "[Script Info]\r\nTitle: UTF16\r\n".encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        let u16_path = cache.store("https://fixture/u16", &utf16, None).unwrap();
        assert_eq!(u16_path.extension().unwrap(), "ass");

        let latin1 = b"1\n00:00:01,000 --> 00:00:02,000\nCaf\xe9\n";
        let l1_path = cache.store("https://fixture/latin1", latin1, None).unwrap();
        assert_eq!(l1_path.extension().unwrap(), "srt");

        assert!(cache.store("https://fixture/html", b"  <!DOCTYPE html><html>wait</html>", None).is_err());
        assert!(cache.store("https://fixture/html2", b"<html>please wait</html>", None).is_err());
        assert!(cache.store("https://fixture/empty", b"   \r\n\t ", None).is_err());
        assert!(cache.store("https://fixture/bin", b"PK\x03\x04\x00\x00", None).is_err());
    }
}
