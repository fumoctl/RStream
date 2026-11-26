//! Sources: what the add-ons offer for a video, the Play order (quality, HDR, size, your audio
//! language), the probe that checks the best candidates, and the lists fetched ahead of Play.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind { Direct, Torrent, External }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedSource {
    pub entry_id: String,
    pub path: PathBuf,
    pub remote: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub addon: String,
    pub name: String,
    pub description: String,
    pub url: String,
    pub filename: String,
    /// Declared size in bytes, or the add-on's own size text; None when absent (never estimated).
    pub size: Option<String>,
    pub size_bytes: Option<u64>,
    pub declared_size: bool,
    /// The size written in the add-on's label (💾), apart from the declared one.
    pub labelled_bytes: Option<u64>,
    /// The torrent is a season pack (its name is a season, not an episode), even when the
    /// file name is the one episode that plays.
    pub pack: bool,
    pub quality: Option<&'static str>,
    pub hdr: Option<&'static str>,
    pub kind: SourceKind,
    pub info_hash: Option<String>,
    pub file_idx: Option<u32>,
    pub tracker_sources: Vec<String>,
    pub seeders: Option<u32>,
    pub binge_group: Option<String>,
    pub indexer: Option<String>,
    pub headers: Vec<(String, String)>,
    /// name, title, description and filename together: what the play order reads.
    pub label: String,
    /// No line of the release names the title being played (an add-on's loose match: "Call Me by
    /// Your Name" for "Your Name"): such sources go after every one that does.
    pub off_title: bool,
    /// The picture the file's own header declares, once Play's check has read it: its size and
    /// HDR then outrank what the label claims.
    pub picture: Option<crate::tracks::Picture>,
    /// Present when this stream option is a downloaded local file or cloud rclone archive.
    pub saved: Option<SavedSource>,
}

pub(super) fn find_quality(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    if t.contains("2160p") || t.contains("3840x2160") || t.contains("4k") || t.contains("uhd") { Some("4K") }
    else if t.contains("1440p") || t.contains("2560x1440") { Some("1440p") }
    else if t.contains("1080p") || t.contains("1920x1080") || t.contains("fhd") { Some("1080p") }
    else if t.contains("720p") || t.contains("1280x720") { Some("720p") }
    else if t.contains("576p") || t.contains("480p") { Some("SD") }
    else { None }
}

pub(super) fn find_hdr(text: &str) -> Option<&'static str> {
    match hdr_tier(text) { 3 => Some("HDR10+"), 2 => Some("HDR"), 1 => Some("DV"), _ => None }
}

/// "4.2 GiB": a size in bytes as the player shows it.
pub fn file_size_label(bytes: u64) -> String { binary_size(bytes as f64) }

pub(super) fn binary_size(bytes: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes; let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 { v /= 1024.0; i += 1; }
    if i >= 3 { format!("{v:.1} {}", units[i]) } else { format!("{v:.0} {}", units[i]) }
}

static_re!(hdr10_plus, r"(?i)\bHDR10(?:\+|[ ._-]?PLUS\b|P\b)");
static_re!(hdr_any, r"(?i)\b(?:HDR10|HDR|HLG)\b");
static_re!(dolby_vision, r"(?i)\b(?:DV|DOVI|DOLBY[ ._-]?VISION)\b");
static_re!(remux, r"(?i)REMUX\b");
static_re!(bluray, r"(?i)\b(?:BLU[ ._-]?RAY|BDRIP|BRRIP|BDREMUX|BD(?:25|50|66|100))\b");
static_re!(web_dl, r"(?i)\bWEB(?:[ ._-]?DL)?\b");
static_re!(web_rip, r"(?i)\bWEB[ ._-]?RIP\b");
static_re!(low_source, r"(?i)\b(?:HDTV|PDTV|DVDRIP|DVDSCR|HDCAM|CAM|TS|HDTS|TELESYNC|TC|SCREENER)\b");
// Filmed or copied in a cinema, or a pre-release screener: never Play's choice, whatever
// resolution the name claims ("1080p HDTS" is a camera copy at 1080p). A bare "TS" isn't here:
// it is also MPEG-TS, a container real releases use.
static_re!(cinema_copy, r"(?i)\b(?:HD[ ._-]?CAM|CAM[ ._-]?RIP|CAM|HD[ ._-]?TS|TELESYNC|HD[ ._-]?TC|TELECINE|DVD[ ._-]?SCR|SCREENER)\b");
static_re!(av1, r"(?i)\bAV1\b");
// Resolution or HDR made up after the fact: AI upscales and SDR regraded to HDR.
static_re!(upscaled, r"(?i)(?:\b|_)(?:AI[ ._-]?)?UPSCALE(?:D|S)?(?:\b|_)|\bUPSCL\b|\bAI[ ._-]?(?:ENHANCED|REMASTER(?:ED)?)\b|\bREGRADED?\b|\bSDR[ ._-]?TO[ ._-]?HDR\b|\bFAKE[ ._-]?HDR\b");
static_re!(hevc, r"(?i)\b(?:HEVC|[HX][ ._-]?265)\b");
static_re!(avc, r"(?i)\b(?:AVC|[HX][ ._-]?264)\b");
static_re!(episode_mark, r"(?i)\bS\d{1,2}[ ._-]?E\d{1,3}\b|\b\d{1,2}x\d{2,3}\b|\bEP?[ ._-]?\d{2,3}\b");
static_re!(season_pack, r"(?i)\bS\d{1,2}\b|\bSEASONS?[ ._-]?\d|\bCOMPLETE\b");
static_re!(html_tag, "<[^>]+>");
static_re!(multi_audio, r"(?i)\b(?:MULTI|DUAL)(?:[ ._-]?AUDIO)?\b|\bMULTi\b");
// Stereoscopic 3D release tags found in stream titles and filenames.
static_re!(stereo_video, r"(?i)\b(?:3[ .-]?D|(?:H(?:ALF)?|F(?:ULL)?)[ .-]?(?:SBS|OU|TAB)|SBS|TAB|SIDE[ .-]BY[ .-]SIDE|TOP[ .-](?:AND[ .-])?BOTTOM|OVER[ .-]UNDER|STEREOSCOPIC)\b");

pub(super) fn is_3d(label: &str) -> bool { stereo_video().is_match(&label.replace('_', ".")) }

static_re!(size_label, r"(?i)(?:💾\s*)?(\d[\d.,]*)\s*([KMGT]i?B)\b");
static_re!(seeders_label, r"👤\s*(\d[\d,]*)");
static_re!(indexer_label, r"⚙\u{fe0f}?[ \t]*([A-Za-z0-9._+-]+(?:[ \t]+[A-Za-z0-9._+-]+)*)");
static_re!(leading_bracket_group, r"^\s*\[([^\[\]]{1,32})\]");
static_re!(trailing_dash_group, r"-([A-Za-z0-9]{2,20})(?:\.[A-Za-z0-9]{2,4})?\s*$");

pub(super) fn parse_seeders(text: &str) -> Option<u32> {
    seeders_label().captures(text)?.get(1)?.as_str().replace(',', "").parse().ok()
}

pub(super) fn parse_indexer(text: &str) -> Option<String> {
    indexer_label()
        .captures(text)
        .and_then(|c| Some(c.get(1)?.as_str().trim().to_owned()))
        .filter(|s| !s.is_empty())
}

pub fn codec_label(text: &str) -> Option<&'static str> {
    if av1().is_match(text) { Some("AV1") }
    else if hevc().is_match(text) { Some("HEVC (x265)") }
    else if avc().is_match(text) { Some("AVC (x264)") }
    else { None }
}

pub fn is_dual_audio(text: &str) -> bool {
    multi_audio().is_match(text)
}

pub fn audio_tag_of(src: &Source) -> Option<String> {
    if is_dual_audio(&src.label) {
        return Some("Dual Audio".into());
    }
    crate::addons::LANGUAGES
        .iter()
        .find(|l| l.matches(&src.label))
        .map(|l| l.label.to_owned())
}

fn is_non_group_token(token: &str) -> bool {
    let lower = token.trim().to_ascii_lowercase();
    lower.is_empty()
        || matches!(
            lower.as_str(),
            "4k" | "2160p" | "1440p" | "1080p" | "720p" | "480p" | "bd" | "bdrip" | "bluray"
                | "web" | "web-dl" | "webrip" | "hevc" | "x265" | "h265" | "avc" | "x264" | "h264"
                | "av1" | "10bit" | "hdr" | "dv" | "dual-audio" | "dual audio" | "multi" | "batch"
                | "p2p" | "rd+" | "rd" | "dl" | "rip" | "aac" | "mp4" | "mkv"
        )
        || lower.starts_with("bd ")
        || lower.starts_with("sub")
        || lower.ends_with("-subs")
        || lower.ends_with("_subs")
}

pub fn release_group_of(src: &Source) -> Option<String> {
    for candidate in [src.description.as_str(), src.filename.as_str(), src.name.as_str()] {
        for line in candidate.lines() {
            if let Some(cap) = leading_bracket_group().captures(line) {
                let g = cap[1].trim();
                if !is_non_group_token(g) {
                    return Some(g.to_owned());
                }
            }
        }
    }
    for candidate in [src.filename.as_str(), src.description.lines().next().unwrap_or("")] {
        if let Some(cap) = trailing_dash_group().captures(candidate.trim()) {
            let g = cap[1].trim();
            if !is_non_group_token(g) {
                return Some(g.to_owned());
            }
        }
    }
    None
}

/// HDR10+ (3), HDR10/HDR/HLG (2), Dolby Vision alone (1), SDR (0); "DV HDR10" counts as HDR10.
pub(super) fn hdr_tier(text: &str) -> u8 {
    if hdr10_plus().is_match(text) { 3 } else if hdr_any().is_match(text) { 2 } else if dolby_vision().is_match(text) { 1 } else { 0 }
}
/// Remux (4), WEB-DL (3), WEBRip (2), unlabelled (1), HDTV/DVD/cam (0); a Blu-ray re-encode is None.
pub(super) fn source_tier(text: &str) -> Option<u8> {
    if remux().is_match(text) { Some(4) } else if bluray().is_match(text) { None } else if web_rip().is_match(text) { Some(2) }
    else if web_dl().is_match(text) { Some(3) } else if low_source().is_match(text) { Some(0) } else { Some(1) }
}
pub(super) fn codec_tier(text: &str) -> u8 { if av1().is_match(text) { 3 } else if hevc().is_match(text) { 2 } else if avc().is_match(text) { 1 } else { 0 } }

/// The size in the add-on's label ("💾 45.3 GB" preferred): text as written, and bytes.
pub(super) fn labelled_size(text: &str) -> Option<(String, u64)> {
    let all: Vec<_> = size_label().captures_iter(text).filter(|c| c[1].chars().any(|d| ('1'..='9').contains(&d))).collect();
    let c = all.iter().find(|c| c[0].starts_with('💾')).or(all.first())?;
    let raw = &c[1];
    let number: f64 = (if raw.contains('.') { raw.replace(',', "") } else { raw.replace(',', ".") }).parse().ok()?;
    let unit = c[2].to_uppercase();
    let power = "BKMGTPE".find(unit.chars().next()?).unwrap_or(0) as i32;
    let base: f64 = if unit.contains('I') { 1024.0 } else { 1000.0 };
    Some((format!("{} {}", raw, &c[2]), (number * base.powi(power)) as u64))
}

fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.');
            d.eq_ignore_ascii_case("localhost") || d.to_ascii_lowercase().ends_with(".localhost")
        }
        None => false,
    }
}

fn is_unspecified_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_unspecified() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_unspecified())
        }
        _ => false,
    }
}

fn is_loopback_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|u| is_loopback_host(&u))
}

fn allowed_direct_stream_url(raw: &str, remote_addon: bool) -> bool {
    let Ok(u) = url::Url::parse(raw) else { return false };
    if !matches!(u.scheme(), "https" | "http") || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    if is_unspecified_host(&u) {
        return false;
    }
    if is_loopback_host(&u) {
        // Port 11470 is Stremio Desktop's local torrent engine; remote addons may never target loopback.
        if remote_addon || u.port() == Some(11470) {
            return false;
        }
    }
    true
}

fn extract_magnet(v: &Value, url: &str) -> Option<String> {
    for candidate in [url, &s(v, "externalUrl")] {
        if candidate.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("magnet:"))
            && candidate.to_ascii_lowercase().contains("urn:btih:")
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

#[cfg(test)]
pub(super) fn source_of(addon: &str, v: &Value) -> Source {
    source_of_from(addon, None, v)
}

pub(super) fn source_of_from(addon: &str, addon_url: Option<&str>, v: &Value) -> Source {
    let hints = &v["behaviorHints"];
    let (name, description) = (s(v, "name"), [s(v, "description"), s(v, "title")].into_iter().find(|t| !t.is_empty()).unwrap_or_default());
    let filename = s(hints, "filename");
    let url = s(v, "url");
    let raw_hash = s(v, "infoHash");
    let info_hash = (!raw_hash.is_empty())
        .then_some(raw_hash)
        .or_else(|| extract_magnet(v, &url));
    let file_idx = v["fileIdx"].as_u64().and_then(|n| u32::try_from(n).ok());
    let tracker_sources = v["sources"].as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let remote_addon = addon_url.is_some_and(|u| !is_loopback_url(u));
    let direct_http = allowed_direct_stream_url(&url, remote_addon);
    let kind = if direct_http { SourceKind::Direct }
        else if info_hash.is_some() { SourceKind::Torrent } else { SourceKind::External };
    let label = format!("{name} {} {} {filename}", s(v, "title"), s(v, "description"));
    let headers = hints["proxyHeaders"]["request"].as_object()
        .map(|h| h.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned()))).collect()).unwrap_or_default();
    let declared = hints["videoSize"].as_u64().or_else(|| hints["videoSize"].as_str().and_then(|x| x.parse().ok())).filter(|b| *b > 0);
    let combined_text = format!("{}\n{}\n{}", s(v, "title"), s(v, "description"), s(v, "name"));
    let labelled = labelled_size(&combined_text);
    let seeders = parse_seeders(&combined_text);
    let indexer = parse_indexer(&combined_text);
    let binge_group = {
        let bg = s(hints, "bingeGroup");
        (!bg.is_empty()).then_some(bg)
    };
    // Add-ons write the torrent's name on its own line (the file name, if any, on another).
    let pack = format!("{name}\n{}\n{}", s(v, "title"), s(v, "description")).lines()
        .any(|line| season_pack().is_match(line) && !episode_mark().is_match(line))
        || (!episode_mark().is_match(&label) && season_pack().is_match(&label));
    Source {
        addon: addon.to_owned(), name, description, filename,
        size: declared.map(|b| binary_size(b as f64)).or_else(|| labelled.as_ref().map(|l| l.0.clone())),
        size_bytes: declared.or(labelled.as_ref().map(|l| l.1)),
        declared_size: declared.is_some(),
        labelled_bytes: labelled.as_ref().map(|l| l.1),
        pack,
        quality: find_quality(&label), hdr: find_hdr(&label), kind, info_hash, file_idx, tracker_sources,
        seeders, binge_group, indexer, headers,
        url: if kind == SourceKind::Direct { url } else { String::new() },
        label,
        off_title: false,
        picture: None,
        saved: None,
    }
}

impl Source {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "addon": self.addon,
            "name": self.name,
            "description": self.description,
            "url": self.url,
            "filename": self.filename,
            "size": self.size,
            "size_bytes": self.size_bytes,
            "declared_size": self.declared_size,
            "labelled_bytes": self.labelled_bytes,
            "pack": self.pack,
            "quality": self.quality,
            "hdr": self.hdr,
            "kind": match self.kind {
                SourceKind::Direct => "direct",
                SourceKind::Torrent => "torrent",
                SourceKind::External => "external",
            },
            "info_hash": self.info_hash,
            "file_idx": self.file_idx,
            "tracker_sources": self.tracker_sources,
            "seeders": self.seeders,
            "binge_group": self.binge_group,
            "indexer": self.indexer,
            "headers": self.headers,
            "label": self.label,
            "saved": self.saved.as_ref().map(|sv| json!({
                "entry_id": sv.entry_id,
                "path": sv.path.to_string_lossy(),
                "remote": sv.remote,
            })),
        })
    }

    pub(crate) fn from_json(v: &Value) -> Option<Self> {
        let kind = match v["kind"].as_str()? {
            "direct" => SourceKind::Direct,
            "torrent" => SourceKind::Torrent,
            "external" => SourceKind::External,
            _ => return None,
        };
        let label = s(v, "label");
        let quality = match v["quality"].as_str() {
            Some("4K") => Some("4K"),
            Some("1440p") => Some("1440p"),
            Some("1080p") => Some("1080p"),
            Some("720p") => Some("720p"),
            Some("SD") => Some("SD"),
            Some("SAVED") => Some("SAVED"),
            _ => find_quality(&label),
        };
        let hdr = match v["hdr"].as_str() {
            Some("HDR10+") => Some("HDR10+"),
            Some("HDR") => Some("HDR"),
            Some("DV") => Some("DV"),
            _ => find_hdr(&label),
        };
        let headers = v["headers"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|pair| {
                        let p = pair.as_array()?;
                        Some((p.first()?.as_str()?.to_owned(), p.get(1)?.as_str()?.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let tracker_sources = v["tracker_sources"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let saved = v.get("saved").filter(|x| x.is_object()).and_then(|sv| {
            let path = s(sv, "path");
            if path.is_empty() {
                return None;
            }
            Some(SavedSource {
                entry_id: s(sv, "entry_id"),
                path: PathBuf::from(path),
                remote: sv["remote"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            })
        });
        Some(Source {
            addon: s(v, "addon"),
            name: s(v, "name"),
            description: s(v, "description"),
            url: s(v, "url"),
            filename: s(v, "filename"),
            size: v["size"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            size_bytes: v["size_bytes"].as_u64(),
            declared_size: v["declared_size"].as_bool().unwrap_or(false),
            labelled_bytes: v["labelled_bytes"].as_u64(),
            pack: v["pack"].as_bool().unwrap_or(false),
            quality,
            hdr,
            kind,
            info_hash: v["info_hash"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            file_idx: v["file_idx"].as_u64().and_then(|n| u32::try_from(n).ok()),
            tracker_sources,
            seeders: v["seeders"].as_u64().and_then(|n| u32::try_from(n).ok()),
            binge_group: v["binge_group"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            indexer: v["indexer"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            headers,
            label,
            off_title: false,
            picture: None,
            saved,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamMatcherLock {
    pub addon: String,
    pub kind: SourceKind,
    pub binge_group: Option<String>,
    pub info_hash: Option<String>,
    pub is_pack: bool,
    pub tracker_sources: Vec<String>,
    pub quality: Option<String>,
    pub release_group: Option<String>,
    pub codec: Option<String>,
    pub dual_audio: bool,
    pub audio_lang: Option<String>,
    pub episode_bytes: Option<u64>,
}

impl StreamMatcherLock {
    pub fn from_source(src: &Source) -> Self {
        Self::from_source_with_pack(src, None)
    }

    pub fn from_source_with_pack(src: &Source, pack_episodes: Option<usize>) -> Self {
        let audio_lang = crate::addons::LANGUAGES
            .iter()
            .find(|l| l.matches(&src.label))
            .map(|l| l.key.to_owned());
        Self {
            addon: src.addon.clone(),
            kind: src.kind,
            binge_group: src.binge_group.clone(),
            info_hash: src.info_hash.clone(),
            is_pack: src.pack,
            tracker_sources: src.tracker_sources.clone(),
            quality: src.quality.map(str::to_owned),
            release_group: release_group_of(src),
            codec: codec_label(&src.label).map(str::to_owned),
            dual_audio: is_dual_audio(&src.label),
            audio_lang,
            episode_bytes: episode_size(src, pack_episodes),
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "addon": self.addon,
            "kind": match self.kind {
                SourceKind::Direct => "direct",
                SourceKind::Torrent => "torrent",
                SourceKind::External => "external",
            },
            "binge_group": self.binge_group,
            "info_hash": self.info_hash,
            "is_pack": self.is_pack,
            "tracker_sources": self.tracker_sources,
            "quality": self.quality,
            "release_group": self.release_group,
            "codec": self.codec,
            "dual_audio": self.dual_audio,
            "audio_lang": self.audio_lang,
            "episode_bytes": self.episode_bytes,
        })
    }

    pub(crate) fn from_json(v: &Value) -> Option<Self> {
        let kind = match v["kind"].as_str()? {
            "direct" => SourceKind::Direct,
            "torrent" => SourceKind::Torrent,
            "external" => SourceKind::External,
            _ => return None,
        };
        let tracker_sources = v["tracker_sources"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        Some(Self {
            addon: s(v, "addon"),
            kind,
            binge_group: v["binge_group"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            info_hash: v["info_hash"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            is_pack: v["is_pack"].as_bool().unwrap_or(false),
            tracker_sources,
            quality: v["quality"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            release_group: v["release_group"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            codec: v["codec"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            dual_audio: v["dual_audio"].as_bool().unwrap_or(false),
            audio_lang: v["audio_lang"].as_str().filter(|x| !x.is_empty()).map(str::to_owned),
            episode_bytes: v["episode_bytes"].as_u64().filter(|b| *b > 0),
        })
    }
}

fn lock_similarity_rank(lock: &StreamMatcherLock, c: &Source) -> (i32, std::cmp::Reverse<u8>, u64, u32) {
    let mut score: i32 = 0;
    if c.kind == lock.kind { score += 15; }
    if !lock.addon.is_empty() && c.addon.eq_ignore_ascii_case(&lock.addon) { score += 20; }
    if lock.quality.as_deref().is_some_and(|q| c.quality == Some(q)) { score += 30; }
    if let Some(rg) = lock.release_group.as_deref() {
        if release_group_of(c).as_deref().is_some_and(|cg| cg.eq_ignore_ascii_case(rg)) {
            score += 45;
        }
    }
    if lock.codec.as_deref().is_some_and(|cd| codec_label(&c.label) == Some(cd)) {
        score += 20;
    }
    if lock.dual_audio == is_dual_audio(&c.label) { score += 15; }
    if lock.audio_lang.as_deref().and_then(crate::addons::language).is_some_and(|l| l.matches(&c.label)) {
        score += 10;
    }
    let cand_ep_bytes = episode_size(c, None);
    if let (Some(ref_b), Some(cand_b)) = (lock.episode_bytes.filter(|b| *b > 0), cand_ep_bytes) {
        let ratio = (cand_b.max(ref_b) as f64) / (cand_b.min(ref_b) as f64);
        if ratio <= 1.5 {
            score += 30;
        } else if ratio <= 2.0 {
            score += 12;
        } else if ratio >= 2.5 {
            score -= 35;
        }
    }
    if c.seeders == Some(0) { score -= 60; }
    let size_tiebreak = match (lock.episode_bytes.filter(|b| *b > 0), cand_ep_bytes) {
        (Some(ref_b), Some(cand_b)) => u64::MAX - cand_b.abs_diff(ref_b),
        _ => cand_ep_bytes.unwrap_or(0),
    };
    (score, std::cmp::Reverse(seeder_tier(c.seeders)), size_tiebreak, c.seeders.unwrap_or(0))
}

/// Match an episode's stream candidates against the user's chosen `StreamMatcherLock`:
/// (1) exact `bingeGroup`, else (2) same `infoHash` pack (or reuse the reference pack's `infoHash`
/// with `file_idx: None` when no sibling release from the same group is listed), else (3) strong
/// release-group or `score >= 85` similarity match with non-conflicting quality/language.
pub fn match_episode_source(lock: &StreamMatcherLock, candidates: &[Source]) -> Option<Source> {
    let global_lang = stream_language();
    let playable: Vec<&Source> = candidates
        .iter()
        .filter(|c| {
            let valid_kind = c.kind == SourceKind::Direct
                || (c.kind == SourceKind::Torrent && c.info_hash.is_some());
            let bad_file = !c.filename.is_empty() && non_video_filename(&c.filename);
            let tiny_sample = sample_clip().is_match(&c.label)
                && c.size_bytes.is_some_and(|b| b < 200_000_000);
            let matches_locked_lang = lock
                .audio_lang
                .as_deref()
                .and_then(crate::addons::language)
                .is_some_and(|l| l.matches(&c.label));
            let foreign_only = !matches_locked_lang
                && global_lang
                    .as_deref()
                    .is_some_and(|lang| other_language_only(&c.label, lang));
            c.saved.is_none()
                && valid_kind
                && !is_3d(&c.label)
                && !c.off_title
                && !cinema_copy().is_match(&c.label)
                && !bad_file
                && !tiny_sample
                && c.seeders != Some(0)
                && !foreign_only
        })
        .collect();

    if let Some(bg) = lock.binge_group.as_deref() {
        if let Some(hit) = playable
            .iter()
            .filter(|c| c.binge_group.as_deref() == Some(bg))
            .min_by_key(|c| (!c.addon.eq_ignore_ascii_case(&lock.addon), seeder_tier(c.seeders)))
        {
            return Some((*hit).clone());
        }
    }

    if let Some(hash) = lock.info_hash.as_deref() {
        if let Some(hit) = playable
            .iter()
            .filter(|c| c.info_hash.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(hash)))
            .min_by_key(|c| seeder_tier(c.seeders))
        {
            return Some((*hit).clone());
        }
        let has_same_group_candidate = lock.release_group.as_deref().is_some_and(|rg| {
            playable.iter().any(|c| {
                release_group_of(c).as_deref().is_some_and(|cg| cg.eq_ignore_ascii_case(rg))
            })
        });
        if lock.is_pack && lock.kind == SourceKind::Torrent && !has_same_group_candidate {
            let quality = match lock.quality.as_deref() {
                Some("4K") => Some("4K"),
                Some("1440p") => Some("1440p"),
                Some("1080p") => Some("1080p"),
                Some("720p") => Some("720p"),
                Some("SD") => Some("SD"),
                _ => None,
            };
            return Some(Source {
                addon: lock.addon.clone(),
                name: lock.addon.clone(),
                description: "Season pack".into(),
                url: String::new(),
                filename: String::new(),
                size: None,
                size_bytes: None,
                declared_size: false,
                labelled_bytes: None,
                pack: true,
                quality,
                hdr: None,
                kind: SourceKind::Torrent,
                info_hash: Some(hash.to_owned()),
                file_idx: None,
                tracker_sources: lock.tracker_sources.clone(),
                seeders: None,
                binge_group: lock.binge_group.clone(),
                indexer: None,
                headers: Vec::new(),
                label: format!("{} {}", lock.addon, lock.quality.as_deref().unwrap_or("")),
                off_title: false,
                picture: None,
                saved: None,
            });
        }
    }

    playable
        .into_iter()
        .filter(|c| {
            let quality_ok = match (lock.quality.as_deref(), c.quality) {
                (Some(lq), Some(cq)) => lq == cq,
                _ => true,
            };
            let lang_ok = lock
                .audio_lang
                .as_deref()
                .is_none_or(|la| !other_language_only(&c.label, la));
            let same_group = lock.release_group.as_deref().is_some_and(|rg| {
                release_group_of(c)
                    .as_deref()
                    .is_some_and(|cg| cg.eq_ignore_ascii_case(rg))
            });
            let (score, ..) = lock_similarity_rank(lock, c);
            quality_ok && lang_ok && (same_group || score >= 85)
        })
        .max_by_key(|c| lock_similarity_rank(lock, c))
        .cloned()
}

pub(super) fn quality_rank(q: Option<&str>) -> u8 { match q { Some("4K") => 0, Some("1440p") => 1, Some("1080p") => 2, Some("720p") => 3, Some("SD") => 4, _ => 5 } }

/// Every add-on's sources for one video, best declared quality first; add-on order kept within a quality.
/// Direct HTTPS streams are ordered ahead of P2P and external sources.
pub async fn sources(kind: &str, video_id: &str) -> Result<(Vec<Source>, Vec<String>)> {
    let mut saved = remote_db::saved_sources_for(kind, video_id);
    match sources_within(kind, video_id, None, None).await {
        Ok((list, failed, _)) => {
            saved.extend(list.into_iter().filter(|s| s.saved.is_none()));
            Ok((saved, failed))
        }
        Err(_) if !saved.is_empty() => Ok((saved, Vec::new())),
        Err(e) => Err(e),
    }
}

/// How long Play waits for the slower add-ons once one has answered with sources it can play.
pub(super) const ADDON_GRACE: Duration = Duration::from_secs(6);

/// `sources`, but with `grace` the answer comes that long after the first add-on with a playable
/// source, without waiting for an add-on that is slow or down (the core waits up to 30 s each).
/// `early` gets each add-on's sources as they come, while others are still answering.
/// The flag says whether every add-on asked answered (false when `grace` left slower ones out).
pub(super) async fn sources_within(kind: &str, video_id: &str, grace: Option<Duration>, early: Option<tokio::sync::mpsc::UnboundedSender<Vec<Source>>>) -> Result<(Vec<Source>, Vec<String>, bool)> {
    let targets = exec(json!({"op":"streams_targets","type":kind,"id":video_id})).await?;
    let targets: Vec<Value> = targets["targets"].as_array().cloned().unwrap_or_default();
    let (mut out, failed, complete) = ask_addons(kind, video_id, targets, grace, early).await?;
    out.sort_by_key(|x| (
        x.kind != SourceKind::Direct,
        x.kind == SourceKind::External,
        quality_rank(x.quality),
        seeder_tier(x.seeders),
        std::cmp::Reverse(x.seeders.unwrap_or(0)),
    ));
    Ok((out, failed, complete))
}

/// One round of `sources_within`: these add-ons' sources (2D only), the ones that failed, and
/// whether all of them answered.
pub(super) async fn ask_addons(kind: &str, video_id: &str, targets: Vec<Value>, grace: Option<Duration>, early: Option<tokio::sync::mpsc::UnboundedSender<Vec<Source>>>) -> Result<(Vec<Source>, Vec<String>, bool)> {
    let (kind, id) = (kind.to_owned(), video_id.to_owned());
    let (groups, complete) = on_runtime(async move {
        let mut jobs = tokio::task::JoinSet::new();
        for (i, t) in targets.into_iter().enumerate() {
            let request = json!({"op":"streams","type":kind,"id":id,"addon_url":t["addon_url"]});
            jobs.spawn(async move { (i, s(&t, "addon_name"), backend().core.execute(request).await) });
        }
        let mut groups: Vec<(usize, Value)> = Vec::new();
        let mut deadline: Option<tokio::time::Instant> = None;
        let mut complete = true;
        loop {
            let next = match deadline {
                Some(at) => match tokio::time::timeout_at(at, jobs.join_next()).await { Ok(n) => n, Err(_) => { complete = jobs.is_empty(); break } },
                None => jobs.join_next().await,
            };
            let Some(done) = next else { break };
            let Ok((i, name, answer)) = done else { continue };
            match answer {
                Ok(v) => for g in v["groups"].as_array().into_iter().flatten() {
                    let addon = s(g, "addon_name");
                    let addon_url = s(g, "addon_url");
                    let remote_addon = !is_loopback_url(&addon_url);
                    let playable = g["streams"].as_array().into_iter().flatten()
                        .any(|st| {
                            let u = s(st, "url");
                            let ext = s(st, "externalUrl");
                            allowed_direct_stream_url(&u, remote_addon)
                                || !s(st, "infoHash").is_empty()
                                || u.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("magnet:"))
                                || ext.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("magnet:"))
                        });
                    if let (Some(grace), None, true) = (grace, deadline, playable) { deadline = Some(tokio::time::Instant::now() + grace); }
                    if let Some(tx) = early.as_ref().filter(|_| playable && !jobs.is_empty()) {
                        let _ = tx.send(g["streams"].as_array().into_iter().flatten().map(|st| source_of_from(&addon, Some(&addon_url), st)).filter(|s| !is_3d(&s.label)).collect());
                    }
                    groups.push((i, g.clone()));
                },
                Err(_) => groups.push((i, json!({"addon_name": name, "streams": [], "error": "unavailable"}))),
            }
        }
        // Add-ons still answering are left out (dropping the set cancels them).
        groups.sort_by_key(|(i, _)| *i);
        (groups.into_iter().map(|(_, g)| g).collect::<Vec<Value>>(), complete)
    }).await?;
    let mut out = Vec::new();
    let mut failed = Vec::new();
    for g in &groups {
        let addon = s(g, "addon_name");
        let addon_url = s(g, "addon_url");
        if g.get("error").is_some() { failed.push(addon.clone()); }
        out.extend(g["streams"].as_array().into_iter().flatten().map(|st| source_of_from(&addon, Some(&addon_url), st)).filter(|s| !is_3d(&s.label)));
    }
    Ok((out, failed, complete))
}


/// Size of the one file that plays: a season pack's size is split per episode ONLY when no
/// individual `file_idx` is selected and the declared size matches the whole-pack label.
pub(super) fn episode_size(src: &Source, pack_episodes: Option<usize>) -> Option<u64> {
    let size = src.size_bytes.filter(|b| *b > 0)?;
    if !src.pack || src.file_idx.is_some() {
        return Some(size);
    }
    let Some(n) = pack_episodes.filter(|n| *n > 1) else { return Some(size) };
    if src.declared_size {
        if let Some(label) = src.labelled_bytes.filter(|l| *l > 0) {
            if (size as f64 - label as f64).abs() / label as f64 > 0.1 { return Some(size); }
        }
    }
    Some(size / n as u64)
}

/// Play order: 2D direct sources of 1080p or more only (unlabelled ones after every labelled one);
/// HDR before SDR, then 4K > 1440p > 1080p (a file too small for its resolution counts as a
/// 1080p, see `picture_class`), then biggest file (per episode for season packs),
/// remux > WEB-DL > WEBRip > other, HDR10+ > HDR > SDR, then AV1 > H.265 > H.264.
/// Dolby-Vision-only files (usually profile 5, wrong colours without a DV decoder) come last;
/// AI upscales and SDR regraded to HDR after every native file.
/// Torrents, external links and anything under 1080p never.
pub fn play_order(sources: &[Source], pack_episodes: Option<usize>) -> Vec<Source> {
    let language = stream_language();
    play_order_for(sources, pack_episodes, language.as_deref())
}

pub(super) static STREAM_LANGUAGE: Mutex<Option<String>> = Mutex::new(None);
pub(super) fn set_stream_language(language: Option<String>) {
    if let Ok(mut l) = STREAM_LANGUAGE.lock() { *l = language; }
}

/// The preferred audio language (a `crate::addons::LANGUAGES` key), None for any.
pub fn stream_language() -> Option<String> { STREAM_LANGUAGE.lock().ok().and_then(|l| l.clone()) }

/// The language codes a file's tracks use for an audio-language setting, in mpv's `alang`
/// order (ISO 639-1, then -2/B and -2/T).
pub fn track_codes(key: &str) -> &'static [&'static str] {
    crate::addons::language(key).map_or(&[], |l| l.codes)
}

/// Whether a track's language tag is the setting's language ("ita", "it", "it-IT" for Italian).
/// Also for subtitle add-on codes: "pob" is Portuguese, "spl" Latin-American Spanish.
pub fn track_in_language(code: &str, key: &str) -> bool {
    let code = code.trim().to_lowercase();
    if code.is_empty() { return false; }
    let base = code.split(['-', '_']).next().unwrap_or("");
    let named = crate::engine::language_name(&code);
    track_codes(key).iter().any(|c| *c == code || (!c.contains('-') && *c == base && key != "latino") || (key == "latino" && code.starts_with("es-") && code != "es-es"))
        || named == subtitle_language_of(key)
        || (key == "portuguese" && named == "Portuguese (Brazil)")
}

/// `play_order` with a preferred audio language, in three tiers, each in the usual order:
/// sources tagged with it (flag, name or release tag, multi-audio included); then untagged or
/// multi-language ones; then those tagged only with other languages (an Italian-only release
/// when you chose English). English is mostly untagged, so for English the first two are one.
/// Within a tier: higher-quality direct sources come first; a link marked as a trial goes
/// last, as it may stop partway.
pub fn play_order_for(sources: &[Source], pack_episodes: Option<usize>, language: Option<&str>) -> Vec<Source> {
    let lang = language.and_then(crate::addons::language);
    let mut out: Vec<Source> = sources.iter().filter(|s| playable(s)).cloned().collect();
    out.sort_by_cached_key(|s| order_key(s, episode_size(s, pack_episodes).unwrap_or(0), lang));
    out
}

static_re!(sample_clip, r"(?i)(?:\b|_)SAMPLE(?:\b|_)");

/// Extensions that are never directly playable video streams (archives, disc images, text/subtitles).
fn non_video_filename(name: &str) -> bool {
    let ext = name.rsplit('.').next().filter(|_| name.contains('.')).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "iso" | "rar" | "zip" | "7z" | "tar" | "gz" | "nfo" | "txt" | "srt" | "sub" | "idx" | "exe" | "jpg" | "jpeg" | "png")
}

/// What Play may choose: a direct 2D video link, not labelled under 1080p (nor an unlabelled DVD rip
/// or TV capture), never a cinema copy / screener, and never an archive, disc image, or tiny sample clip.
pub(super) fn playable(s: &Source) -> bool {
    if s.saved.is_some() {
        return false;
    }
    let low = match s.quality { Some(q) => quality_rank(Some(q)) > 2, None => low_source().is_match(&s.label) };
    let bad_file = !s.filename.is_empty() && non_video_filename(&s.filename);
    let tiny_sample = sample_clip().is_match(&s.label) && s.size_bytes.is_some_and(|b| b < 200_000_000);
    s.kind == SourceKind::Direct && !is_3d(&s.label) && !low && !cinema_copy().is_match(&s.label) && !bad_file && !tiny_sample
}

/// Swarm health tier for torrent selection: healthy (>= 5 seeders) > live (1..=4) > unknown > dead (0).
pub(super) fn seeder_tier(seeders: Option<u32>) -> u8 {
    match seeders {
        Some(n) if n >= 5 => 0,
        Some(n) if n >= 1 => 1,
        None => 2,
        Some(0) => 3,
        Some(_) => 2,
    }
}

/// Rank torrent candidates for automatic Play fallback when no working Direct stream exists.
pub fn torrent_order(list: &[Source], pack_episodes: Option<usize>) -> Vec<Source> {
    let lang = stream_language().and_then(|l| crate::addons::language(&l));
    let mut ranked: Vec<Source> = list.iter()
        .filter(|s| {
            let bad_file = !s.filename.is_empty() && non_video_filename(&s.filename);
            let tiny_sample = sample_clip().is_match(&s.label) && s.size_bytes.is_some_and(|b| b < 200_000_000);
            s.kind == SourceKind::Torrent && s.info_hash.is_some() && !is_3d(&s.label) && !cinema_copy().is_match(&s.label) && !bad_file && !tiny_sample
        })
        .cloned()
        .collect();
    ranked.sort_by_key(|s| {
        let ep_bytes = episode_size(s, pack_episodes).unwrap_or(0);
        let is_episode = pack_episodes.is_some() || s.pack || episode_mark().is_match(&s.label);
        let oversized_p2p = if is_episode { ep_bytes > 7_000_000_000 } else { ep_bytes > 25_000_000_000 };
        let low_res = match s.quality {
            Some(q) => quality_rank(Some(q)) > 2,
            None => true,
        };
        let swarm_bucket = match s.seeders {
            Some(n) if n >= 50 => 0u8,
            Some(n) if n >= 15 => 1,
            Some(n) if n >= 5 => 2,
            _ => 3,
        };
        let key = order_key(s, ep_bytes, lang);
        (
            s.seeders == Some(0),
            key.0,
            key.1,
            seeder_tier(s.seeders),
            low_res,
            oversized_p2p,
            swarm_bucket,
            s.tracker_sources.is_empty(),
            key,
            std::cmp::Reverse(s.seeders.unwrap_or(0)),
        )
    });
    ranked
}

pub(super) type OrderKey = (bool, u8, bool, bool, bool, std::cmp::Reverse<(bool, u8, u64, i16, u8, u8)>);

/// The HDR scale Play ranks a file by: its header's where Play's check read one that says,
/// else its label's. HDR10+ lives in the frames, so a label's HDR10+ stands over a header's HDR.
pub(super) fn verified_hdr_tier(s: &Source) -> u8 {
    let label = hdr_tier(&s.label);
    match s.picture.and_then(|p| p.hdr_tier()) { Some(2) if label == 3 => 3, Some(t) => t, None => label }
}

/// The resolution Play ranks a file by: its label's, or 1080p's when the file is too small to
/// carry it (a 2 GB "4K" film is a heavy re-encode: among the 1080p files its size decides). An
/// episode (S01E02, or a season pack's share) needs less than a film. Unknown sizes keep their
/// label's class.
pub(super) fn picture_class(s: &Source, size: u64) -> u8 {
    let class = 5 - quality_rank(s.quality);
    // With the file's real length the bitrate decides (about what the size floors mean for a
    // two-hour film or a 45-minute episode).
    if let Some(seconds) = s.picture.and_then(|p| p.seconds).filter(|_| size > 0) {
        let mbps = size as f64 * 8.0 / seconds / 1e6;
        let floor = match s.quality { Some("4K") => 3.0, Some("1440p") => 2.0, _ => 0.0 };
        return if mbps < floor { class.min(5 - quality_rank(Some("1080p"))) } else { class };
    }
    let episode = s.pack || episode_mark().is_match(&s.label);
    let floor: u64 = match (s.quality, episode) {
        (Some("4K"), false) => 3_000_000_000, (Some("4K"), true) => 1_000_000_000,
        (Some("1440p"), false) => 2_000_000_000, (Some("1440p"), true) => 700_000_000,
        _ => 0,
    };
    if size > 0 && size < floor { class.min(5 - quality_rank(Some("1080p"))) } else { class }
}

/// Whether the current display natively decodes Dolby Vision (cached cheaply per call).
fn screen_supports_dv() -> bool {
    #[cfg(test)]
    { false }
    #[cfg(not(test))]
    { crate::display::screen().is_some_and(|s| s.dolby_vision) }
}

/// The Play-order sort key of one source whose file is `size` bytes: smaller plays first.
pub(super) fn order_key(s: &Source, size: u64, lang: Option<&crate::addons::Language>) -> OrderKey {
    order_key_for(s, size, lang, screen_supports_dv())
}

pub(super) fn order_key_for(s: &Source, size: u64, lang: Option<&crate::addons::Language>, dolby_vision_screen: bool) -> OrderKey {
    let tier = match lang {
        None => 0,
        Some(l) if l.matches(&s.label) => 0,
        Some(l) if other_language_only(&s.label, l.key) => 2,
        Some(l) => u8::from(l.key != "english"),
    };
    let hdr = verified_hdr_tier(s);
    // Dolby Vision alone (typically profile 5) ranks below SDR on screens without native DV,
    // because without a DV decoder its colours are wrong; on a native DV panel it ranks with HDR.
    let dv_penalty = hdr == 1 && !dolby_vision_screen;
    let is_hdr = hdr >= 2 || (hdr == 1 && dolby_vision_screen);
    let hdr_rank = match hdr { 1 if dolby_vision_screen => 2, 1 => 0, 0 => 1, t => t };
    // An upscale or regrade goes after every native file: its 4K and HDR are made up, its size padding.
    (s.off_title, tier, dv_penalty, upscaled().is_match(&s.label), s.quality.is_none(), std::cmp::Reverse((is_hdr,
        picture_class(s, size), size, source_tier(&s.label).map_or(-1, i16::from), hdr_rank, codec_tier(&s.label))))
}

/// The source's label names the Audio-language setting's language (flag, name or release tag).
pub fn claims_language(s: &Source) -> bool {
    stream_language().and_then(|l| crate::addons::language(&l)).is_some_and(|l| l.matches(&s.label))
}

/// A label tagged with some language other than `key`, and not as a multi-language release.
pub(super) fn other_language_only(label: &str, key: &str) -> bool {
    !multi_audio().is_match(label) && crate::addons::LANGUAGES.iter().any(|l| l.key != key && l.matches(label))
}

/// The automatic choice: the source that plays, the others to try next ("Another source"),
/// and why nothing played when nothing did.
/// `audio_known`: the chosen file's own tracks said whether it has your language (the player
/// need not check again).
#[derive(Clone, Debug, Default)]
pub struct Pick { pub best: Option<Source>, pub rest: Vec<Source>, pub provider_status: Option<String>, pub audio_known: bool }

/// Probe this many different files at once (the first round), then smaller rounds while an
/// unprobed one could still beat the best that works: at most `PROBE_MAX` files and about
/// `PROBE_BUDGET` in all.
pub(super) const PROBE_FIRST: usize = 12;
pub(super) const PROBE_NEXT: usize = 6;
pub(super) const PROBE_MAX: usize = 20;
pub(super) const PROBE_BUDGET: Duration = Duration::from_millis(2500);
/// One probe's limit, and how long the others in a round get once one source is known to work.
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
pub(super) const PROBE_GRACE: Duration = Duration::from_millis(1200);

/// Play's automatic choice for one video: every add-on's sources (not waiting for one that is
/// slow or down), the Play order, then the probe. `title` (and a film's `year`) puts releases
/// of other titles last.
pub async fn autoplay(kind: &str, video_id: &str, pack_episodes: Option<usize>, title: &str, year: Option<i32>) -> Pick {
    autoplay_with_lock(kind, video_id, pack_episodes, title, year, None).await
}

/// Like `autoplay`, preferring (1) any downloaded local/rclone stream option first, then
/// (2) `lock` (`bingeGroup` / pack / release-group match for Play Next & Resume), and finally
/// (3) standard probed quality order.
pub async fn autoplay_with_lock(
    kind: &str,
    video_id: &str,
    pack_episodes: Option<usize>,
    title: &str,
    year: Option<i32>,
    lock: Option<&StreamMatcherLock>,
) -> Pick {
    let saved = remote_db::saved_sources_for(kind, video_id);
    if !saved.is_empty() {
        let mut iter = saved.into_iter();
        return Pick {
            best: iter.next(),
            rest: iter.collect(),
            provider_status: None,
            audio_known: true,
        };
    }

    let started = std::time::Instant::now();
    dev_log("autoplay: start");
    // While slower add-ons answer, the first ones' best files are checked already (the checks are
    // kept by link, so the final round reuses or joins them).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<Source>>();
    let (early_title, early_kind) = (title.to_owned(), kind.to_owned());
    let has_lock = lock.is_some();
    let early = async move {
        if has_lock {
            while rx.recv().await.is_some() {}
            return;
        }
        while let Some(mut batch) = rx.recv().await {
            for s in &mut batch { s.off_title = !names_title(s, &early_title, year); }
            let order = play_order(&batch, pack_episodes);
            dev_log(&format!("autoplay: early round on {} {} sources", order.len(), early_kind));
            if !order.is_empty() { let _ = pick_source(order, pack_episodes).await; }
        }
    };
    let mut list = if lock.is_some() {
        // Bound slow/dead addons by `ADDON_GRACE` so Play Next never stalls for a hung provider.
        sources_within(kind, video_id, Some(ADDON_GRACE), None).await.map(|r| r.0).unwrap_or_default()
    } else {
        let fetch = play_sources(kind, video_id, Some(tx));
        tokio::pin!(fetch, early);
        tokio::select! { biased; list = &mut fetch => list, _ = &mut early => fetch.await }
    };
    list.retain(|s| s.saved.is_none());
    for s in &mut list { s.off_title = !names_title(s, title, year); }
    let mut order = play_order(&list, pack_episodes);
    let listed = started.elapsed().as_millis();
    let torrents = torrent_order(&list, pack_episodes);

    if let Some(lock) = lock {
        if let Some(matched) = match_episode_source(lock, &list) {
            let mut rest: Vec<Source> = list
                .iter()
                .filter(|s| {
                    s.saved.is_none()
                        && (s.kind == SourceKind::Direct || (s.kind == SourceKind::Torrent && s.info_hash.is_some()))
                        && *s != &matched
                        && (matched.info_hash.is_none() || s.info_hash != matched.info_hash || s.file_idx != matched.file_idx)
                })
                .cloned()
                .collect();
            rest.sort_by_key(|s| std::cmp::Reverse(lock_similarity_rank(lock, s)));
            rest.truncate(6);
            if matched.kind == SourceKind::Torrent {
                dev_log(&format!("autoplay: bingeGroup lock matched torrent ({}, {:?})", matched.addon, matched.size));
                // `audio_known: true` disables `language_check` so mpv never aborts a subbed Japanese/foreign
                // bingeGroup release to hunt for a 1.4 GB English dub in `rest`!
                return Pick { best: Some(matched), rest, provider_status: None, audio_known: true };
            }
            let failed_url = matched.url.clone();
            let lang = stream_language().and_then(|l| crate::addons::language(&l));
            let (probed, _) = probe_batch(vec![matched], lang, None, pack_episodes).await;
            if let Some((verified, _)) = probed.into_iter().next() {
                dev_log(&format!("autoplay: bingeGroup lock matched direct stream ({}, {:?})", verified.addon, verified.size));
                return Pick {
                    best: Some(verified),
                    rest,
                    provider_status: None,
                    audio_known: true,
                };
            }
            // Locked direct URL didn't answer: exclude it so `pick_source` never probes it twice.
            order.retain(|s| s.url != failed_url);
        }
    }
    // Direct links exist but none is 1080p or better, and no torrent fallback exists: Play says so.
    if order.is_empty() && torrents.is_empty() && list.iter().any(|s| s.kind == SourceKind::Direct && !is_3d(&s.label)) {
        dev_log(&format!("autoplay: {} sources, none 1080p or better", list.len()));
        return Pick { provider_status: Some(BELOW_1080P.into()), ..Default::default() };
    }
    let playable_len = order.len();
    let mut pick = pick_source(order, pack_episodes).await;
    if pick.best.is_none() && !torrents.is_empty() {
        let mut iter = torrents.into_iter();
        pick.best = iter.next();
        pick.rest = iter.take(4).collect();
        pick.provider_status = None;
    } else if pick.best.is_some() && !torrents.is_empty() {
        pick.rest.extend(torrents.into_iter().take(4));
    }
    let total_ms = started.elapsed().as_millis();
    dev_log(&format!("autoplay: {} sources, {} playable in {listed} ms; chose {} after {total_ms} ms",
        list.len(), playable_len, pick.best.as_ref().map(source_summary).unwrap_or_else(|| "nothing".into())));
    if total_ms >= 8000 {
        dev_log(&format!("performance: slow autoplay ({total_ms} ms) kind={kind} sources={} playable={} list_ms={listed} picked={}",
            list.len(), playable_len, pick.best.is_some()));
    }
    pick
}

/// Play's source lists, fetched ahead (`prefetch_sources`) or by Play itself, kept for a while:
/// Play joins a fetch already under way instead of asking the add-ons again. The flag says
/// whether every add-on answered; a list without the slower ones is kept only briefly.
pub(super) type Ahead = std::sync::Arc<tokio::sync::OnceCell<(Vec<Source>, bool)>>;
pub(super) static AHEAD: OnceLock<Mutex<HashMap<String, (std::time::Instant, Ahead)>>> = OnceLock::new();
pub(super) const AHEAD_TTL: Duration = Duration::from_secs(10 * 60);
pub(super) const PARTIAL_TTL: Duration = Duration::from_secs(60);
const AHEAD_CAP: usize = 64;

/// Forget the source lists, title metadata and link checks kept so far: the key, the add-ons or
/// the language changed, so they may hold links that no longer work (or miss new add-ons).
pub fn forget_sources() {
    if let Some(Ok(mut map)) = AHEAD.get().map(|m| m.lock()) { map.clear(); }
    if let Some(Ok(mut map)) = PROBED.get().map(|m| m.lock()) { map.clear(); }
    if let Some(Ok(mut c)) = TITLE_META_CACHE.get().map(|m| m.lock()) { c.clear(); }
}

pub(super) async fn play_sources(kind: &str, video_id: &str, early: Option<tokio::sync::mpsc::UnboundedSender<Vec<Source>>>) -> Vec<Source> {
    let cell = {
        let Ok(mut map) = AHEAD.get_or_init(Default::default).lock() else { return Vec::new() };
        // Expired lists, and fetches that found nothing (an add-on may have been down), are asked again.
        map.retain(|_, (at, cell)| {
            let ttl = if cell.get().is_some_and(|(_, complete)| !complete) { PARTIAL_TTL } else { AHEAD_TTL };
            at.elapsed() < ttl && cell.get().is_none_or(|(v, _)| !v.is_empty())
        });
        let key = format!("{kind}/{video_id}");
        if map.len() >= AHEAD_CAP && !map.contains_key(&key) {
            map.retain(|_, (_, cell)| !cell.initialized());
            if map.len() >= AHEAD_CAP { map.clear(); }
        }
        map.entry(key).or_insert_with(|| (std::time::Instant::now(), Ahead::default())).1.clone()
    };
    let (kind, id) = (kind.to_owned(), video_id.to_owned());
    cell.get_or_init(|| async move { sources_within(&kind, &id, Some(ADDON_GRACE), early).await.map(|r| (r.0, r.2)).unwrap_or_default() }).await.0.clone()
}

/// Ask the add-ons for a video's sources before Play is pressed (a title page or Tonight's pick is
/// open), so Play only has to check them. Nothing is resolved on the media provider yet.
pub fn prefetch_sources(kind: &str, video_id: &str) {
    let (kind, id) = (kind.to_owned(), video_id.to_owned());
    if let Some(runtime) = RUNTIME.get() { runtime.spawn(async move { play_sources(&kind, &id, None).await; }); }
}

/// Developer option: Play's automatic choice for each "kind/id/title/year" line, one after the
/// other, nothing played: cold (sources asked for then), then again with the sources fetched
/// ahead as a title page does. Times and picks go to the developer log.
pub async fn dev_autoplay(list: String) {
    for pass in ["cold", "ahead"] {
        for line in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let parts: Vec<&str> = line.splitn(4, '/').collect();
            let [kind, id, ..] = parts[..] else { continue };
            let (name, year) = (parts.get(2).copied().unwrap_or(""), parts.get(3).and_then(|y| y.parse().ok()));
            dev_log(&format!("bench {pass} {name}"));
            let t = std::time::Instant::now();
            let pick = autoplay(kind, id, (kind == "series").then_some(10), name, year).await;
            dev_log(&format!("bench {pass} {name}: {} ms -> {}", t.elapsed().as_millis(), pick.best.as_ref().map(source_summary).unwrap_or_else(|| "nothing".into())));
        }
    }
}

/// "4K HDR 58.2 GiB REMUX (AddonA)": what the log says about a source; never its link.
pub(crate) fn source_summary(s: &Source) -> String {
    format!("{} {} {} ({})", s.quality.unwrap_or("?"), s.hdr.unwrap_or("SDR"), s.size.as_deref().unwrap_or("size?"), s.addon)
}

/// One line in the developer log (logcat "veylo" on Android, stderr elsewhere). Never URLs or keys.
pub(crate) fn dev_log(message: &str) {
    record_diag_line(message);
    #[cfg(target_os = "android")]
    crate::android::log(message);
    #[cfg(not(target_os = "android"))]
    eprintln!("{message}");
}

/// The best candidates are probed together (the relay's one-byte check), one link per file, and
/// the ones that answer are ranked again by the size their server reports: add-ons often declare
/// a whole pack's or collection's size as the file's. A round stops waiting once no pending
/// candidate could outrank a working one, or `PROBE_GRACE` after the first works; another round
/// runs while the best unprobed candidate (by its declared size) could still win. Provider status
/// clips report why nothing worked.
pub async fn pick_source(candidates: Vec<Source>, pack_episodes: Option<usize>) -> Pick {
    if candidates.first().is_some_and(|c| c.kind == SourceKind::Torrent) {
        let mut iter = candidates.into_iter().filter(|c| c.kind == SourceKind::Torrent && c.info_hash.is_some());
        return Pick { best: iter.next(), rest: iter.collect(), provider_status: None, audio_known: false };
    }
    let size = |s: &Source| episode_size(s, pack_episodes).unwrap_or(0);
    let lang = stream_language().and_then(|l| crate::addons::language(&l));
    let started = std::time::Instant::now();
    // The same file from several scrapers: probe one of its links.
    let file = |s: &Source| if s.filename.is_empty() { s.url.clone() } else { format!("{}|{:?}", s.filename.to_lowercase(), s.size_bytes) };
    let mut probed_files: HashSet<String> = HashSet::new();
    let mut probed_urls: HashSet<String> = HashSet::new();
    let mut working: Vec<Source> = Vec::new();
    let mut verdicts: Vec<AudioVerdict> = Vec::new();
    let mut provider_status = None;
    let mut cursor = 0;
    loop {
        let take = if probed_files.is_empty() { PROBE_FIRST } else { PROBE_NEXT }.min(PROBE_MAX - probed_files.len());
        let mut batch = Vec::new();
        while cursor < candidates.len() && batch.len() < take {
            let c = &candidates[cursor];
            cursor += 1;
            if probed_files.insert(file(c)) { probed_urls.insert(c.url.clone()); batch.push(c.clone()); }
        }
        if batch.is_empty() { break; }
        // Later rounds only run while something works: each gets `PROBE_GRACE`, within the budget.
        let cap = (!working.is_empty()).then(|| tokio::time::Instant::from_std((started + PROBE_BUDGET).min(std::time::Instant::now() + PROBE_GRACE)));
        let (found, status) = probe_batch(batch, lang, cap, pack_episodes).await;
        if status.is_some() { provider_status = status; }
        for (src, verdict) in found { working.push(src); verdicts.push(verdict); }
        let Some(best) = working.iter().map(|s| order_key(s, size(s), lang)).min() else { continue };
        // Candidates are in Play order by their declared sizes, which are at least the real ones;
        // a round is worth it only for a file at least a tenth bigger, or of a better kind.
        let next = candidates[cursor..].iter().find(|c| !probed_files.contains(&file(c)));
        let could_win = next.is_some_and(|c| order_key(c, size(c) / 10 * 9, lang) < best)
            || (started.elapsed() < PROBE_GRACE && rivals_wanted(&working, lang, pack_episodes).is_some_and(|class| candidates[cursor..].iter().take(PROBE_NEXT * 2).any(|c| remux_class(c) == Some(class))));
        if !could_win || probed_files.len() >= PROBE_MAX || started.elapsed() >= PROBE_BUDGET { break; }
    }
    if working.is_empty() { return Pick { best: None, rest: Vec::new(), provider_status, audio_known: false }; }
    let mut paired: Vec<(Source, AudioVerdict)> = working.into_iter().zip(verdicts).collect();
    paired.sort_by_cached_key(|(s, _)| order_key(s, size(s), lang));
    dev_log(&format!("checked {} working files in {} ms, audio: {}; pictures: {}", paired.len(), started.elapsed().as_millis(),
        paired.iter().map(|(_, v)| match v { Some((true, true)) => "main", Some((true, false)) => "yes", Some((false, _)) => "no", None => "?" }).collect::<Vec<_>>().join(" "),
        paired.iter().map(|(s, _)| picture_summary(s)).collect::<Vec<_>>().join(" | ")));
    // The files' own audio tracks: one in your language first, then those that can't tell, then
    // the rest, each in the Play order.
    let (working, verdicts): (Vec<Source>, Vec<AudioVerdict>) = paired.into_iter().unzip();
    let ranked = rank_by_audio(working, verdicts);
    let audio_known = ranked[0].0.is_some();
    let mut working: Vec<Source> = ranked.into_iter().map(|(_, s)| s).collect();
    let mut rest = working.split_off(1);
    rest.extend(candidates.into_iter().filter(|c| !probed_urls.contains(&c.url)));
    Pick { best: working.pop(), rest, provider_status: None, audio_known }
}

/// What a file's tracks say about the Audio-language setting: (it has that language, its main
/// track is in it); None when unread or unclear.
pub(super) type AudioVerdict = Option<(bool, bool)>;

/// Working sources (in the Play order) ranked again by their own audio: files with your language,
/// then those that can't tell, then the rest. Among remuxes of the same resolution and HDR (the
/// same picture: their sizes differ by the dubs they carry) the one whose main track is in your
/// language, the original release, goes first.
pub(super) fn rank_by_audio(working: Vec<Source>, verdicts: Vec<AudioVerdict>) -> Vec<(AudioVerdict, Source)> {
    let mut groups: Vec<usize> = Vec::new();
    for (i, s) in working.iter().enumerate() {
        let same = remux_class(s).and_then(|c| working[..i].iter().position(|o| remux_class(o) == Some(c)));
        groups.push(same.unwrap_or(i));
    }
    let mut ranked: Vec<(usize, AudioVerdict, Source)> = groups.into_iter().zip(verdicts).zip(working).map(|((g, v), s)| (g, v, s)).collect();
    ranked.sort_by_key(|(group, v, _)| {
        let tier = match v { Some((true, _)) => 0, None => 1, Some((false, _)) => 2 };
        (tier, *group, !matches!(v, Some((_, true))))
    });
    ranked.into_iter().map(|(_, v, s)| (v, s)).collect()
}

/// A remux's picture class (resolution, HDR, Dolby Vision only, upscaled): remuxes of one class
/// carry the same picture. None for encodes, whose size is their bitrate.
pub(super) type RemuxClass = (Option<&'static str>, bool, bool, bool);
pub(super) fn remux_class(s: &Source) -> Option<RemuxClass> {
    let hdr = verified_hdr_tier(s);
    (source_tier(&s.label) == Some(4)).then(|| (s.quality, hdr >= 2, hdr == 1, upscaled().is_match(&s.label)))
}

/// Working remuxes of the best one's class wanted before probing stops, so their audio can decide.
pub(super) const REMUX_RIVALS: usize = 3;

/// The best working source's remux class while fewer than `REMUX_RIVALS` of it work.
pub(super) fn rivals_wanted(working: &[Source], lang: Option<&crate::addons::Language>, pack_episodes: Option<usize>) -> Option<RemuxClass> {
    let best = working.iter().min_by_key(|s| order_key(s, episode_size(s, pack_episodes).unwrap_or(0), lang))?;
    let class = remux_class(best)?;
    (working.iter().filter(|s| remux_class(s) == Some(class)).count() < REMUX_RIVALS).then_some(class)
}

/// How long a working file's header may take to read.
pub(super) const AUDIO_TIMEOUT: Duration = Duration::from_millis(1500);

/// What checking one link found: it works (and its real size, provider status clip, audio tracks
/// and the link its redirects end at).
#[derive(Clone, Debug)]
pub(super) struct ProbeOutcome { ok: bool, status: u64, bytes: Option<u64>, provider_status: Option<String>, tracks: Option<Vec<crate::tracks::AudioTrack>>,
    picture: Option<crate::tracks::Picture>, resolved: Option<String> }

/// Checks by link, for a while: a check under way is joined, not repeated (Play's early round
/// on the first add-on's sources, the final one, "Another source"). Only links that work are
/// kept: a status clip (often a passing refusal: a burst of requests, the stream provider busy),
/// a timeout or a network error is tried again next time.
pub(super) type Probed = std::sync::Arc<tokio::sync::OnceCell<ProbeOutcome>>;
pub(super) static PROBED: OnceLock<Mutex<HashMap<String, (std::time::Instant, Probed)>>> = OnceLock::new();
pub(super) const PROBED_TTL: Duration = Duration::from_secs(5 * 60);
const PROBED_CAP: usize = 512;
/// Checks under way at once for one add-on's links.
pub(super) const PROBES_PER_ADDON: usize = 12;
/// The pause before checking a link again that answered "too many requests".
pub(super) const RETRY_AFTER_429: Duration = Duration::from_millis(1200);
pub(super) static PROBE_GATES: OnceLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Semaphore>>>> = OnceLock::new();

/// The relay's one-byte check of a link, then (when it works) its header for the audio tracks.
pub(super) async fn probe_one(src: Source) -> Option<ProbeOutcome> {
    let cell = {
        let mut map = PROBED.get_or_init(Default::default).lock().ok()?;
        map.retain(|_, (at, _)| at.elapsed() < PROBED_TTL);
        if map.len() >= PROBED_CAP && !map.contains_key(&src.url) {
            map.retain(|_, (_, cell)| !cell.initialized());
            if map.len() >= PROBED_CAP { map.clear(); }
        }
        map.entry(src.url.clone()).or_insert_with(|| (std::time::Instant::now(), Probed::default())).1.clone()
    };
    match cell.get_or_try_init(|| async move {
        // A few at a time per add-on: a burst of resolves is what stream providers refuse.
        let gate = {
            let mut gates = PROBE_GATES.get_or_init(Default::default).lock().map_err(|_| None)?;
            if gates.len() >= 64 && !gates.contains_key(&src.addon) { gates.clear(); }
            gates.entry(src.addon.clone()).or_insert_with(|| std::sync::Arc::new(tokio::sync::Semaphore::new(PROBES_PER_ADDON))).clone()
        };
        let _turn = gate.acquire_owned().await.map_err(|_| None)?;
        let headers = Value::Object(src.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect());
        let check = || tokio::time::timeout(PROBE_TIMEOUT, backend().proxy.probe(&src.url, &headers, false));
        let mut probe = check().await.map_err(|_| None)?.map_err(|_| None)?;
        // "Too many requests": once more after a pause, as such a refusal passes.
        if probe["status"] == 429 {
            tokio::time::sleep(RETRY_AFTER_429).await;
            probe = check().await.map_err(|_| None)?.map_err(|_| None)?;
        }
        let ok = probe["ok"] == true;
        let playlist = probe["playlist"] == true;
        let probe_resolved = probe["resolved_url"].as_str().filter(|u| u.starts_with("https://")).map(str::to_owned);
        let (tracks, picture, header_resolved) = if ok && !playlist {
            tokio::time::timeout(AUDIO_TIMEOUT, file_header(src, probe_resolved.clone())).await.unwrap_or_default()
        } else {
            (None, None, None)
        };
        let resolved = header_resolved.or(probe_resolved);
        let outcome = ProbeOutcome { ok, status: probe["status"].as_u64().unwrap_or(0), bytes: probe["bytes"].as_u64(), provider_status: probe["provider_status"].as_str().map(str::to_owned),
            tracks, picture, resolved };
        if ok { Ok(outcome) } else { Err(Some(outcome)) }
    }).await {
        Ok(found) => Some(found.clone()),
        Err(answer) => answer,
    }
}

fn same_origin(a: &str, b: &str) -> bool {
    let (Ok(ua), Ok(ub)) = (url::Url::parse(a), url::Url::parse(b)) else { return false };
    ua.scheme() == ub.scheme() && ua.host_str() == ub.host_str() && ua.port_or_known_default() == ub.port_or_known_default()
}

fn header_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("RStream/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(8))
            .build()
            .expect("header probe client")
    })
}

/// The audio tracks and the picture a file's header declares, read from its first bytes
/// (stopping as soon as the tracks are known: the picture is in the same part of the header),
/// and the https link the source's redirects end at. When `probe_resolved` is already known from
/// the one-byte check, fetch directly from it to avoid a second provider redirect/resolve.
pub(super) async fn file_header(src: Source, probe_resolved: Option<String>) -> (Option<Vec<crate::tracks::AudioTrack>>, Option<crate::tracks::Picture>, Option<String>) {
    file_header_with_client(header_http_client(), src, probe_resolved).await
}

pub(super) async fn file_header_with_client(http: &reqwest::Client, src: Source, probe_resolved: Option<String>) -> (Option<Vec<crate::tracks::AudioTrack>>, Option<crate::tracks::Picture>, Option<String>) {
    let _ = http;
    let target = probe_resolved.as_deref().unwrap_or(&src.url);
    let mut req = header_http_client().get(target);
    // Forward origin-scoped headers only when fetching the original origin (never leak to redirected CDNs),
    // and never let add-on headers override reserved Range or Host headers.
    if same_origin(&src.url, target) {
        for (k, v) in &src.headers {
            if !k.eq_ignore_ascii_case("range") && !k.eq_ignore_ascii_case("host") {
                req = req.header(k.as_str(), v.as_str());
            }
        }
    }
    req = req.header(reqwest::header::RANGE, format!("bytes=0-{}", crate::tracks::HEAD_BYTES - 1));
    let Ok(mut response) = req.send().await else { return (None, None, probe_resolved) };
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT { return (None, None, probe_resolved); }
    let resolved = Some(response.url().to_string()).filter(|u| u.starts_with("https://") && *u != src.url).or(probe_resolved);
    let mut head = Vec::with_capacity(65536);
    // Avoid re-parsing the EBML/MP4 header on every ~16 KiB TCP chunk; parse at geometric milestones.
    let mut next_parse = 16 * 1024;
    while head.len() < crate::tracks::HEAD_BYTES {
        let Ok(Some(chunk)) = response.chunk().await else { break };
        head.extend_from_slice(&chunk);
        if head.len() >= next_parse {
            if let Some(tracks) = crate::tracks::audio_tracks(&head) { return (Some(tracks), crate::tracks::picture(&head), resolved); }
            next_parse = (next_parse * 2).min(crate::tracks::HEAD_BYTES);
        }
    }
    (crate::tracks::audio_tracks(&head), crate::tracks::picture(&head), resolved)
}

/// A working file as its header declares it: its resolution and HDR replace the label's (the
/// label stays where the header can't tell).
pub(super) fn with_picture(mut src: Source, picture: crate::tracks::Picture) -> Source {
    let class = crate::player::resolution_class(picture.width as i64, picture.height as i64);
    if !class.is_empty() { src.quality = Some(class); }
    src.picture = Some(picture);
    src.hdr = match verified_hdr_tier(&src) { 3 => Some("HDR10+"), 2 => Some("HDR"), 1 => Some("DV"), _ => None };
    src
}

/// "3840x1608 PQ DV8 24 Mb/s": what the log says about a file's own picture.
pub(super) fn picture_summary(s: &Source) -> String {
    let Some(p) = s.picture else { return "?".into() };
    let transfer = match p.transfer { crate::tracks::Transfer::Pq => " PQ", crate::tracks::Transfer::Hlg => " HLG", crate::tracks::Transfer::Sdr => " SDR", _ => "" };
    let dv = p.dolby_vision.map(|d| format!(" DV{d}")).unwrap_or_default();
    let rate = p.seconds.zip(s.size_bytes).map(|(sec, b)| format!(" {:.0} Mb/s", b as f64 * 8.0 / sec / 1e6)).unwrap_or_default();
    format!("{}x{}{transfer}{dv}{rate}", p.width, p.height)
}

/// The batch's working sources, each with the size its server reports, and a provider status seen.
pub(super) async fn probe_batch(batch: Vec<Source>, lang: Option<&'static crate::addons::Language>, cap: Option<tokio::time::Instant>, pack_episodes: Option<usize>) -> (Vec<(Source, AudioVerdict)>, Option<String>) {
    // Season packs count by their share per episode, as in the Play order.
    let size = move |s: &Source| episode_size(s, pack_episodes).unwrap_or(0);
    on_runtime(async move {
        let mut jobs = tokio::task::JoinSet::new();
        for (i, src) in batch.iter().enumerate() {
            let src = src.clone();
            jobs.spawn(async move {
                let outcome = probe_one(src).await;
                let audio = lang.zip(outcome.as_ref().and_then(|o| o.tracks.clone()))
                    .and_then(|(l, t)| crate::tracks::in_language(&t, l.key).map(|has| (has, crate::tracks::primary_in_language(&t, l.key))));
                (i, outcome, audio)
            });
        }
        let estimated: Vec<OrderKey> = batch.iter().map(|s| order_key(s, size(s), lang)).collect();
        let mut pending: HashSet<usize> = (0..batch.len()).collect();
        let mut working: Vec<Source> = Vec::new();
        let mut verdicts: Vec<AudioVerdict> = Vec::new();
        let mut provider_status = None;
        let mut deadline: Option<tokio::time::Instant> = cap;
        let started = std::time::Instant::now();
        while !pending.is_empty() {
            let next = match deadline {
                Some(at) => match tokio::time::timeout_at(at, jobs.join_next()).await { Ok(n) => n, Err(_) => break },
                None => jobs.join_next().await,
            };
            let Some(Ok((i, outcome, audio))) = next else { if next.is_none() { break } else { continue } };
            pending.remove(&i);
            if std::env::var_os("VEYLO_PROBE_TRACE").is_some() {
                let what = outcome.as_ref().map(|o| format!("ok={} status={} bytes={:?}", o.ok, o.status, o.bytes)).unwrap_or_else(|| "error or timeout".into());
                dev_log(&format!("  probe #{i} {} {:.60} est {} -> {what} at {} ms", batch[i].hdr.unwrap_or("SDR"), batch[i].filename, batch[i].size.as_deref().unwrap_or("?"), started.elapsed().as_millis()));
            }
            let Some(result) = outcome else { continue };
            if !result.ok {
                if let Some(status) = result.provider_status { provider_status = Some(status); }
                else if result.status == 429 && provider_status.is_none() { provider_status = Some("busy".into()); }
                continue;
            }
            let src = apply_probe_outcome(batch[i].clone(), result);
            working.push(src);
            verdicts.push(audio);
            let grace = tokio::time::Instant::now() + PROBE_GRACE;
            if working.len() == 1 { deadline = Some(deadline.map_or(grace, |d| d.min(grace))); }
            // A pending source's real size is at most its declared one: if none could beat the
            // best working source even at that size, the answer is known.
            let best = working.iter().map(|s| order_key(s, size(s), lang)).min().expect("one works");
            let wanted = rivals_wanted(&working, lang, pack_episodes);
            if pending.iter().all(|p| estimated[*p] >= best && (wanted.is_none() || remux_class(&batch[*p]) != wanted)) { break; }
        }
        (working.into_iter().zip(verdicts).collect(), provider_status)
    }).await.unwrap_or_default()
}

/// Apply a successful `ProbeOutcome` to `src`, stripping origin-scoped headers whenever a
/// resolved redirect crossed origins so bearer tokens never leak to third-party CDNs.
pub(super) fn apply_probe_outcome(mut src: Source, result: ProbeOutcome) -> Source {
    if let Some(picture) = result.picture {
        src = with_picture(src, picture);
    }
    // The player opens the link the add-on's resolve led to: no second resolve on Play.
    if let Some(url) = result.resolved {
        if !same_origin(&src.url, &url) {
            src.headers.clear();
        }
        src.url = url;
    }
    if let Some(bytes) = result.bytes.filter(|b| *b > 0) {
        // The server's size is the file's own: no pack share, no label.
        src.size_bytes = Some(bytes);
        src.size = Some(binary_size(bytes as f64));
        src.declared_size = true;
        src.labelled_bytes = None;
        src.pack = false;
    }
    src
}

/// `Pick::provider_status` when the only direct links are under 1080p (Play never chooses those).
pub(super) const BELOW_1080P: &str = "below-1080p";

/// What to say when no checked source worked (the Android app's wording).
pub fn no_source_message(provider_status: Option<&str>) -> &'static str {
    match provider_status {
        Some("downloading") => "Your media provider is still preparing this file. Try again later or choose another source.",
        Some("failed") => "Your media provider could not provide this file. Choose another source.",
        Some("busy") => "Your sources are refusing requests for a moment (too many at once). Try again in a minute.",
        Some(BELOW_1080P) => "No source is 1080p or better, so Play didn't choose one. Pick one from the list.",
        _ => "No source responded. Try again or choose another source.",
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn playable_rejects_non_video_extensions_and_tiny_samples() {
        let mk = |title: &str, filename: &str, bytes: u64| source_of("A", &json!({
            "title": title, "url": "https://x.invalid/v",
            "behaviorHints": {"filename": filename, "videoSize": bytes}
        }));
        assert!(!playable(&mk("Film 2160p HDR", "Film.2160p.iso", 50_000_000_000)));
        assert!(!playable(&mk("Film 2160p HDR", "Film.2160p.rar", 50_000_000_000)));
        assert!(!playable(&mk("Film 2160p HDR", "Film.2160p.srt", 100_000)));
        assert!(!playable(&mk("Film.2025.2160p.SAMPLE.mkv", "Film.2025.2160p.sample.mkv", 50_000_000)));
        assert!(playable(&mk("Film.2025.2160p.HDR.mkv", "Film.2025.2160p.HDR.mkv", 25_000_000_000)));
        // A full film whose title happens to contain "Sample" is not rejected.
        assert!(playable(&mk("The Sample 2025 1080p WEB-DL", "The.Sample.2025.1080p.mkv", 5_000_000_000)));
    }

    #[test]
    fn dolby_vision_only_ranks_with_hdr_on_dv_screens_only() {
        let src = |title: &str| source_of("A", &json!({"title": title, "url": "https://x.invalid/a"}));
        let dv = src("Film 2025 2160p DV WEB-DL 💾 25 GB");
        let sdr = src("Film 2025 2160p WEB-DL 💾 30 GB");
        // Non-DV screen: SDR beats DV-only.
        assert!(order_key_for(&sdr, 30_000_000_000, None, false) < order_key_for(&dv, 25_000_000_000, None, false));
        // Native DV screen: DV-only beats SDR.
        assert!(order_key_for(&dv, 25_000_000_000, None, true) < order_key_for(&sdr, 30_000_000_000, None, true));
    }

    #[test]
    fn remux_class_uses_verified_header_hdr() {
        use crate::tracks::{Picture, Transfer};
        let mut unlabeled_hdr = source_of("A", &json!({"title": "Film 2160p REMUX 60 GB", "url": "https://x.invalid/1"}));
        unlabeled_hdr = with_picture(unlabeled_hdr, Picture { width: 3840, height: 2160, transfer: Transfer::Pq, dolby_vision: None, seconds: Some(7200.0) });
        let labeled_hdr = source_of("A", &json!({"title": "Film 2160p HDR REMUX 65 GB", "url": "https://x.invalid/2"}));
        assert_eq!(remux_class(&unlabeled_hdr), remux_class(&labeled_hdr));
    }

    #[test]
    fn same_origin_guards_redirected_header_forwarding() {
        assert!(same_origin("https://addon.example/a", "https://addon.example/b"));
        assert!(!same_origin("https://addon.example/a", "https://cdn.other.example/file.mkv"));
        assert!(!same_origin("https://addon.example:8443/a", "https://addon.example/a"));
    }

    #[test]
    fn autoplay_ranking_matrix_language_and_rivals() {
        let mk = |addon: &str, title: &str, url: &str| source_of(addon, &json!({"title": title, "url": url}));
        let ita_hd = mk("AddonB", "Film 2025 1080p ITA 💾 12 GB", "https://x.invalid/ita-hd");
        let ita_uhd = mk("AddonA", "Film 2025 2160p HDR ITA 💾 20 GB", "https://x.invalid/ita-uhd");
        let multi_uhd = mk("AddonA", "Film 2025 2160p HDR MULTI 💾 40 GB", "https://x.invalid/multi-uhd");
        let eng_only = mk("AddonA", "Film 2025 2160p HDR ENG 💾 50 GB", "https://x.invalid/eng-uhd");
        let order = play_order_for(&[eng_only.clone(), multi_uhd.clone(), ita_hd.clone(), ita_uhd.clone()], None, Some("italian"));
        let urls: Vec<&str> = order.iter().map(|s| s.url.as_str()).collect();
        // Tier 0 (Italian claimed, all add-ons equal): 4K HDR > 1080p; Tier 1 (MULTI); Tier 2 (other language only).
        assert_eq!(urls, [ita_uhd.url.as_str(), ita_hd.url.as_str(), multi_uhd.url.as_str(), eng_only.url.as_str()]);

        // Rivals wanted asks for up to REMUX_RIVALS working remuxes of the best class.
        let r1 = mk("AddonA", "Film 2025 2160p HDR REMUX 💾 65 GB", "https://x.invalid/r1");
        let r2 = mk("AddonA", "Film 2025 2160p HDR REMUX 💾 60 GB", "https://x.invalid/r2");
        let r3 = mk("AddonA", "Film 2025 2160p HDR REMUX 💾 58 GB", "https://x.invalid/r3");
        assert!(rivals_wanted(&[r1.clone(), r2.clone()], None, None).is_some());
        assert!(rivals_wanted(&[r1, r2, r3], None, None).is_none());
    }

    #[test]
    fn torrent_metadata_seeder_order_and_matcher_lock_tiers() {
        let mk_torrent = |addon: &str, title: &str, hash: &str, bg: Option<&str>| {
            let mut hints = json!({});
            if let Some(bg) = bg { hints["bingeGroup"] = json!(bg); }
            source_of(addon, &json!({
                "name": format!("{addon}\n1080p"),
                "title": title,
                "infoHash": hash,
                "behaviorHints": hints,
            }))
        };

        let dead_4k = source_of("StreamAddon", &json!({
            "name": "StreamAddon\n4K",
            "title": "Film 2025 2160p HDR REMUX\n👤 0 💾 45.0 GB ⚙️ IndexerA",
            "infoHash": "0000000000000000000000000000000000000000",
        }));
        let healthy_1080p = mk_torrent(
            "StreamAddon",
            "[EncoderA] Pioneer One (Seasons 1-2) [BD 1080p][HEVC x265 10bit][Dual-Audio] (Batch)\n[EncoderA] Pioneer One - S01E01.mkv\n👤 214 💾 278.64 MB ⚙️ IndexerB",
            "2020000020200000202000002020000020200000",
            Some("addon-a|2020000020200000202000002020000020200000"),
        );
        let low_seed_4k = source_of("StreamAddon", &json!({
            "name": "StreamAddon\n4K",
            "title": "Pioneer One 2160p HDR\n👤 2 💾 12.0 GB ⚙️ IndexerC",
            "infoHash": "1111111111111111111111111111111111111111",
        }));

        assert_eq!(healthy_1080p.seeders, Some(214));
        assert_eq!(healthy_1080p.indexer.as_deref(), Some("IndexerB"));
        assert_eq!(healthy_1080p.binge_group.as_deref(), Some("addon-a|2020000020200000202000002020000020200000"));
        assert!(healthy_1080p.pack);
        assert_eq!(dead_4k.seeders, Some(0));

        // Healthy swarm (>= 5) ranks ahead of 1-4 seeder swarm, and dead 0-seeder 4K ranks last!
        let ordered = torrent_order(&[dead_4k.clone(), low_seed_4k.clone(), healthy_1080p.clone()], Some(23));
        assert_eq!(ordered[0].info_hash, healthy_1080p.info_hash);
        assert_eq!(ordered[1].info_hash, low_seed_4k.info_hash);
        assert_eq!(ordered[2].info_hash, dead_4k.info_hash);

        let lock = StreamMatcherLock::from_source(&healthy_1080p);
        assert_eq!(lock.release_group.as_deref(), Some("EncoderA"));
        assert_eq!(lock.codec.as_deref(), Some("HEVC (x265)"));
        assert!(lock.dual_audio);

        // (1) Exact bingeGroup match wins over everything else.
        let ep2_other = mk_torrent("StreamAddon", "[EncoderB] Pioneer One - S01E02 (1080p) [AVC]\n👤 500 💾 1.2 GB ⚙️ IndexerB", "aaaa0000aaaa0000aaaa0000aaaa0000aaaa0000", Some("other|group"));
        let ep2_bg = mk_torrent("StreamAddon", "[EncoderA] Pioneer One - S01E02.mkv\n👤 80 💾 280 MB ⚙️ IndexerB", "bbbb0000bbbb0000bbbb0000bbbb0000bbbb0000", Some("addon-a|2020000020200000202000002020000020200000"));
        assert_eq!(match_episode_source(&lock, &[ep2_other.clone(), ep2_bg.clone()]).unwrap().info_hash, ep2_bg.info_hash);

        // (2) Pack infoHash fallback reuses reference pack with file_idx: None when bingeGroup is absent.
        let mut lock_no_bg = lock.clone();
        lock_no_bg.binge_group = None;
        let reused_pack = match_episode_source(&lock_no_bg, std::slice::from_ref(&ep2_other)).unwrap();
        assert_eq!(reused_pack.info_hash.as_deref(), Some("2020000020200000202000002020000020200000"));
        assert_eq!(reused_pack.file_idx, None);

        // (3) Fallback release tags (addon + quality + group + codec + dual_audio) win when not a pack.
        let mut single_lock = lock_no_bg;
        single_lock.is_pack = false;
        single_lock.info_hash = Some("9999999999999999999999999999999999999999".into());
        let ep2_tag_match = mk_torrent("StreamAddon", "[EncoderA] Pioneer One - S01E02 [1080p][HEVC x265][Dual-Audio]\n👤 40 💾 290 MB ⚙️ IndexerB", "cccc0000cccc0000cccc0000cccc0000cccc0000", None);
        assert_eq!(match_episode_source(&single_lock, &[ep2_other, ep2_tag_match.clone()]).unwrap().info_hash, ep2_tag_match.info_hash);

        // (4) Magnet URL add-ons and hybrid HTTPS+infoHash streams classify accurately.
        let magnet_only = source_of("MagnetAddon", &json!({
            "name": "MagnetAddon 1080p",
            "title": "Big Buck Bunny 2008 1080p WEB-DL\n👤 180 💾 4.2 GB",
            "url": "magnet:?xt=urn:btih:3030000030300000303000003030000030300000&tr=udp://tracker.opentrackr.org:1337/announce",
        }));
        assert_eq!(magnet_only.kind, SourceKind::Torrent);
        assert!(magnet_only.info_hash.as_deref().is_some_and(|m| m.starts_with("magnet:?xt=urn:btih:")));

        let hybrid_direct = source_of("DirectAddon", &json!({
            "name": "DirectAddon 4K",
            "title": "Big Buck Bunny 2008 2160p HDR",
            "url": "https://cdn.example/d/ABC/BigBuckBunny.mkv",
            "infoHash": "3030000030300000303000003030000030300000",
        }));
        assert_eq!(hybrid_direct.kind, SourceKind::Direct);

        // (5) P2P torrent_order prefers tracker-backed high-seeder WEB-DL over 52 GB 0-tracker Remux.
        let huge_dht_remux = source_of("StreamAddon", &json!({
            "name": "StreamAddon\n4K",
            "title": "Sintel 2010 2160p UHD BluRay REMUX HDR\n👤 61 💾 51.84 GB",
            "infoHash": "4040000040400000404000004040000040400000",
        }));
        let fast_tracked_uhd = source_of("StreamAddon", &json!({
            "name": "StreamAddon\n4K",
            "title": "Sintel 2010 WEB-DL 2160p HDR10\n👤 95 💾 14.2 GB",
            "infoHash": "5050000050500000505000005050000050500000",
            "sources": ["tracker:udp://tracker.opentrackr.org:1337/announce"],
        }));
        let ranked_p2p = torrent_order(&[huge_dht_remux.clone(), fast_tracked_uhd.clone()], None);
        assert_eq!(ranked_p2p[0].info_hash, fast_tracked_uhd.info_hash);

        // (6) ~238 MB split-pack bingeGroup vs 1.42 GB [EncoderC] BluRay:
        // both exact bingeGroup AND single-episode fallback keep the ~238 MB release instead of jumping >1 GB.
        let show_ep1_small = source_of("StreamAddon", &json!({
            "name": "StreamAddon\n1080p",
            "title": "[GroupW]OpenShow_-_01~11[BDRip][X264-AAC][1920x1080][Sub_Esp][MP4]\n[GroupW]OpenShow_-_Capitulo_01[BDRip][X264-AAC][1920x1080][Sub_Esp].mp4\n👤 14 💾 374.1 MB ⚙️ IndexerA",
            "infoHash": "6060000060600000606000006060000060600000",
            "fileIdx": 0,
            "behaviorHints": { "bingeGroup": "addon-a|6060000060600000606000006060000060600000" }
        }));
        let show_ep2_small = source_of("StreamAddon", &json!({
            "name": "StreamAddon\n1080p",
            "title": "[GroupW]OpenShow_-_01~11[BDRip][X264-AAC][1920x1080][Sub_Esp][MP4]\n[GroupW]OpenShow_-_Capitulo_02[BDRip][X264-AAC][1920x1080][Sub_Esp].mp4\n👤 14 💾 238.5 MB ⚙️ IndexerA",
            "infoHash": "6060000060600000606000006060000060600000",
            "fileIdx": 1,
            "behaviorHints": { "bingeGroup": "addon-a|6060000060600000606000006060000060600000" }
        }));
        let show_ep12_split_pack = source_of("StreamAddon", &json!({
            "name": "StreamAddon\n1080p",
            "title": "[GroupW]OpenShow_-_12~22[BDRip][X264-AAC][1920x1080][Sub_Esp][MP4]\n[GroupW]OpenShow_-_Capitulo_12[BDRip][X264-AAC][1920x1080][Sub_Esp].mp4\n👤 12 💾 245.0 MB ⚙️ IndexerA",
            "infoHash": "7070000070700000707000007070000070700000",
            "fileIdx": 0,
            "behaviorHints": { "bingeGroup": "addon-a|7070000070700000707000007070000070700000" }
        }));
        let show_big_bluray = source_of("StreamAddon", &json!({
            "name": "StreamAddon\n1080p",
            "title": "[EncoderC] OpenShow 2010 (1080p Bluray Opus 2.0 x265) v2\nOpenShow.2010.s01e02.1080p.BluRay.Opus2.0.x265.v2-EncoderC.mkv\n👤 50 💾 1.42 GB ⚙️ IndexerB",
            "infoHash": "8080000080800000808000008080000080800000",
            "fileIdx": 2,
            "behaviorHints": { "bingeGroup": "addon-a|8080000080800000808000008080000080800000" }
        }));

        let groupw_lock = StreamMatcherLock::from_source_with_pack(&show_ep1_small, Some(23));
        assert_eq!(groupw_lock.episode_bytes, show_ep1_small.size_bytes);
        assert_eq!(
            match_episode_source(&groupw_lock, &[show_big_bluray.clone(), show_ep2_small.clone()])
                .unwrap()
                .info_hash,
            show_ep2_small.info_hash
        );
        // Crossing from 01~11 pack to Episode 12 (12~22 pack) picks [GroupW] ~245 MB instead of 1.42 GB [EncoderC]!
        assert_eq!(
            match_episode_source(&groupw_lock, &[show_big_bluray, show_ep12_split_pack.clone()])
                .unwrap()
                .info_hash,
            show_ep12_split_pack.info_hash
        );
    }

    #[test]
    fn file_header_reads_chunked_206_and_never_leaks_headers_cross_origin() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let clip = include_bytes!("../../assets/selftest.mp4");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen_headers = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
            let seen_srv = seen_headers.clone();
            let server = tokio::spawn(async move {
                for _ in 0..2 {
                    let (mut sock, _) = listener.accept().await.unwrap();
                    let mut req_buf = [0u8; 2048];
                    let n = sock.read(&mut req_buf).await.unwrap_or(0);
                    seen_srv.lock().unwrap().push(String::from_utf8_lossy(&req_buf[..n]).to_lowercase());
                    let resp = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        clip.len() - 1, clip.len(), clip.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(clip).await;
                }
            });
            let client = reqwest::Client::new();
            let local_url = format!("http://127.0.0.1:{port}/video.mp4");
            // Same-origin fetch: Authorization header is sent, while spoofed Range/Host are ignored.
            let mut src = source_of("A", &json!({"title": "Film 2160p", "url": local_url}));
            src.headers.push(("Authorization".into(), "Bearer secret-token".into()));
            src.headers.push(("Range".into(), "bytes=999999-9999999".into()));
            src.headers.push(("Host".into(), "evil.example".into()));
            let (tracks, pic, _) = file_header_with_client(&client, src.clone(), None).await;
            assert_eq!(tracks.as_ref().map(Vec::len), Some(1));
            assert_eq!(pic.map(|p| (p.width, p.height)), Some((320, 240)));

            // Cross-origin resolved URL: Authorization header MUST NOT be forwarded.
            src.url = "https://addon.origin.invalid/resolve/123".into();
            let (tracks2, pic2, _) = file_header_with_client(&client, src, Some(local_url)).await;
            assert_eq!(tracks2.as_ref().map(Vec::len), Some(1));
            assert_eq!(pic2.map(|p| (p.width, p.height)), Some((320, 240)));

            server.await.unwrap();
            let reqs = seen_headers.lock().unwrap();
            assert!(reqs[0].contains("authorization: bearer secret-token"), "same-origin must send header");
            assert!(reqs[0].contains("range: bytes=0-"), "reserved Range header must not be overridden");
            assert!(!reqs[0].contains("evil.example"), "reserved Host header must not be overridden");
            assert!(!reqs[1].contains("secret-token"), "cross-origin resolved URL must never receive origin secret headers");
        });
    }

    #[test]
    fn match_episode_source_rejects_bad_candidates_and_weak_tier3_fallbacks() {
        set_stream_language(Some("english".into()));
        let ep1 = source_of("AddonA", &json!({
            "name": "AddonA 4K",
            "title": "[GroupAlpha] Show S01E01 3840x2160 HEVC\n👤 40 💾 3.0 GB",
            "infoHash": "aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111",
        }));
        assert_eq!(ep1.quality, Some("4K"));
        let lock = StreamMatcherLock::from_source(&ep1);

        // Candidates disqualified by off_title, cinema_copy, non-video ext, 0 seeders, or foreign-only audio.
        let mut off_title = source_of("AddonA", &json!({
            "title": "[GroupAlpha] Other Show S01E02 2160p HEVC\n👤 30 💾 3.0 GB",
            "infoHash": "bbbb1111bbbb1111bbbb1111bbbb1111bbbb1111",
        }));
        off_title.off_title = true;
        let cam = source_of("AddonA", &json!({
            "title": "[GroupAlpha] Show S01E02 2160p HDCAM HEVC\n👤 30 💾 3.0 GB",
            "infoHash": "cccc1111cccc1111cccc1111cccc1111cccc1111",
        }));
        let archive = source_of("AddonA", &json!({
            "title": "[GroupAlpha] Show S01E02 2160p HEVC\n👤 30 💾 3.0 GB",
            "infoHash": "dddd1111dddd1111dddd1111dddd1111dddd1111",
            "behaviorHints": { "filename": "Show.S01E02.rar" },
        }));
        let dead = source_of("AddonA", &json!({
            "title": "[GroupAlpha] Show S01E02 2160p HEVC\n👤 0 💾 3.0 GB",
            "infoHash": "eeee1111eeee1111eeee1111eeee1111eeee1111",
        }));
        let rus_only = source_of("AddonA", &json!({
            "title": "[GroupAlpha] Show S01E02 2160p HEVC RUS\n👤 30 💾 3.0 GB 🇷🇺",
            "infoHash": "ffff1111ffff1111ffff1111ffff1111ffff1111",
        }));
        // Weak unrelated Tier-3 candidates (different group/addon/codec/quality, score < 85).
        let weak_other = source_of("AddonB", &json!({
            "title": "[OtherGroup] Show S01E02 1080p AVC\n👤 90 💾 800 MB",
            "infoHash": "1212121212121212121212121212121212121212",
        }));
        let weak_same_addon = source_of("AddonA", &json!({
            "title": "[OtherGroup] Show S01E02 2160p AVC\n👤 90 💾 9.5 GB",
            "infoHash": "3434343434343434343434343434343434343434",
        }));

        assert_eq!(
            match_episode_source(
                &lock,
                &[off_title, cam, archive, dead, rus_only, weak_other, weak_same_addon]
            ),
            None,
            "weak or invalid candidates must return None so autoplay falls back to normal ranking"
        );
    }

    #[test]
    fn probe_outcome_strips_headers_cross_origin_and_remote_addons_reject_loopback() {
        let mut src = source_of("DirectAddon", &json!({
            "title": "Film 1920x1080 WEB-DL",
            "url": "https://addon.example/resolve/token123/video.mkv",
            "behaviorHints": {
                "proxyHeaders": { "request": { "Authorization": "Bearer top-secret" } }
            }
        }));
        assert_eq!(src.quality, Some("1080p"));
        assert_eq!(src.headers.len(), 1);

        // Same-origin redirect keeps scoped headers.
        let same = apply_probe_outcome(
            src.clone(),
            ProbeOutcome {
                ok: true,
                status: 206,
                bytes: Some(4_000_000_000),
                provider_status: None,
                tracks: None,
                picture: None,
                resolved: Some("https://addon.example/dl/video.mkv".into()),
            },
        );
        assert_eq!(same.url, "https://addon.example/dl/video.mkv");
        assert_eq!(same.headers.len(), 1);

        // Cross-origin redirect MUST strip headers before updating `src.url`.
        src = apply_probe_outcome(
            src,
            ProbeOutcome {
                ok: true,
                status: 206,
                bytes: Some(4_000_000_000),
                provider_status: None,
                tracks: None,
                picture: None,
                resolved: Some("https://cdn.other.example/video.mkv".into()),
            },
        );
        assert_eq!(src.url, "https://cdn.other.example/video.mkv");
        assert!(src.headers.is_empty(), "cross-origin resolved URL must clear headers");

        // Remote addons may never yield loopback or unspecified Direct URLs on any port.
        let remote_manifest = Some("https://remote-addon.example/manifest.json");
        for bad in [
            "http://127.0.0.1:53682/secret.mkv",
            "http://localhost:8080/video.mkv",
            "http://[::1]:9000/video.mkv",
            "http://0.0.0.0:8080/video.mkv",
            "https://127.0.0.1:8443/video.mkv",
        ] {
            let parsed = source_of_from("Remote", remote_manifest, &json!({"title": "1280x720", "url": bad}));
            assert_eq!(parsed.quality, Some("720p"));
            assert_eq!(parsed.kind, SourceKind::External, "{bad} must not become Direct");
            assert!(parsed.url.is_empty(), "{bad} URL must be cleared");
        }
        // Local user-installed addon on loopback may still serve non-11470 loopback streams.
        let local_ok = source_of_from(
            "LocalDev",
            Some("http://127.0.0.1:7000/manifest.json"),
            &json!({"title": "1080p", "url": "http://127.0.0.1:7000/stream.mkv"}),
        );
        assert_eq!(local_ok.kind, SourceKind::Direct);

        // Pack torrent with `fileIdx` must NOT divide single-episode size by season episode count.
        let pack_ep = source_of("A", &json!({
            "title": "[Grp] Anime - S01 [1080p]\n[Grp] Anime - 01.mkv\n💾 400 MB",
            "infoHash": "9090000090900000909000009090000090900000",
            "fileIdx": 0,
        }));
        assert!(pack_ep.pack);
        assert_eq!(episode_size(&pack_ep, Some(24)), pack_ep.size_bytes);
    }
}

#[cfg(test)]
mod audit {
    use super::*;

    /// Developer audit, nothing played or resolved: Play's order on saved source lists
    /// (VEYLO_AUDIT: {"movie/tt…": {"name", "year", "kind", "streams": [add-on streams]}}),
    /// printing each title's first three. `cargo test ranking_audit -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn ranking_audit() {
        let path = std::env::var("VEYLO_AUDIT").expect("VEYLO_AUDIT: a saved list of titles and their streams");
        let all: serde_json::Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for (key, t) in &all {
            let (name, year, series) = (s(t, "name"), t["year"].as_i64().map(|y| y as i32), t["kind"] == "series");
            let mut list: Vec<Source> = t["streams"].as_array().into_iter().flatten().map(|v| source_of("StreamAddon", v)).collect();
            for src in &mut list { src.off_title = !names_title(src, &name, if series { None } else { year }); }
            let order = play_order_for(&list, series.then_some(10), Some("english"));
            println!("== {name} ({key}): {} sources, {} playable", list.len(), order.len());
            for src in order.iter().take(3) {
                let release = src.description.lines().next().unwrap_or(&src.filename).chars().take(78).collect::<String>();
                let ranked = episode_size(src, series.then_some(10)).unwrap_or(0);
                println!("   {:5} {:6} {:>10} (ranked as {:.2} GB, class {}) {}{}", src.quality.unwrap_or("?"), src.hdr.unwrap_or("SDR"), src.size.as_deref().unwrap_or("?"),
                    ranked as f64 / 1e9, picture_class(src, ranked), if src.off_title { "[off-title] " } else { "" }, release);
            }
        }
    }
}
