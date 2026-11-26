//! Keyless sources: everything here works without a TMDB key and runs FIRST even when a key is
//! configured (TMDB then fills only what free sources lack). TVmaze gives a series' cast and crew,
//! untouched season posters, backgrounds, episode stills, next episode and Home's new episodes;
//! ARM + AniList + ani.zip fill multi-season anime posters and episode stills where TVmaze or TMDB
//! collapse seasons; Wikidata gives a film's credits with photos, studios, certificate, critics'
//! scores and Picks keywords; Cinemeta's catalogues give Picks candidates and related titles.
//! Answers are kept for the session, one request per title however many screens ask at once; a
//! failed answer isn't kept, so the next visit asks again.
use super::*;
use std::sync::Arc;

async fn get_json_inner(url: String, query: Vec<(&'static str, String)>) -> Option<Value> {
    let response = backend().http.get(url).query(&query)
        .timeout(Duration::from_secs(12)).send().await.ok()?.error_for_status().ok()?;
    response.json::<Value>().await.ok()
}

/// GET JSON from a fixed public endpoint.
async fn get_json(url: String, query: Vec<(&'static str, String)>) -> Option<Value> {
    on_runtime(get_json_inner(url, query)).await.ok().flatten()
}

/// POST JSON to a fixed public endpoint (e.g. AniList's keyless GraphQL API).
async fn post_json(url: String, body: Value) -> Option<Value> {
    on_runtime(async move {
        let response = backend().http.post(url).json(&body)
            .timeout(Duration::from_secs(12)).send().await.ok()?.error_for_status().ok()?;
        response.json::<Value>().await.ok()
    }).await.ok().flatten()
}

type Shared = HashMap<String, Arc<tokio::sync::OnceCell<Value>>>;

async fn once(cache: &'static OnceLock<Mutex<Shared>>, key: String, fetch: impl Future<Output = Option<Value>>) -> Option<Value> {
    let cell = {
        let mut c = cache.get_or_init(Default::default).lock().ok()?;
        match c.get(&key) {
            Some(cell) => cell.clone(),
            None => { let cell = Arc::new(tokio::sync::OnceCell::new()); cache_insert(&mut c, 120, key, cell.clone()); cell }
        }
    };
    cell.get_or_try_init(|| async move { fetch.await.ok_or(()) }).await.ok().cloned()
}

// ---------------------------------------------------------------- TVmaze

static SHOWS: OnceLock<Mutex<Shared>> = OnceLock::new();

/// TVmaze's show for a series, with its cast, crew, seasons, episodes, images and next episode in one answer.
async fn tvmaze_show(m: &Media) -> Option<Value> {
    if m.kind == "movie" { return None; }
    let imdb = imdb_of(m)?;
    once(&SHOWS, imdb.clone(), async move {
        let found = get_json("https://api.tvmaze.com/lookup/shows".into(), vec![("imdb", imdb)]).await?;
        let id = found["id"].as_i64()?;
        let embed = ["cast", "crew", "seasons", "episodes", "nextepisode", "images"].map(|e| ("embed[]", e.to_owned())).to_vec();
        get_json(format!("https://api.tvmaze.com/shows/{id}"), embed).await
    }).await
}

/// Prefer TVmaze's untouched full-resolution original image, falling back to medium only if absent.
fn tvmaze_image(v: &Value) -> Option<String> {
    v["image"]["original"].as_str().or(v["image"]["medium"].as_str()).filter(|u| u.starts_with("https://")).map(str::to_owned)
}

/// Highest-resolution URLs of one TVmaze `_embedded.images` type ("background", "poster"), largest first.
fn tvmaze_embedded_images(show: &Value, kind: &str) -> Vec<String> {
    let mut items: Vec<(bool, i64, String)> = show["_embedded"]["images"].as_array().into_iter().flatten()
        .filter(|i| i["type"].as_str() == Some(kind))
        .filter_map(|i| {
            let res = &i["resolutions"];
            let pick = if res["original"]["url"].as_str().is_some_and(|u| u.starts_with("https://")) {
                &res["original"]
            } else {
                &res["medium"]
            };
            let url = pick["url"].as_str().filter(|u| u.starts_with("https://"))?.to_owned();
            let area = pick["width"].as_i64().unwrap_or(0) * pick["height"].as_i64().unwrap_or(0);
            let main = i["main"].as_bool().unwrap_or(false);
            Some((main, area, url))
        }).collect();
    items.sort_by_key(|(main, area, _)| std::cmp::Reverse((*main, *area)));
    let mut seen = HashSet::new();
    items.into_iter().map(|(_, _, u)| u).filter(|u| seen.insert(u.clone())).collect()
}

/// A series' cast (in billing order, with full-res photos and parts), crew, untouched season posters,
/// high-resolution backgrounds/stills, poster and language.
pub(super) fn tvmaze_production(show: &Value) -> Artwork {
    let e = &show["_embedded"];
    let mut cast: Vec<Person> = Vec::new();
    for c in e["cast"].as_array().into_iter().flatten() {
        let name = s(&c["person"], "name");
        if name.is_empty() || cast.iter().any(|p| p.name == name) { continue; }
        cast.push(Person { name, role: s(&c["character"], "name"), photo: tvmaze_image(&c["person"]).or_else(|| tvmaze_image(&c["character"])) });
        if cast.len() == 15 { break; }
    }
    let mut crew: Vec<(String, Vec<Person>)> = Vec::new();
    for (kind, label) in [("Creator", "Created by"), ("Music", "Music by"), ("Director Of Photography", "Cinematography by")] {
        let people: Vec<Person> = e["crew"].as_array().into_iter().flatten().filter(|c| c["type"] == kind)
            .map(|c| Person { name: s(&c["person"], "name"), role: String::new(), photo: tvmaze_image(&c["person"]) })
            .filter(|p| !p.name.is_empty()).take(3).collect();
        if !people.is_empty() { crew.push((label.into(), people)); }
    }
    let season_posters = e["seasons"].as_array().into_iter().flatten()
        .filter_map(|x| Some((x["number"].as_i64()?, tvmaze_image(x)?))).collect();
    // TVmaze backgrounds have no textless flag (some carry network watermarks like Netflix/Prime),
    // so only keep the highest-res one as a single fallback backdrop, never as cycling hero stills.
    let backdrop = tvmaze_embedded_images(show, "background").into_iter().next();
    let poster = tvmaze_embedded_images(show, "poster").into_iter().next().or_else(|| tvmaze_image(show));
    Artwork {
        backdrop, poster, cast, crew, season_posters,
        language: Some(s(show, "language")).filter(|l| !l.is_empty()),
        ..Artwork::default()
    }
}

/// Free high-res TVmaze artwork for a series (used when BetterPosters has no backdrop/poster).
pub(super) async fn tvmaze_backdrop_artwork(m: &Media) -> Option<Artwork> {
    let show = tvmaze_show(m).await?;
    let art = tvmaze_production(&show);
    (art.backdrop.is_some() || art.poster.is_some()).then_some(art)
}

/// TVmaze's episode stills for one season: episode number -> full-resolution image.
pub(super) async fn tvmaze_stills(m: &Media, season: i64) -> HashMap<i64, String> {
    let Some(show) = tvmaze_show(m).await else { return HashMap::new() };
    show["_embedded"]["episodes"].as_array().into_iter().flatten().filter(|e| e["season"].as_i64() == Some(season))
        .filter_map(|e| Some((e["number"].as_i64()?, tvmaze_image(e)?))).collect()
}

// ---------------------------------------------------------------- Anime (ARM + AniList + ani.zip)

static ARM: OnceLock<Mutex<Shared>> = OnceLock::new();
static ANIME_ART: OnceLock<Mutex<Shared>> = OnceLock::new();
static ANIZIP: OnceLock<Mutex<Shared>> = OnceLock::new();

pub(super) fn is_animated_series(m: &Media) -> bool {
    m.kind != "movie" && (m.kind == "anime" || m.genres.is_empty()
        || m.genres.iter().any(|g| g.eq_ignore_ascii_case("animation") || g.eq_ignore_ascii_case("anime")))
}

/// Free keyless IMDb -> AniList/TVDB season cross-map (arm.haglund.dev).
async fn anime_arm(m: &Media) -> Option<Value> {
    if !is_animated_series(m) { return None; }
    let imdb = imdb_of(m)?;
    once(&ARM, imdb.clone(), async move {
        get_json("https://arm.haglund.dev/api/v2/imdb".into(), vec![("id", imdb)]).await
            .filter(Value::is_array)
    }).await
}

/// Season number -> ordered unique AniList IDs from an ARM mapping array.
/// Uses `thetvdb-season` (matching Cinemeta/TVmaze numbering), preferring TV entries over OVAs/specials.
pub(super) fn anime_season_ids(entries: &Value) -> HashMap<i64, Vec<i64>> {
    let mut rows: Vec<(i64, bool, usize, i64)> = entries.as_array().into_iter().flatten().enumerate().filter_map(|(idx, e)| {
        let anilist = e["anilist"].as_i64()?;
        let season = e["thetvdb-season"].as_i64().or_else(|| e["themoviedb-season"].as_i64())?;
        let is_tv = e["media"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("tv"));
        Some((season, is_tv, idx, anilist))
    }).collect();
    // TV entries first for regular seasons while keeping original chronological order within each group.
    rows.sort_by_key(|(season, is_tv, idx, _)| (*season, *season > 0 && !*is_tv, *idx));
    let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
    for (season, _, _, anilist) in rows {
        let list = map.entry(season).or_default();
        if !list.contains(&anilist) { list.push(anilist); }
    }
    map
}

/// Parse AniList GraphQL batch response into season posters (`coverImage.extraLarge`) and fallback banner.
pub(super) fn anilist_artwork_from(season_ids: &HashMap<i64, Vec<i64>>, response: &Value) -> Artwork {
    let mut covers: HashMap<i64, String> = HashMap::new();
    let mut banners: HashMap<i64, String> = HashMap::new();
    for media in response["data"]["Page"]["media"].as_array().into_iter().flatten() {
        let Some(id) = media["id"].as_i64() else { continue };
        if let Some(cover) = media["coverImage"]["extraLarge"].as_str()
            .or_else(|| media["coverImage"]["large"].as_str())
            .filter(|u| u.starts_with("https://"))
        {
            covers.insert(id, cover.to_owned());
        }
        if let Some(banner) = media["bannerImage"].as_str().filter(|u| u.starts_with("https://")) {
            banners.insert(id, banner.to_owned());
        }
    }
    let mut season_posters = HashMap::new();
    let mut sorted_seasons: Vec<i64> = season_ids.keys().copied().collect();
    sorted_seasons.sort_unstable();
    let mut backdrop = None;
    for season in sorted_seasons {
        if let Some(ids) = season_ids.get(&season) {
            if let Some(url) = ids.iter().find_map(|id| covers.get(id)).cloned() {
                season_posters.insert(season, url);
            }
            if backdrop.is_none() && season > 0 {
                backdrop = ids.iter().find_map(|id| banners.get(id)).cloned();
            }
        }
    }
    Artwork { backdrop, season_posters, ..Artwork::default() }
}

/// Free keyless anime season posters (`coverImage.extraLarge`) and fallback banner via ARM + AniList GraphQL.
async fn anime_artwork(m: &Media) -> Option<Artwork> {
    let entries = anime_arm(m).await?;
    let season_ids = anime_season_ids(&entries);
    if season_ids.is_empty() { return None; }
    let imdb = imdb_of(m)?;
    let mut ids: Vec<i64> = season_ids.values().filter_map(|v| v.first().copied()).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() { return None; }
    let query = json!({
        "query": "query ($ids: [Int]) { Page(perPage: 25) { media(id_in: $ids, type: ANIME) { id coverImage { extraLarge large } bannerImage } } }",
        "variables": { "ids": ids }
    });
    let resp = once(&ANIME_ART, imdb, post_json("https://graphql.anilist.co".into(), query)).await?;
    let art = anilist_artwork_from(&season_ids, &resp);
    (!art.season_posters.is_empty() || art.backdrop.is_some()).then_some(art)
}

/// Parse one `api.ani.zip` mapping payload for a target season's episode stills.
pub(super) fn anizip_stills_for_season(mapping: &Value, season: i64) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    let Some(episodes) = mapping["episodes"].as_object() else { return out };
    for (key, ep) in episodes {
        let Some(url) = ep["image"].as_str().filter(|u| u.starts_with("https://")) else { continue };
        let ep_season = ep["seasonNumber"].as_i64();
        if ep_season.is_some() && ep_season != Some(season) { continue; }
        let ep_num = ep["episodeNumber"].as_i64()
            .or_else(|| ep["episode"].as_str().and_then(|n| n.parse::<i64>().ok()))
            .or_else(|| key.parse::<i64>().ok());
        if let Some(num) = ep_num.filter(|n| *n > 0) {
            out.entry(num).or_insert_with(|| url.to_owned());
        }
    }
    out
}

/// Free keyless anime episode stills for one season via ARM + `api.ani.zip` (TVDB screencaps).
pub(super) async fn anime_stills(m: &Media, season: i64) -> HashMap<i64, String> {
    let Some(entries) = anime_arm(m).await else { return HashMap::new() };
    let season_ids = anime_season_ids(&entries);
    let Some(ids) = season_ids.get(&season) else { return HashMap::new() };
    let mut out = HashMap::new();
    // Cap at 3 cours per season to keep requests bounded.
    for &id in ids.iter().take(3) {
        let Some(mapping) = once(&ANIZIP, id.to_string(),
            get_json("https://api.ani.zip/mappings".into(), vec![("anilist_id", id.to_string())])
        ).await else { continue };
        for (ep, url) in anizip_stills_for_season(&mapping, season) {
            out.entry(ep).or_insert(url);
        }
    }
    out
}

/// Where a series stands: its next episode and when it airs, or that it has ended.
pub async fn series_status(m: &Media) -> Option<String> {
    let show = tvmaze_show(m).await?;
    let next = &show["_embedded"]["nextepisode"];
    if let (Some(season), Some(number), Some(date)) = (next["season"].as_i64(), next["number"].as_i64(), next["airdate"].as_str().filter(|d| !d.is_empty())) {
        // "27 Nov" this year, "27 Nov 2027" beyond it: the fact shares one row with the others.
        let this_year = day_label(now_secs().div_euclid(86_400))[..4].to_owned();
        let when = date_label(date);
        let when = when.strip_suffix(&format!(" {this_year}")).unwrap_or(&when);
        return Some(format!("Next: {}, {when}", episode_tag(season, number)));
    }
    (show["status"] == "Ended").then(|| "Ended".to_owned())
}

/// A TVmaze show as a Cinemeta title (by its IMDb id), so it opens and plays like any other.
pub(super) fn tvmaze_media(s: &Value) -> Option<Media> {
    let imdb = remote_db::extract_imdb_id(s["externals"]["imdb"].as_str().unwrap_or_default(), &Value::Null)?;
    let year: i32 = s["premiered"].as_str().and_then(|d| d.get(..4)).and_then(|y| y.parse().ok()).unwrap_or(0);
    let ended: Option<i32> = s["ended"].as_str().and_then(|d| d.get(..4)).and_then(|y| y.parse().ok());
    let summary = html_tag().replace_all(s["summary"].as_str().unwrap_or_default(), "").to_string();
    let meta = json!({
        "id": imdb, "type": "series", "name": s["name"], "description": summary,
        "releaseInfo": match (year, ended) { (0, _) => String::new(), (y, Some(e)) if e != y => format!("{y}–{e}"), (y, Some(_)) => y.to_string(), (y, None) => format!("{y}–") },
        "poster": s["image"]["original"].as_str().or(s["image"]["medium"].as_str()).unwrap_or_default(),
        "genres": tvmaze_genres(s), "imdbRating": s["rating"]["average"].as_f64().map(|v| format!("{v:.1}")).unwrap_or_default(),
    });
    Media::from_json(&meta, CINEMETA).map(super::tidy)
}

/// "YYYY-MM-DD" for a day counted from 1970-01-01 (UTC).
fn day_label(days: i64) -> String {
    let (y, m, d, _) = civil(days * 86_400);
    format!("{y:04}-{m:02}-{d:02}")
}

static SCHEDULES: OnceLock<Mutex<Shared>> = OnceLock::new();

/// Home's "New episodes": the most followed series with an episode out in the last three days,
/// on US television or streaming (TVmaze's schedules). Formats (talk, news, game shows, reality,
/// sport, awards) are left out.
pub async fn new_episodes() -> Vec<Media> {
    let today = now_secs().div_euclid(86_400);
    let batches = on_runtime(async move {
        let mut jobs = tokio::task::JoinSet::new();
        let mut idx = 0usize;
        for day in [today, today - 1, today - 2].map(day_label) {
            for (url, country) in [("https://api.tvmaze.com/schedule/web", None), ("https://api.tvmaze.com/schedule", Some("US"))] {
                let mut query = vec![("date", day.clone())];
                if let Some(c) = country { query.push(("country", c.to_owned())); }
                let key = format!("{url}|{day}");
                let order = idx;
                idx += 1;
                jobs.spawn(async move {
                    (order, once(&SCHEDULES, key, get_json_inner(url.into(), query)).await)
                });
            }
        }
        let mut results = Vec::with_capacity(idx);
        while let Some(Ok(done)) = jobs.join_next().await {
            results.push(done);
        }
        results.sort_by_key(|(i, _)| *i);
        results
    }).await.unwrap_or_default();

    let mut shows: Vec<Value> = Vec::new();
    for (_, list) in batches {
        let Some(list) = list else { continue };
        shows.extend(list.as_array().into_iter().flatten().map(|e| if e["show"].is_object() { e["show"].clone() } else { e["_embedded"]["show"].clone() }));
    }
    let formats = ["Talk Show", "News", "Game Show", "Reality", "Sports", "Award Show", "Variety"];
    shows.retain(|s| s["weight"].as_i64().unwrap_or(0) >= 85 && !formats.contains(&s["type"].as_str().unwrap_or_default()));
    shows.sort_by_key(|s| std::cmp::Reverse(s["weight"].as_i64().unwrap_or(0)));
    let mut seen = HashSet::new();
    shows.iter().filter(|s| seen.insert(s["id"].as_i64())).filter_map(tvmaze_media).take(20).collect()
}

// ---------------------------------------------------------------- Wikidata

const SPARQL: &str = "https://query.wikidata.org/sparql";
static TITLES: OnceLock<Mutex<Shared>> = OnceLock::new();

async fn sparql(query: String) -> Option<Value> {
    let v = get_json(SPARQL.into(), vec![("query", query), ("format", "json".into())]).await?;
    Some(v["results"]["bindings"].clone()).filter(Value::is_array)
}

/// What the title page takes from Wikidata, in one query: credits (cast with photos and parts),
/// studios and networks with their logos, languages, the US certificate and review scores.
async fn wikidata_title(m: &Media) -> Option<Value> {
    let imdb = imdb_of(m)?;
    let query = format!(r#"SELECT ?k ?v ?vLabel ?role ?chLabel ?img ?method ?when WHERE {{
  ?f wdt:P345 "{imdb}".
  {{ ?f p:P161 ?st. ?st ps:P161 ?v. BIND("cast" AS ?k) OPTIONAL {{ ?st pq:P4633 ?role }} OPTIONAL {{ ?st pq:P453 ?ch }} OPTIONAL {{ ?v wdt:P18 ?img }} }}
  UNION {{ ?f wdt:P170 ?v. BIND("Created by" AS ?k) OPTIONAL {{ ?v wdt:P18 ?img }} }}
  UNION {{ ?f wdt:P57 ?v. BIND("Directed by" AS ?k) OPTIONAL {{ ?v wdt:P18 ?img }} }}
  UNION {{ ?f wdt:P58 ?v. BIND("Written by" AS ?k) OPTIONAL {{ ?v wdt:P18 ?img }} }}
  UNION {{ ?f wdt:P86 ?v. BIND("Music by" AS ?k) OPTIONAL {{ ?v wdt:P18 ?img }} }}
  UNION {{ ?f wdt:P344 ?v. BIND("Cinematography by" AS ?k) OPTIONAL {{ ?v wdt:P18 ?img }} }}
  UNION {{ ?f wdt:P272|wdt:P449 ?v. BIND("studio" AS ?k) OPTIONAL {{ ?v wdt:P154 ?img }} }}
  UNION {{ ?f wdt:P364 ?v. BIND("language" AS ?k) }}
  UNION {{ ?f wdt:P1657 ?v. BIND("certificate" AS ?k) }}
  UNION {{ ?f p:P444 ?st. ?st ps:P444 ?role. ?st pq:P447 ?v. BIND("score" AS ?k) OPTIONAL {{ ?st pq:P459 ?method }} OPTIONAL {{ ?st pq:P585 ?when }} }}
  SERVICE wikibase:label {{ bd:serviceParam wikibase:language "en,mul". }}
}} LIMIT 800"#);
    once(&TITLES, imdb, sparql(query)).await
}

fn field<'a>(row: &'a Value, key: &str) -> &'a str { row[key]["value"].as_str().unwrap_or_default() }

/// A label (English, else the language-neutral one many names now have), unless Wikidata has
/// neither (it then answers the item's id, "Q12345").
fn label(row: &Value, key: &str) -> Option<String> {
    let l = field(row, key);
    let qid = l.len() > 1 && l.starts_with('Q') && l[1..].bytes().all(|b| b.is_ascii_digit());
    (!l.is_empty() && !qid).then(|| l.to_owned())
}

/// A Commons file at a small width (an SVG logo comes back as a PNG).
fn commons(url: &str, width: u32) -> Option<String> {
    let file = url.split("/Special:FilePath/").nth(1).filter(|f| !f.is_empty())?;
    Some(format!("https://commons.wikimedia.org/wiki/Special:FilePath/{file}?width={width}"))
}

/// Credits, studios, language and certificate from wikidata_title's rows.
pub(super) fn wikidata_production(rows: &Value) -> Artwork {
    let rows: Vec<&Value> = rows.as_array().into_iter().flatten().collect();
    fn of<'a>(rows: &'a [&'a Value], k: &'a str) -> impl Iterator<Item = &'a Value> + 'a { rows.iter().copied().filter(move |r| field(r, "k") == k) }
    // Wikidata keeps no billing order: the named parts first, then those with a photo (the
    // title page puts the add-on's top-billed names before both).
    let mut cast: Vec<(bool, bool, Person)> = Vec::new();
    for r in of(&rows, "cast") {
        let Some(name) = label(r, "vLabel") else { continue };
        let role = Some(field(r, "role").to_owned()).filter(|x| !x.is_empty()).or_else(|| label(r, "chLabel")).unwrap_or_default();
        let photo = commons(field(r, "img"), 360);
        match cast.iter_mut().find(|(_, _, p)| p.name == name) {
            Some((has_role, has_photo, p)) => {
                if p.role.is_empty() && !role.is_empty() { p.role = role; *has_role = true; }
                if p.photo.is_none() && photo.is_some() { p.photo = photo; *has_photo = true; }
            }
            None => cast.push((!role.is_empty(), photo.is_some(), Person { name, role, photo })),
        }
    }
    cast.sort_by_key(|(role, photo, _)| std::cmp::Reverse((*role, *photo)));
    let mut crew: Vec<(String, Vec<Person>)> = Vec::new();
    for job in ["Created by", "Directed by", "Written by", "Music by", "Cinematography by"] {
        let mut people: Vec<Person> = Vec::new();
        for r in of(&rows, job) {
            let Some(name) = label(r, "vLabel") else { continue };
            let photo = commons(field(r, "img"), 360);
            match people.iter_mut().find(|p| p.name == name) {
                Some(p) => if p.photo.is_none() { p.photo = photo },
                None => if people.len() < 3 { people.push(Person { name, role: String::new(), photo }) },
            }
        }
        if !people.is_empty() { crew.push((job.into(), people)); }
    }
    let mut studios: Vec<Studio> = Vec::new();
    for r in of(&rows, "studio") {
        let (Some(name), Some(logo)) = (label(r, "vLabel"), commons(field(r, "img"), 360)) else { continue };
        if studios.len() < 4 && !studios.iter().any(|x| x.name == name) { studios.push(Studio { name, logo }); }
    }
    // Only a title with one original language says which; several can't be told apart here.
    let mut languages: Vec<String> = of(&rows, "language").filter_map(|r| label(r, "vLabel")).collect();
    languages.dedup();
    let certificate = of(&rows, "certificate").filter_map(|r| label(r, "vLabel")).find(|c| c.len() <= 6);
    Artwork {
        cast: cast.into_iter().map(|(_, _, p)| p).take(30).collect(), crew, studios, certificate,
        language: (languages.len() == 1).then(|| languages.remove(0)),
        ..Artwork::default()
    }
}

/// Critics' scores: Rotten Tomatoes' Tomatometer (percent) and Metacritic's Metascore (0-100).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Scores { pub tomatometer: Option<u8>, pub metascore: Option<u8> }

const ROTTEN_TOMATOES: &str = "http://www.wikidata.org/entity/Q105584";
const TOMATOMETER: &str = "http://www.wikidata.org/entity/Q108403393";
const METACRITIC: &str = "http://www.wikidata.org/entity/Q150248";

/// The latest of each score (scores are dated as they change).
pub(super) fn scores_from(rows: &Value) -> Scores {
    let latest = |by: &str, parse: &dyn Fn(&str) -> Option<u8>, method: Option<&str>| {
        rows.as_array().into_iter().flatten()
            .filter(|r| field(r, "k") == "score" && field(r, "v") == by && method.is_none_or(|m| field(r, "method") == m))
            .filter_map(|r| Some((field(r, "when").to_owned(), parse(field(r, "role"))?)))
            .max_by(|a, b| a.0.cmp(&b.0)).map(|(_, v)| v)
    };
    let percent = |t: &str| t.trim().strip_suffix('%')?.trim().parse::<u8>().ok().filter(|v| *v <= 100);
    let of_hundred = |t: &str| t.trim().strip_suffix("/100")?.trim().parse::<u8>().ok().filter(|v| *v <= 100);
    Scores { tomatometer: latest(ROTTEN_TOMATOES, &percent, Some(TOMATOMETER)), metascore: latest(METACRITIC, &of_hundred, None) }
}

pub async fn review_scores(m: &Media) -> Scores {
    wikidata_title(m).await.map(|rows| scores_from(&rows)).unwrap_or_default()
}

fn has_artwork(art: &Artwork) -> bool {
    art.backdrop.is_some() || art.logo.is_some() || art.poster.is_some()
        || !art.stills.is_empty() || !art.cast.is_empty() || !art.crew.is_empty()
        || !art.studios.is_empty() || !art.season_posters.is_empty()
        || art.certificate.is_some() || art.language.is_some() || !art.trailers.is_empty()
}

/// Synchronously read cached free series/anime production (TVmaze + AniList) if already fetched.
pub fn cached_series_free(m: &Media) -> Option<Artwork> {
    if m.kind == "movie" { return None; }
    let imdb = imdb_of(m)?;
    let tv = SHOWS.get().and_then(|c| c.lock().ok()?.get(&imdb).cloned())
        .and_then(|cell| cell.get().map(tvmaze_production));
    let anime = ANIME_ART.get().and_then(|c| c.lock().ok()?.get(&imdb).cloned())
        .and_then(|cell| cell.get().cloned())
        .and_then(|resp| {
            let entries = ARM.get()?.lock().ok()?.get(&imdb)?.get()?.clone();
            let ids = anime_season_ids(&entries);
            Some(anilist_artwork_from(&ids, &resp))
        });
    let out = match (tv, anime) {
        (Some(t), Some(a)) => with_extras(t, a),
        (Some(t), None) => t,
        (None, Some(a)) => a,
        (None, None) => return None,
    };
    has_artwork(&out).then_some(out)
}

/// Free keyless series/anime enrichment (TVmaze untouched season posters & billing-order credits,
/// ARM + AniList extraLarge season posters). Runs alongside fast hero art so the hero paints
/// immediately without waiting on series lookups, while free season posters & cast win over TMDB.
pub async fn open_series_free(m: &Media) -> Option<Artwork> {
    if m.kind == "movie" { return None; }
    if let Some(hit) = cached_series_free(m) { return Some(hit); }
    let (show, anime) = tokio::join!(tvmaze_show(m), anime_artwork(m));
    let out = match (show.map(|s| tvmaze_production(&s)), anime) {
        (Some(t), Some(a)) => with_extras(t, a),
        (Some(t), None) => t,
        (None, Some(a)) => a,
        (None, None) => return None,
    };
    has_artwork(&out).then_some(out)
}

/// Synchronously read cached Wikidata enrichment for a title if already fetched this session.
pub fn cached_wikidata(m: &Media) -> Option<Artwork> {
    let imdb = imdb_of(m)?;
    let cell = TITLES.get()?.lock().ok()?.get(&imdb)?.clone();
    let rows = cell.get()?;
    let art = wikidata_production(rows);
    has_artwork(&art).then_some(art)
}

/// Slower free keyless enrichment from Wikidata SPARQL (film credits, studio logos, certificate,
/// language). Never touches hero backdrop/logo, so layering it on top never reloads the page.
pub async fn open_wikidata(m: &Media) -> Option<Artwork> {
    if let Some(hit) = cached_wikidata(m) { return Some(hit); }
    let rows = wikidata_title(m).await?;
    let art = wikidata_production(&rows);
    has_artwork(&art).then_some(art)
}

/// All free keyless material for a title: fast sources first, then Wikidata filling any gaps.
#[cfg(test)]
pub async fn open_production(m: &Media) -> Option<Artwork> {
    let (bp, series, wiki) = tokio::join!(bp_artwork(m), open_series_free(m), open_wikidata(m));
    let mut art = bp.unwrap_or_default();
    if let Some(s) = series { art = with_extras(art, s); }
    if let Some(w) = wiki { art = with_extras(art, w); }
    has_artwork(&art).then_some(art)
}

/// `art` with `extra` filling ONLY what `art` lacks (per-item for season posters, cast/crew
/// portraits, studios and stills). Free sources pass as `art`; TMDB passes as `extra`.
/// The one deliberate exception: a verified textless backdrop (`backdrop_clean == true`, e.g.
/// TMDB `iso_639_1: null`) upgrades an unverified promotional backdrop so Netflix/Prime/Amazon
/// watermarks never appear on the hero.
pub fn with_extras(mut art: Artwork, extra: Artwork) -> Artwork {
    if art.genres.is_empty() { art.genres = extra.genres; }
    if extra.backdrop_clean && !art.backdrop_clean {
        art.backdrop = extra.backdrop;
        art.backdrop_clean = true;
    } else if art.backdrop.is_none() {
        art.backdrop = extra.backdrop;
        art.backdrop_clean = extra.backdrop_clean;
    }
    art.logo = art.logo.or(extra.logo);
    art.poster = art.poster.or(extra.poster);
    if art.trailers.is_empty() { art.trailers = extra.trailers; }
    art.tmdb = art.tmdb.or(extra.tmdb);
    art.collection = art.collection.or(extra.collection);
    for s in extra.stills {
        if art.stills.len() >= 4 { break; }
        if Some(s.as_str()) != art.backdrop.as_deref() && !art.stills.contains(&s) {
            art.stills.push(s);
        }
    }
    art.tagline = art.tagline.or(extra.tagline);
    if art.cast.is_empty() {
        art.cast = extra.cast;
    } else {
        // Fill missing actor portraits and roles on existing free cast members first.
        for p in &mut art.cast {
            if let Some(m) = extra.cast.iter().find(|e| e.name.eq_ignore_ascii_case(&p.name)) {
                if p.photo.is_none() && m.photo.is_some() { p.photo = m.photo.clone(); }
                if p.role.is_empty() && !m.role.is_empty() { p.role = m.role.clone(); }
            }
        }
        // If free cast has few portraits, supplement with photo-bearing cast from fallback.
        let photos = art.cast.iter().filter(|p| p.photo.is_some()).count();
        if photos < 6 {
            for e in extra.cast {
                if art.cast.len() >= 15 { break; }
                if e.photo.is_some() && !art.cast.iter().any(|p| p.name.eq_ignore_ascii_case(&e.name)) {
                    art.cast.push(e);
                }
            }
        }
    }
    if art.crew.is_empty() {
        art.crew = extra.crew;
    } else {
        for (job, people) in extra.crew {
            match art.crew.iter_mut().find(|(j, _)| j == &job) {
                Some((_, existing)) => {
                    for p in existing.iter_mut() {
                        if p.photo.is_none() {
                            if let Some(m) = people.iter().find(|e| e.name.eq_ignore_ascii_case(&p.name)) {
                                p.photo = m.photo.clone();
                            }
                        }
                    }
                }
                None => art.crew.push((job, people)),
            }
        }
        let order = ["Created by", "Directed by", "Written by", "Music by", "Cinematography by"];
        art.crew.sort_by_key(|(l, _)| order.iter().position(|o| o == l).unwrap_or(order.len()));
    }
    for st in extra.studios {
        if art.studios.len() >= 4 { break; }
        if !art.studios.iter().any(|x| x.name.eq_ignore_ascii_case(&st.name)) {
            art.studios.push(st);
        }
    }
    for (season, url) in extra.season_posters {
        art.season_posters.entry(season).or_insert(url);
    }
    art.certificate = art.certificate.or(extra.certificate);
    art.language = art.language.or(extra.language);
    if art.videos.is_empty() { art.videos = extra.videos; }
    art
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_become_dates() {
        assert_eq!(day_label(0), "1970-01-01");
        assert_eq!(day_label(19_782), "2024-02-29");
        assert_eq!(day_label(20_779), "2026-11-22");
    }

    fn row(k: &str, v: &str, role: &str, method: &str, when: &str) -> Value {
        json!({"k":{"value":k},"v":{"value":v},"role":{"value":role},"method":{"value":method},"when":{"value":when}})
    }

    #[test]
    fn the_latest_tomatometer_and_metascore() {
        let rows = json!([
            row("score", ROTTEN_TOMATOES, "8.1/10", "http://www.wikidata.org/entity/Q108403540", "2021-09-04T00:00:00Z"),
            row("score", ROTTEN_TOMATOES, "86%", TOMATOMETER, "2020-01-01T00:00:00Z"),
            row("score", ROTTEN_TOMATOES, "87%", TOMATOMETER, "2022-11-29T00:00:00Z"),
            row("score", METACRITIC, "74/100", "", "2025-06-16T00:00:00Z"),
            row("score", "http://www.wikidata.org/entity/Q37312", "8.8/10", "", ""),
        ]);
        assert_eq!(scores_from(&rows), Scores { tomatometer: Some(87), metascore: Some(74) });
        assert_eq!(scores_from(&json!([])), Scores::default());
    }

    #[test]
    fn wikidata_credits_skip_unlabelled_items_and_keep_one_language() {
        let rows = json!([
            {"k":{"value":"cast"},"vLabel":{"value":"Q123"}},
            {"k":{"value":"cast"},"vLabel":{"value":"Extra Person"}},
            {"k":{"value":"cast"},"vLabel":{"value":"Lead Actor"},"chLabel":{"value":"Cobb"},"img":{"value":"http://commons.wikimedia.org/wiki/Special:FilePath/Lead%20Actor.jpg"}},
            {"k":{"value":"Directed by"},"vLabel":{"value":"A Director"},"img":{"value":"http://commons.wikimedia.org/wiki/Special:FilePath/D.jpg"}},
            {"k":{"value":"studio"},"vLabel":{"value":"No Logo Pictures"}},
            {"k":{"value":"studio"},"vLabel":{"value":"Logo Pictures"},"img":{"value":"http://commons.wikimedia.org/wiki/Special:FilePath/Logo.svg"}},
            {"k":{"value":"language"},"vLabel":{"value":"English"}},
            {"k":{"value":"certificate"},"vLabel":{"value":"PG-13"}},
        ]);
        let art = wikidata_production(&rows);
        assert_eq!(art.cast.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Lead Actor", "Extra Person"]);
        assert_eq!(art.cast[0].role, "Cobb");
        assert_eq!(art.cast[0].photo.as_deref(), Some("https://commons.wikimedia.org/wiki/Special:FilePath/Lead%20Actor.jpg?width=360"));
        assert_eq!(art.crew.len(), 1);
        assert_eq!((art.crew[0].0.as_str(), art.crew[0].1[0].name.as_str()), ("Directed by", "A Director"));
        assert_eq!(art.crew[0].1[0].photo.as_deref(), Some("https://commons.wikimedia.org/wiki/Special:FilePath/D.jpg?width=360"));
        assert_eq!(art.studios.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Logo Pictures"]);
        assert_eq!((art.language.as_deref(), art.certificate.as_deref()), (Some("English"), Some("PG-13")));
    }

    #[test]
    fn tvmaze_credits_in_billing_order_and_prefers_original_images() {
        let show = json!({
            "language":"English",
            "image":{"medium":"https://static.tvmaze.com/show-m.jpg","original":"https://static.tvmaze.com/show-o.jpg"},
            "_embedded":{
                "cast":[{"person":{"name":"Bryan Cranston","image":{"medium":"https://static.tvmaze.com/p-m.jpg","original":"https://static.tvmaze.com/p-o.jpg"}},"character":{"name":"Walter White"}}],
                "crew":[{"type":"Creator","person":{"name":"Vince Gilligan","image":{"medium":"https://static.tvmaze.com/v.jpg"}}}],
                "seasons":[{"number":1,"image":{"medium":"https://static.tvmaze.com/s1-m.jpg","original":"https://static.tvmaze.com/s1-o.jpg"}},{"number":2,"image":null}],
                "images":[
                    {"type":"background","main":false,"resolutions":{"original":{"url":"https://static.tvmaze.com/bg-1080.jpg","width":1920,"height":1080}}},
                    {"type":"background","main":false,"resolutions":{"original":{"url":"https://static.tvmaze.com/bg-1440.jpg","width":2560,"height":1440}}},
                    {"type":"poster","main":true,"resolutions":{"original":{"url":"https://static.tvmaze.com/poster-4k.jpg","width":2000,"height":3000}}}
                ]
            }
        });
        let art = tvmaze_production(&show);
        assert_eq!(art.cast[0].role, "Walter White");
        assert_eq!(art.cast[0].photo.as_deref(), Some("https://static.tvmaze.com/p-o.jpg"));
        assert_eq!((art.crew[0].0.as_str(), art.crew[0].1[0].name.as_str()), ("Created by", "Vince Gilligan"));
        assert_eq!(art.crew[0].1[0].photo.as_deref(), Some("https://static.tvmaze.com/v.jpg"));
        assert_eq!(art.season_posters.get(&1).map(String::as_str), Some("https://static.tvmaze.com/s1-o.jpg"));
        assert_eq!(art.backdrop.as_deref(), Some("https://static.tvmaze.com/bg-1440.jpg"));
        assert!(art.stills.is_empty(), "unverified TVmaze backgrounds must not cycle as hero stills");
        assert_eq!(art.poster.as_deref(), Some("https://static.tvmaze.com/poster-4k.jpg"));
    }

    #[test]
    fn arm_and_anilist_map_multi_season_anime_posters() {
        let arm = json!([
            {"anilist": 113415, "thetvdb-season": 1, "media": "TV"},
            {"anilist": 145064, "thetvdb-season": 2, "media": "TV"},
            {"anilist": 172463, "thetvdb-season": 3, "media": "TV"},
            {"anilist": 209895, "thetvdb-season": 3, "media": "TV"}
        ]);
        let season_ids = anime_season_ids(&arm);
        assert_eq!(season_ids.get(&3), Some(&vec![172463, 209895]));
        let anilist = json!({
            "data": {"Page": {"media": [
                {"id": 113415, "coverImage": {"extraLarge": "https://s4.anilist.co/s1.jpg"}, "bannerImage": "https://s4.anilist.co/b1.jpg"},
                {"id": 172463, "coverImage": {"extraLarge": "https://s4.anilist.co/s3.jpg"}, "bannerImage": null}
            ]}}
        });
        let art = anilist_artwork_from(&season_ids, &anilist);
        assert_eq!(art.season_posters.get(&1).map(String::as_str), Some("https://s4.anilist.co/s1.jpg"));
        assert_eq!(art.season_posters.get(&3).map(String::as_str), Some("https://s4.anilist.co/s3.jpg"));
        assert_eq!(art.backdrop.as_deref(), Some("https://s4.anilist.co/b1.jpg"));
    }

    #[test]
    fn anizip_parses_only_matching_season_stills() {
        let payload = json!({
            "episodes": {
                "1": {"seasonNumber": 3, "episodeNumber": 1, "image": "https://artworks.thetvdb.com/ep1.jpg"},
                "2": {"seasonNumber": 3, "episodeNumber": 2, "image": "https://artworks.thetvdb.com/ep2.jpg"},
                "S1": {"seasonNumber": 0, "episodeNumber": 1, "image": "https://artworks.thetvdb.com/sp1.jpg"}
            }
        });
        let stills = anizip_stills_for_season(&payload, 3);
        assert_eq!(stills.len(), 2);
        assert_eq!(stills.get(&1).map(String::as_str), Some("https://artworks.thetvdb.com/ep1.jpg"));
        assert_eq!(stills.get(&2).map(String::as_str), Some("https://artworks.thetvdb.com/ep2.jpg"));
    }

    #[test]
    fn with_extras_keeps_free_primary_and_fills_only_gaps() {
        let mut free = Artwork {
            backdrop: Some("https://free.example/bg.jpg".into()),
            cast: vec![
                Person { name: "Actor A".into(), role: "Hero".into(), photo: Some("https://free.example/a.jpg".into()) },
                Person { name: "Actor B".into(), role: String::new(), photo: None },
            ],
            crew: vec![("Created by".into(), vec![Person { name: "Creator X".into(), role: String::new(), photo: None }])],
            studios: vec![Studio { name: "MAPPA".into(), logo: "https://free.example/mappa.png".into() }],
            ..Artwork::default()
        };
        free.season_posters.insert(1, "https://free.example/s1.jpg".into());
        free.season_posters.insert(3, "https://free.example/s3.jpg".into());

        let mut tmdb = Artwork {
            backdrop: Some("https://tmdb.example/bg.jpg".into()),
            logo: Some("https://tmdb.example/logo.png".into()),
            tagline: Some("A tagline".into()),
            cast: vec![
                Person { name: "Actor A".into(), role: "Other".into(), photo: Some("https://tmdb.example/a.jpg".into()) },
                Person { name: "actor b".into(), role: "Rival".into(), photo: Some("https://tmdb.example/b.jpg".into()) },
            ],
            crew: vec![
                ("Created by".into(), vec![Person { name: "creator x".into(), role: String::new(), photo: Some("https://tmdb.example/x.jpg".into()) }]),
                ("Music by".into(), vec![Person { name: "Composer Y".into(), role: String::new(), photo: Some("https://tmdb.example/y.jpg".into()) }]),
            ],
            studios: vec![
                Studio { name: "mappa".into(), logo: "https://tmdb.example/mappa.png".into() },
                Studio { name: "TOHO".into(), logo: "https://tmdb.example/toho.png".into() },
            ],
            ..Artwork::default()
        };
        tmdb.season_posters.insert(0, "https://tmdb.example/s0.jpg".into());
        tmdb.season_posters.insert(1, "https://tmdb.example/s1.jpg".into());

        let merged = with_extras(free, tmdb);
        assert_eq!(merged.backdrop.as_deref(), Some("https://free.example/bg.jpg"));
        assert_eq!(merged.logo.as_deref(), Some("https://tmdb.example/logo.png"));
        assert_eq!(merged.tagline.as_deref(), Some("A tagline"));
        assert_eq!(merged.season_posters.get(&1).map(String::as_str), Some("https://free.example/s1.jpg"));
        assert_eq!(merged.season_posters.get(&3).map(String::as_str), Some("https://free.example/s3.jpg"));
        assert_eq!(merged.season_posters.get(&0).map(String::as_str), Some("https://tmdb.example/s0.jpg"));
        assert_eq!(merged.cast[0].photo.as_deref(), Some("https://free.example/a.jpg"));
        assert_eq!(merged.cast[1].photo.as_deref(), Some("https://tmdb.example/b.jpg"));
        assert_eq!(merged.cast[1].role, "Rival");
        assert_eq!(merged.crew[0].1[0].photo.as_deref(), Some("https://tmdb.example/x.jpg"));
        assert_eq!(merged.crew[1].0, "Music by");
        assert_eq!(merged.studios.len(), 2);
        assert_eq!(merged.studios[0].logo, "https://free.example/mappa.png");
        assert_eq!(merged.studios[1].name, "TOHO");

        // Verified textless (ad-free) backdrop upgrades an unverified promotional backdrop.
        let promo = Artwork { backdrop: Some("https://free.example/netflix-ad.jpg".into()), backdrop_clean: false, ..Artwork::default() };
        let clean = Artwork { backdrop: Some("https://tmdb.example/textless.jpg".into()), backdrop_clean: true, ..Artwork::default() };
        let upgraded = with_extras(promo, clean);
        assert_eq!(upgraded.backdrop.as_deref(), Some("https://tmdb.example/textless.jpg"));
        assert!(upgraded.backdrop_clean);
    }

    /// Asks the live services (VEYLO_DX_DATA: an empty folder, so no TMDB key) and prints what
    /// each keyless source gives. `cargo test live_keyless -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_keyless_sources() {
        assert!(std::env::var_os("VEYLO_DX_DATA").is_some(), "point VEYLO_DX_DATA at an empty folder");
        init().unwrap();
        let film = Media::from_json(&json!({"id":"tt1375666","type":"movie","name":"Inception","releaseInfo":"2010","genres":["Sci-Fi","Action"]}), CINEMETA).unwrap();
        let series = Media::from_json(&json!({"id":"tt0903747","type":"series","name":"Breaking Bad","releaseInfo":"2008–2013","genres":["Drama","Crime"]}), CINEMETA).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            assert!(!tmdb_ready());
            let t = std::time::Instant::now();
            let f = open_production(&film).await.unwrap();
            println!("film ({:?}): cast {} ({} photos), first {:?}", t.elapsed(), f.cast.len(), f.cast.iter().filter(|p| p.photo.is_some()).count(), f.cast.first());
            println!("  crew {:?}\n  studios {:?}\n  cert {:?} lang {:?}", f.crew, f.studios.iter().map(|s| &s.name).collect::<Vec<_>>(), f.certificate, f.language);
            println!("  scores {:?}", review_scores(&film).await);
            let t = std::time::Instant::now();
            let s = open_production(&series).await.unwrap();
            println!("series ({:?}): cast {:?}\n  crew {:?} posters {} lang {:?} studios {:?}", t.elapsed(), s.cast.iter().take(3).map(|p| (&p.name, &p.role)).collect::<Vec<_>>(),
                s.crew, s.season_posters.len(), s.language, s.studios.iter().map(|s| &s.name).collect::<Vec<_>>());
            println!("  stills s1 {} status {:?} scores {:?}", tvmaze_stills(&series, 1).await.len(), series_status(&series).await, review_scores(&series).await);
            let t = std::time::Instant::now();
            let n = new_episodes().await;
            println!("new episodes ({:?}): {:?}", t.elapsed(), n.iter().map(|m| &m.name).collect::<Vec<_>>());
            let rows = home_rows().await.unwrap();
            println!("home rows: {:?}", rows.iter().map(|r| (&r.title, r.ranked)).collect::<Vec<_>>());
            for row in rows.into_iter().filter(|r| r.ranked || r.key == NEW_EPISODES_ROW) {
                let items = catalog(row.clone()).await.unwrap();
                println!("  {}: {} {:?}", row.title, items.len(), items.iter().take(3).map(|m| &m.name).collect::<Vec<_>>());
            }
        });
    }
}
