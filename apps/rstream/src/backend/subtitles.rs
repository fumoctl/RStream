//! Online subtitles (the add-ons', then OpenSubtitles) and intro/recap/credits times (IntroDB, AniSkip).
use super::*;

// ---------------------------------------------------------------- subtitles

pub(super) const INTRODB: &str = "https://api.introdb.app/segments";
pub(super) const ANISKIP: &str = "https://api.aniskip.com/v2/skip-times";
pub(super) const KITSU_API: &str = "https://kitsu.app/api/edge/anime";

/// Where an episode's intro times can be looked up, from its video id.
#[derive(Debug, PartialEq)]
pub enum SkipLookup { IntroDb { imdb: String, season: u32, episode: u32 }, Kitsu { id: u64, episode: u32 }, Mal { id: u64, episode: u32 } }

/// "tt0903747:1:2" (IntroDB), "kitsu:1376:5" and "mal:5114:3" (AniSkip); None for films and the rest.
pub fn skip_lookup(video_id: &str) -> Option<SkipLookup> {
    let parts: Vec<&str> = video_id.split(':').collect();
    match parts.as_slice() {
        [imdb, s, e] if imdb.starts_with("tt") && imdb[2..].chars().all(|c| c.is_ascii_digit()) && imdb.len() > 2 =>
            Some(SkipLookup::IntroDb { imdb: (*imdb).to_owned(), season: s.parse().ok()?, episode: e.parse().ok()? }),
        ["kitsu", id, e] => Some(SkipLookup::Kitsu { id: id.parse().ok()?, episode: e.parse().ok()? }),
        ["mal", id, e] => Some(SkipLookup::Mal { id: id.parse().ok()?, episode: e.parse().ok()? }),
        _ => None,
    }
}

/// The intro, recap and credits the online databases know for an episode: IntroDB for IMDb
/// episodes, AniSkip for anime (a Kitsu id is turned into its MyAnimeList id first). Only public
/// ids are sent. Nothing found, or a service down, is an empty list: the player just offers no skip.
pub async fn online_segments(video_id: &str, duration: f64) -> Vec<crate::engine::Segment> {
    let Some(lookup) = skip_lookup(video_id) else { return Vec::new() };
    let found = on_runtime(async move {
        let http = backend_http();
        let get = |url: url::Url| { let http = http.clone(); async move {
            let r = http.get(url).send().await?;
            // AniSkip answers 404 when it has nothing; that is an answer, not a failure.
            if r.status() == reqwest::StatusCode::NOT_FOUND { return Ok::<Value, anyhow::Error>(Value::Null); }
            Ok(r.error_for_status()?.json::<Value>().await?)
        } };
        let aniskip = |mal: u64, episode: u32| async move {
            let mut url = url::Url::parse(ANISKIP)?;
            url.path_segments_mut().map_err(|_| anyhow!("bad url"))?.extend([mal.to_string(), episode.to_string()]);
            {
                let mut q = url.query_pairs_mut();
                for t in ["op", "ed", "mixed-op", "mixed-ed", "recap"] { q.append_pair("types", t); }
                q.append_pair("episodeLength", &format!("{:.3}", duration.max(0.0)));
            }
            Ok::<_, anyhow::Error>(aniskip_segments(&get(url).await?, duration))
        };
        match lookup {
            SkipLookup::IntroDb { imdb, season, episode } => {
                let mut url = url::Url::parse(INTRODB)?;
                url.query_pairs_mut().append_pair("imdb_id", &imdb).append_pair("season", &season.to_string()).append_pair("episode", &episode.to_string());
                Ok(introdb_segments(&get(url).await?, duration))
            }
            SkipLookup::Mal { id, episode } => aniskip(id, episode).await,
            SkipLookup::Kitsu { id, episode } => {
                let mut url = url::Url::parse(KITSU_API)?;
                url.path_segments_mut().map_err(|_| anyhow!("bad url"))?.extend([id.to_string(), "mappings".into()]);
                url.query_pairs_mut().append_pair("filter[externalSite]", "myanimelist/anime");
                match mal_of(&get(url).await?) { Some(mal) => aniskip(mal, episode).await, None => Ok(Vec::new()) }
            }
        }
    }).await;
    match found { Ok(Ok(list)) => list, _ => Vec::new() }
}

/// The MyAnimeList id in a Kitsu mappings answer.
pub fn mal_of(v: &Value) -> Option<u64> {
    v["data"].as_array()?.iter().find(|m| m["attributes"]["externalSite"] == "myanimelist/anime")
        .and_then(|m| m["attributes"]["externalId"].as_str()?.parse().ok())
}

/// A segment from a database, kept only when it fits inside this video (an entry for another cut
/// of the episode can run past its end).
pub(super) fn online_segment(kind: crate::engine::SegmentKind, start: f64, end: f64, duration: f64) -> Option<crate::engine::Segment> {
    let end = if duration > 0.0 && end > duration { if end - duration <= 5.0 { duration } else { return None } } else { end };
    (start >= 0.0 && end - start >= 3.0).then_some(crate::engine::Segment { kind, start, end })
}

pub fn introdb_segments(v: &Value, duration: f64) -> Vec<crate::engine::Segment> {
    use crate::engine::SegmentKind::*;
    [("recap", Recap), ("intro", Intro), ("outro", Credits)].into_iter().filter_map(|(key, kind)| {
        let g = &v[key];
        let start = g["start_ms"].as_f64().map(|ms| ms / 1000.0).or_else(|| g["start_sec"].as_f64())?;
        // Credits to the end can be stored without an end.
        let end = g["end_ms"].as_f64().map(|ms| ms / 1000.0).or_else(|| g["end_sec"].as_f64())
            .or((kind == Credits && duration > 0.0).then_some(duration))?;
        online_segment(kind, start, end, duration)
    }).collect()
}

pub fn aniskip_segments(v: &Value, duration: f64) -> Vec<crate::engine::Segment> {
    use crate::engine::SegmentKind::*;
    let mut out: Vec<crate::engine::Segment> = v["results"].as_array().into_iter().flatten().filter_map(|r| {
        let kind = match r["skipType"].as_str()? { "op" | "mixed-op" => Intro, "ed" | "mixed-ed" => Credits, "recap" => Recap, _ => return None };
        online_segment(kind, r["interval"]["startTime"].as_f64()?, r["interval"]["endTime"].as_f64()?, duration)
    }).collect();
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    out
}

/// A subtitle file an add-on offers for the playing video.
#[derive(Clone, Debug, PartialEq)]
pub struct OnlineSubtitle {
    pub id: String,
    pub url: String,
    /// The add-on's language code, verbatim ("ita", "pob").
    pub code: String,
    /// The language in English ("Italian"), or the code when it is not known.
    pub language: String,
    /// The file or release it was made for, as the add-on names it (helps pick one in sync).
    pub release: Option<String>,
}

/// Every subtitle the add-ons offer for one video: the preferred language first, then English,
/// then the rest by name; the add-on's own order is kept within a language.
pub async fn online_subtitles(kind: &str, video_id: &str) -> Result<Vec<OnlineSubtitle>> {
    let v = exec(json!({"op":"subtitles","type":kind,"id":video_id})).await?;
    let list = v["subtitles"].as_array().cloned().unwrap_or_default();
    if list.is_empty() && v["errors"].as_array().is_some_and(|e| !e.is_empty()) {
        anyhow::bail!("Subtitle providers could not be reached");
    }
    let preferred = stream_language().map(|k| subtitle_language_of(&k));
    Ok(rank_subtitles(list.iter().filter_map(subtitle_of).collect(), preferred.as_deref()))
}

/// The subtitle-list name of an audio-language setting (`crate::addons::LANGUAGES` key): the two
/// lists name a few languages differently.
pub fn subtitle_language_of(key: &str) -> String {
    match key {
        "latino" => "Spanish (Latin America)".into(),
        other => crate::addons::language(other).map(|l| l.label.to_owned()).unwrap_or_default(),
    }
}

pub(super) fn safe_subtitle_url(raw: &str) -> bool {
    let Ok(u) = url::Url::parse(raw.trim()) else { return false };
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    match u.host() {
        Some(url::Host::Ipv4(ip)) => !ip.is_loopback() && !ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => {
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
        }
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.');
            !d.eq_ignore_ascii_case("localhost") && !d.to_ascii_lowercase().ends_with(".localhost")
        }
        None => false,
    }
}

pub(super) fn subtitle_of(v: &Value) -> Option<OnlineSubtitle> {
    let url = v["url"].as_str()?.trim();
    if !safe_subtitle_url(url) { return None; }
    let code = v["lang"].as_str().unwrap_or("").trim().to_owned();
    let language = Some(crate::engine::language_name(&code)).filter(|l| !l.is_empty()).unwrap_or_else(|| "Unknown".into());
    let id = v["id"].as_str().map(str::to_owned).or_else(|| v["id"].as_i64().map(|n| n.to_string())).unwrap_or_else(|| url.to_owned());
    let release = ["subtitleFileName", "movieReleaseName"].iter().find_map(|k| v[*k].as_str().map(str::trim).filter(|r| !r.is_empty()).map(str::to_owned));
    Some(OnlineSubtitle { id, url: url.to_owned(), code, language, release })
}

pub fn rank_subtitles(mut list: Vec<OnlineSubtitle>, preferred: Option<&str>) -> Vec<OnlineSubtitle> {
    let mut seen = HashSet::new();
    list.retain(|s| seen.insert(s.url.clone()));
    // A stable sort keeps the add-on's order within one language.
    list.sort_by_key(|s| (Some(s.language.as_str()) != preferred, s.language != "English", s.language == "Unknown", s.language.clone()));
    list
}

/// Subtitles whose language name or code starts a word with the query ("ita", "port", "pob").
pub fn subtitle_matches(s: &OnlineSubtitle, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() { return true; }
    s.code.to_lowercase() == q || s.language.to_lowercase().split([' ', '(', ')', ',']).any(|w| w.starts_with(&q)) || s.language.to_lowercase().starts_with(&q)
}

/// Download one subtitle into the cache and return the local file for the player.
/// Rejected `http://`, loopback, or failed subtitle URLs are never forwarded directly to `libmpv`.
pub async fn fetch_subtitle(url: &str) -> String {
    if !safe_subtitle_url(url) {
        return String::new();
    }
    match exec(json!({"op":"subtitle_file","url":url})).await {
        Ok(v) => v["cache_path"].as_str().map(str::to_owned).unwrap_or_else(|| url.to_owned()),
        Err(_) => String::new(),
    }
}
