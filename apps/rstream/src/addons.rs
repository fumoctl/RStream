//! Add-on management: Official OpenSubtitles v3 installed by default (unless removed by the user),
//! Cinemeta as the built-in metadata core, and user-supplied Stremio manifest URLs.
//! User-installed add-ons are never overwritten or removed automatically, and manifest URLs stay
//! private inside `addons.json` (never logged or echoed in errors).
use crate::backend::{self, exec, s};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::collections::HashMap;

const OPENSUBTITLES_URL: &str = "https://opensubtitles-v3.strem.io/manifest.json";
/// Opened in the browser by "Get a TMDB key".
pub const TMDB_KEY_PAGE: &str = "https://www.themoviedb.org/settings/api";

// ---------------------------------------------------------------- languages

/// A stream language: option key, display label, flag, release tags in filenames, and ISO 639
/// track codes (mpv's `alang` order: ISO 639-1, then -2/B and -2/T).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Language {
    pub key: &'static str,
    pub label: &'static str,
    pub flag: &'static str,
    tags: &'static [&'static str],
    pub codes: &'static [&'static str],
}

impl Language {
    /// Whether a source's label says it has this audio (flag, name or release tag). Subtitle
    /// languages do not count: flags after "Subs" or a tag after "Sub" ("Sub ITA").
    pub fn matches(&self, label: &str) -> bool {
        label.lines().any(|line| {
            let audio = line.find("Subs").map_or(line, |i| &line[..i]);
            if audio.contains(self.flag) { return true; }
            let words: Vec<&str> = line.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
            words.iter().enumerate().any(|(i, w)| self.tags.iter().any(|t| w.eq_ignore_ascii_case(t))
                && !(i > 0 && ["sub", "subs", "subbed", "subtitles"].iter().any(|s| words[i - 1].eq_ignore_ascii_case(s))))
        })
    }
}

macro_rules! lang { ($key:expr, $label:expr, $flag:expr, [$($tag:expr),*], [$($code:expr),*]) => {
    Language { key: $key, label: $label, flag: $flag, tags: &[$($tag),*], codes: &[$($code),*] }
}; }

pub const LANGUAGES: &[Language] = &[
    lang!("english", "English", "🇬🇧", ["ENG", "ENGLISH"], ["en", "eng"]),
    lang!("italian", "Italian", "🇮🇹", ["ITA", "ITALIAN", "ITALIANO"], ["it", "ita"]),
    lang!("spanish", "Spanish", "🇪🇸", ["SPA", "ESP", "SPANISH", "CASTELLANO", "ESPANOL"], ["es", "spa"]),
    lang!("latino", "Latino", "🇲🇽", ["LATINO", "LAT"], ["es-419", "es-mx", "es", "spa"]),
    lang!("french", "French", "🇫🇷", ["FRE", "FRA", "FRENCH", "VFF", "VF", "TRUEFRENCH"], ["fr", "fre", "fra"]),
    lang!("german", "German", "🇩🇪", ["GER", "DEU", "GERMAN"], ["de", "ger", "deu"]),
    lang!("portuguese", "Portuguese", "🇵🇹", ["POR", "PORTUGUESE", "DUBLADO"], ["pt", "por", "pt-br", "pob"]),
    lang!("dutch", "Dutch", "🇳🇱", ["DUT", "NLD", "DUTCH"], ["nl", "dut", "nld"]),
    lang!("polish", "Polish", "🇵🇱", ["POL", "POLISH", "PL"], ["pl", "pol"]),
    lang!("russian", "Russian", "🇷🇺", ["RUS", "RUSSIAN"], ["ru", "rus"]),
    lang!("ukrainian", "Ukrainian", "🇺🇦", ["UKR", "UKRAINIAN"], ["uk", "ukr"]),
    lang!("turkish", "Turkish", "🇹🇷", ["TUR", "TURKISH"], ["tr", "tur"]),
    lang!("greek", "Greek", "🇬🇷", ["GRE", "GREEK"], ["el", "gre", "ell"]),
    lang!("swedish", "Swedish", "🇸🇪", ["SWE", "SWEDISH"], ["sv", "swe"]),
    lang!("danish", "Danish", "🇩🇰", ["DAN", "DANISH"], ["da", "dan"]),
    lang!("norwegian", "Norwegian", "🇳🇴", ["NOR", "NORWEGIAN"], ["no", "nor", "nob", "nb"]),
    lang!("finnish", "Finnish", "🇫🇮", ["FIN", "FINNISH"], ["fi", "fin"]),
    lang!("czech", "Czech", "🇨🇿", ["CZE", "CES", "CZECH"], ["cs", "cze", "ces"]),
    lang!("hungarian", "Hungarian", "🇭🇺", ["HUN", "HUNGARIAN"], ["hu", "hun"]),
    lang!("romanian", "Romanian", "🇷🇴", ["ROM", "RON", "ROMANIAN"], ["ro", "rum", "ron"]),
    lang!("arabic", "Arabic", "🇸🇦", ["ARA", "ARABIC"], ["ar", "ara"]),
    lang!("hebrew", "Hebrew", "🇮🇱", ["HEB", "HEBREW"], ["he", "heb"]),
    lang!("hindi", "Hindi", "🇮🇳", ["HIN", "HINDI"], ["hi", "hin"]),
    lang!("japanese", "Japanese", "🇯🇵", ["JAP", "JPN", "JAPANESE"], ["ja", "jpn"]),
    lang!("korean", "Korean", "🇰🇷", ["KOR", "KOREAN"], ["ko", "kor"]),
    lang!("chinese", "Chinese", "🇨🇳", ["CHI", "CHN", "CHINESE", "MANDARIN"], ["zh", "chi", "zho"]),
];

pub fn language(key: &str) -> Option<&'static Language> { LANGUAGES.iter().find(|l| l.key == key) }

// ---------------------------------------------------------------- catalogue

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Curated {
    OpenSubtitles,
}

impl Curated {
    pub const ALL: [Curated; 1] = [Curated::OpenSubtitles];

    fn id(self) -> &'static str {
        match self {
            Curated::OpenSubtitles => "opensubtitles",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Curated::OpenSubtitles => "OpenSubtitles v3",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Curated::OpenSubtitles => "Official Stremio OpenSubtitles add-on with multi-language community subtitles.",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Curated::OpenSubtitles => "Installed by default, no account or API key needed",
        }
    }

    fn host(self) -> &'static str {
        match self {
            Curated::OpenSubtitles => "opensubtitles-v3.strem.io",
        }
    }

    pub fn manifest_url(self) -> &'static str {
        match self {
            Curated::OpenSubtitles => OPENSUBTITLES_URL,
        }
    }
}

fn removed_key(c: Curated) -> String { format!("{}:removed", c.id()) }

fn host_of(url: &str) -> String { url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default() }

// ---------------------------------------------------------------- the app's own record

/// addons.json: managed state (such as `opensubtitles:removed`) and add-on versions already seen.
#[derive(Default)]
struct Store { managed: HashMap<String, Value>, seen: HashMap<String, String> }

fn store_path() -> std::path::PathBuf { backend::backend_data_dir().join("addons.json") }

fn load_from(path: &std::path::Path) -> Store {
    let v: Value = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let map = |k: &str| v[k].as_object().cloned().unwrap_or_default();
    Store {
        managed: map("managed").into_iter().collect(),
        seen: map("seen").into_iter().filter_map(|(k, v)| v.as_str().map(|v| (k, v.to_owned()))).collect(),
    }
}

fn save_to(path: &std::path::Path, store: &Store) {
    let v = json!({"managed": store.managed, "seen": store.seen});
    let _ = backend::write_private_file(path, v.to_string().as_bytes());
}

fn load() -> Store { load_from(&store_path()) }

fn save(store: &Store) { save_to(&store_path(), store) }

/// The installed add-ons (core descriptors: the manifest plus `manifest_url`).
async fn installed() -> Vec<Value> {
    exec(json!({"op":"addons"})).await.ok().and_then(|v| v["addons"].as_array().cloned()).unwrap_or_default()
}

// ---------------------------------------------------------------- status for Settings

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub name: String,
    pub version: String,
    /// A newer version you have not applied yet.
    pub update: Option<String>,
    pub offers: String,
    /// The curated entry this is, when matched.
    pub curated: Option<Curated>,
    /// Installed add-on's link: kept inside the app for Update and Remove, never displayed.
    url: String,
}

impl Row {
    /// Custom user-installed add-ons and OpenSubtitles can be removed; Cinemeta is the built-in metadata core.
    pub fn removable(&self) -> bool { !self.url.contains("v3-cinemeta.strem.io") }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Overview {
    /// Curated entries with their installed row, if any.
    pub curated: Vec<(Curated, Option<Row>)>,
    /// Everything else that is installed.
    pub others: Vec<Row>,
    pub updates: usize,
}

fn parse_ver(version: &str) -> Vec<u64> {
    let v = version.trim().trim_start_matches('v');
    let end = v.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(v.len());
    v[..end].split('.').map_while(|p| p.parse().ok()).collect()
}

pub fn newer(latest: &str, current: &str) -> bool {
    let (a, b) = (parse_ver(latest), parse_ver(current));
    !a.is_empty() && a > b
}

fn seen_key(addon: &Value) -> String { format!("{}@{}", s(addon, "id"), host_of(&s(addon, "manifest_url"))) }

/// Installed add-ons with their versions; the first sighting of an add-on is recorded silently,
/// later a higher version shows as an update until you apply it.
pub async fn overview() -> Overview {
    let addons = installed().await;
    let mut store = load();
    let mut changed = false;
    let mut out = Overview::default();
    let managed_url = |c: Curated, store: &Store| store.managed.get(c.id()).map(|m| s(m, "url"));
    let mut rows = Vec::new();
    for a in &addons {
        if crate::backend::retired_addon(&s(a, "manifest_url")) { continue; }
        let version = s(a, "version");
        let key = seen_key(a);
        let update = match store.seen.get(&key) {
            Some(seen) if newer(&version, seen) => Some(version.clone()),
            Some(_) => None,
            None => { store.seen.insert(key, version.clone()); changed = true; None }
        };
        let url = s(a, "manifest_url");
        let curated = Curated::ALL.into_iter().find(|c| {
            managed_url(*c, &store).as_deref() == Some(url.as_str()) || host_of(&url) == c.host()
        });
        let offers = a["resources"].as_array().into_iter().flatten()
            .filter_map(|r| r.as_str().map(str::to_owned).or_else(|| r["name"].as_str().map(str::to_owned))).collect::<Vec<_>>().join(", ");
        rows.push(Row { name: s(a, "name"), version, update, offers, curated, url });
    }
    if changed { save(&store); }
    out.updates = rows.iter().filter(|r| r.update.is_some()).count();
    for c in Curated::ALL {
        out.curated.push((c, rows.iter().find(|r| r.curated == Some(c)).cloned()));
    }
    out.others = rows.into_iter().filter(|r| r.curated.is_none()).collect();
    out
}

/// Ask every installed add-on for its manifest again (new versions), then count updates.
pub async fn check_updates() -> usize { check().await.0 }

/// The check, with what to tell you: updates found, and add-ons that didn't answer.
pub async fn check() -> (usize, String) {
    let refreshed = exec(json!({"op":"refresh_addons"})).await.ok();
    let unreachable: Vec<String> = refreshed.iter().flat_map(|v| v["results"].as_array().cloned().unwrap_or_default())
        .filter(|r| r.get("error").is_some()).map(|r| s(&r["addon"], "name")).collect();
    let ov = overview().await;
    let total = ov.others.len() + ov.curated.iter().filter(|(_, r)| r.is_some()).count();
    let mut note = match ov.updates {
        0 => format!("All {total} add-ons are on their latest version."),
        1 => "1 update available: tap Update on it.".to_owned(),
        n => format!("{n} updates available: tap Update on each."),
    };
    if !unreachable.is_empty() { note.push_str(&format!(" Couldn't reach {}.", unreachable.join(", "))); }
    (ov.updates, note)
}

fn mark_seen(store: &mut Store, addon: &Value) {
    store.seen.insert(seen_key(addon), s(addon, "version"));
}

// ---------------------------------------------------------------- install, update, remove

static CHANGING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Install a curated add-on without touching or replacing any user-installed add-ons.
pub async fn install(c: Curated) -> Result<()> {
    let _one = CHANGING.lock().await;
    install_unlocked(c).await
}

async fn install_unlocked(c: Curated) -> Result<()> {
    let url = c.manifest_url();
    let addon = exec(json!({"op":"install_addon","url":url})).await
        .map_err(|_| anyhow::anyhow!("{} couldn't be installed. Check your connection and try again.", c.name()))?;
    let new_url = s(&addon, "manifest_url");
    let mut store = load();
    store.managed.insert(c.id().into(), json!({"url": new_url}));
    store.managed.remove(&removed_key(c));
    mark_seen(&mut store, &addon);
    save(&store);
    backend::forget_sources();
    Ok(())
}

pub async fn remove(row: &Row) -> Result<()> {
    let _one = CHANGING.lock().await;
    exec(json!({"op":"remove_addon","url":row.url})).await?;
    backend::forget_sources();
    if let Some(c) = row.curated {
        let mut store = load();
        store.managed.insert(removed_key(c), json!(true));
        save(&store);
    }
    Ok(())
}

/// Normalize a user-provided Stremio add-on URL (`stremio://` -> `https://`, appending
/// `/manifest.json` if omitted). Rejects non-HTTPS remote URLs; never echoes the URL in errors.
pub fn normalize_custom_addon_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    ensure!(!trimmed.is_empty(), "Paste a Stremio add-on manifest URL.");
    let normalized = if trimmed.as_bytes().get(..10).is_some_and(|p| p.eq_ignore_ascii_case(b"stremio://")) {
        format!("https://{}", &trimmed[10..])
    } else {
        trimmed.to_owned()
    };
    let mut parsed = url::Url::parse(&normalized).map_err(|_| anyhow::anyhow!("That doesn't look like a valid add-on URL."))?;
    let host = parsed.host_str().unwrap_or_default();
    let is_local = matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
    ensure!(parsed.scheme() == "https" || (parsed.scheme() == "http" && is_local),
        "Add-on links must start with https:// (or stremio://).");
    if !parsed.path().ends_with("/manifest.json") {
        let base = parsed.path().trim_end_matches('/');
        parsed.set_path(&format!("{base}/manifest.json"));
    }
    Ok(parsed.into())
}

/// Install any user-supplied Stremio add-on manifest URL. Never logs or echoes the URL.
pub async fn install_custom(raw: String) -> Result<String> {
    let url = normalize_custom_addon_url(&raw)?;
    let _one = CHANGING.lock().await;
    let addon = exec(json!({"op":"install_addon","url":url})).await
        .map_err(|_| anyhow::anyhow!("That add-on couldn't be installed. Check the link and your connection."))?;
    let mut store = load();
    mark_seen(&mut store, &addon);
    save(&store);
    backend::forget_sources();
    let name = s(&addon, "name");
    Ok(if name.is_empty() { "Custom add-on".into() } else { name })
}

/// Apply a newer version by fetching the add-on's manifest again.
pub async fn update(row: &Row) -> Result<()> {
    let _one = CHANGING.lock().await;
    if let Some(c) = row.curated { return install_unlocked(c).await; }
    let addon = exec(json!({"op":"install_addon","url":row.url})).await?;
    let mut store = load();
    mark_seen(&mut store, &addon);
    save(&store);
    backend::forget_sources();
    Ok(())
}

/// At start: ensure Official OpenSubtitles v3 is installed unless explicitly removed by the user,
/// never overwriting or removing user-installed add-ons, then check for updates.
pub async fn startup() -> usize {
    {
        let _one = CHANGING.lock().await;
        set_up().await;
    }
    check_updates().await
}

async fn set_up() {
    let addons = installed().await;
    let store = load();
    for c in Curated::ALL {
        let present = addons.iter().any(|a| host_of(&s(a, "manifest_url")) == c.host());
        if !present && !store.managed.contains_key(&removed_key(c)) {
            let _ = install_unlocked(c).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_contains_only_opensubtitles() {
        assert_eq!(Curated::ALL.len(), 1);
        let c = Curated::ALL[0];
        assert_eq!(c, Curated::OpenSubtitles);
        assert_eq!(c.id(), "opensubtitles");
        assert_eq!(c.name(), "OpenSubtitles v3");
        assert_eq!(c.host(), "opensubtitles-v3.strem.io");
        assert_eq!(c.manifest_url(), "https://opensubtitles-v3.strem.io/manifest.json");
        assert_eq!(removed_key(c), "opensubtitles:removed");
    }

    #[test]
    fn cinemeta_is_core_and_other_rows_are_removable() {
        let cinemeta = Row {
            name: "Cinemeta".into(),
            version: "3.0.0".into(),
            update: None,
            offers: "catalog, meta".into(),
            curated: None,
            url: "https://v3-cinemeta.strem.io/manifest.json".into(),
        };
        let opensubs = Row {
            name: "OpenSubtitles v3".into(),
            version: "1.0.0".into(),
            update: None,
            offers: "subtitles".into(),
            curated: Some(Curated::OpenSubtitles),
            url: OPENSUBTITLES_URL.into(),
        };
        assert!(!cinemeta.removable());
        assert!(opensubs.removable());
    }

    #[test]
    fn store_persists_opensubtitles_removed_flag() {
        let dir = std::env::temp_dir().join(format!("rstream-addons-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("addons.json");
        let mut store = Store::default();
        store.managed.insert(removed_key(Curated::OpenSubtitles), json!(true));
        save_to(&path, &store);
        let loaded = load_from(&path);
        assert_eq!(loaded.managed.get("opensubtitles:removed"), Some(&json!(true)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn languages_match_flags_and_release_tags() {
        let it = language("italian").unwrap();
        assert!(it.matches("Inception 2010 2160p\n🇬🇧 / 🇮🇹"));
        assert!(it.matches("Inception.2010.BDRemux.Ita.Eng.x265"));
        assert!(!it.matches("Inception.2010.2160p.WEB-DL.Italic.Subs"));
        assert!(!it.matches("Inception 2010 2160p 🇬🇧"));
        assert!(!it.matches("Film.2160p\n💾 79 GiB\n🇬🇧 / 🇯🇵 Subs / 🇬🇧 / 🇮🇹"));
        assert!(!it.matches("Film 2010 AC3 5.1 ENG 2160p sub ITA"));
        assert!(it.matches("Film (2010) AC3 5.1 ITA.ENG 2160p H265 sub NUita.eng"));
    }

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.0.16", "0.0.15"));
        assert!(newer("2.40.0", "2.38.1"));
        assert!(!newer("2.38.1", "2.38.1"));
        assert!(!newer("1.9", "1.10"));
    }

    #[test]
    fn normalizes_and_validates_custom_addon_urls() {
        assert_eq!(normalize_custom_addon_url("stremio://example.com/cfg").unwrap(), "https://example.com/cfg/manifest.json");
        assert_eq!(normalize_custom_addon_url("STREMIO://example.com/cfg?token=abc&lang=en").unwrap(), "https://example.com/cfg/manifest.json?token=abc&lang=en");
        assert_eq!(normalize_custom_addon_url("https://example.com/?key=1").unwrap(), "https://example.com/manifest.json?key=1");
        assert_eq!(normalize_custom_addon_url("https://example.com/manifest.json?key=1").unwrap(), "https://example.com/manifest.json?key=1");
        assert_eq!(normalize_custom_addon_url("https://example.com/manifest.json").unwrap(), "https://example.com/manifest.json");
        assert_eq!(normalize_custom_addon_url("http://127.0.0.1:7000/").unwrap(), "http://127.0.0.1:7000/manifest.json");
        assert_eq!(normalize_custom_addon_url("http://[::1]:7000/").unwrap(), "http://[::1]:7000/manifest.json");
        assert!(normalize_custom_addon_url("http://remote.example.com/manifest.json").is_err());
        assert!(normalize_custom_addon_url("not-a-url").is_err());
    }
}
