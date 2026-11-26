//! Artwork and metadata from outside the add-ons: posters, BetterPosters (charts, tagged posters,
//! fuller details), TMDB (artwork, cast, videos, Picks candidates) and TVmaze. Without a TMDB key
//! the keyless sources in open.rs stand in.
use super::*;

// ---------------------------------------------------------------- posters

pub(super) static BETTER_POSTERS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
pub(super) static HDR_OUTPUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Windows: send HDR to an HDR screen (the setting, kept in memory for the player).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn hdr_output() -> bool { HDR_OUTPUT.load(std::sync::atomic::Ordering::Relaxed) }

/// Poster images in order of preference: BetterPosters (by IMDb id, rating and genre tags),
/// then the add-on's own poster (Cinemeta for most titles), then the backdrop. A failing
/// image moves on to the next one.
pub fn posters(m: &Media) -> Vec<String> {
    let imdb = imdb_of(m);
    let mut out = Vec::new();
    if BETTER_POSTERS.load(std::sync::atomic::Ordering::Relaxed) {
        if let Some(id) = &imdb { out.push(format!("https://btttr.cc/poster/imdb/poster-default/{id}.jpg")); }
    }
    out.push(m.poster.clone());
    // Cinemeta's image service has a poster for nearly every IMDb title; try high-res first.
    if let Some(id) = &imdb {
        out.push(format!("https://images.metahub.space/poster/large/{id}/img"));
        out.push(format!("https://images.metahub.space/poster/medium/{id}/img"));
    }
    out.push(m.background.clone());
    out.retain(|u| !u.is_empty());
    let mut seen = HashSet::new();
    out.retain(|u| seen.insert(u.clone()));
    out
}

// ---------------------------------------------------------------- BetterPosters

/// BetterPosters (btttr.cc) is a public Stremio add-on: Trakt and TMDB charts, metadata and the
/// tagged posters. Veylo reads it directly, with a short timeout, and never installs it: its
/// titles keep Cinemeta as their add-on, so the Library, history and sources stay keyed as
/// before, and Cinemeta answers whenever BetterPosters is slow or down. "Add-on" in Settings
/// turns all of it off.
pub(super) const BP_BASE: &str = "https://btttr.cc";
/// Home row keys that are BetterPosters charts.
pub(super) const BP_ROW: &str = "betterposters|";
/// The "today" charts (TMDB's most watched today) are Top 10 shelves.
pub(super) const BP_CHARTS: [(&str, &str, &str); 7] = [
    ("movie", "trakt-trending", "Trending movies"), ("series", "trakt-trending", "Trending series"),
    ("movie", "tmdb-today", "Top 10 movies today"), ("series", "tmdb-today-shows", "Top 10 series today"),
    ("movie", "trakt-popular", "Popular movies"), ("series", "trakt-popular", "Popular series"),
    ("movie", "tmdb-top", "Top rated movies"),
];
pub(super) fn bp_ranked(id: &str) -> bool { id.starts_with("tmdb-today") }

pub fn better_posters_on() -> bool { BETTER_POSTERS.load(std::sync::atomic::Ordering::Relaxed) }

/// Answers by path and when they came: kept 15 minutes, a failure 2 minutes, so a stalled
/// server isn't asked again on every screen.
pub(super) type Answers = HashMap<String, (i64, Option<Value>)>;
pub(super) static BP_CACHE: OnceLock<Mutex<Answers>> = OnceLock::new();

pub(super) async fn bp_get(path: String) -> Option<Value> {
    let cache = BP_CACHE.get_or_init(Default::default);
    let now = now_secs();
    if let Some((at, hit)) = cache.lock().ok().and_then(|c| c.get(&path).cloned()) {
        if now - at < if hit.is_some() { 900 } else { 120 } { return hit; }
    }
    let url = format!("{BP_BASE}/{path}");
    let got = on_runtime(async move {
        let response = backend().http.get(url).timeout(Duration::from_secs(8)).send().await.ok()?.error_for_status().ok()?;
        response.json::<Value>().await.ok()
    }).await.ok().flatten();
    if let Ok(mut c) = cache.lock() {
        // Expired answers (whole charts among them) go before a new one is kept.
        c.retain(|_, (at, hit)| now - *at < if hit.is_some() { 900 } else { 120 });
        cache_insert(&mut c, 400, path, (now, got.clone()));
    }
    got
}

/// A chart's titles as Cinemeta titles (`origin`): only IMDb ids, which Cinemeta also serves.
pub(super) fn bp_titles(v: &Value, origin: &str) -> Vec<Media> {
    v["metas"].as_array().into_iter().flatten()
        .filter(|m| remote_db::extract_imdb_id(&s(m, "id"), m).is_some())
        .filter_map(|m| Media::from_json(m, origin))
        .scan(HashSet::new(), |seen, m| Some(seen.insert(m.key()).then_some(m))).flatten().collect()
}

/// BetterPosters' metadata for a film or series with an IMDb id.
pub(super) async fn bp_meta(m: &Media) -> Option<Value> {
    if !better_posters_on() || !matches!(m.kind.as_str(), "movie" | "series") { return None; }
    let id = imdb_of(m)?;
    bp_get(format!("meta/{}/{id}.json", m.kind)).await?.get("meta").filter(|v| v.is_object()).cloned()
}

pub(super) fn filled(v: &Value) -> bool {
    !(v.is_null() || v.as_str().is_some_and(|t| t.trim().is_empty()) || v.as_array().is_some_and(Vec::is_empty))
}

/// The add-on's metadata with BetterPosters' laid over it: BetterPosters wins where it has a
/// value, its trailers and credits come first, and the add-on's episode list stays (its ids
/// find sources and keep watched marks) with BetterPosters' titles, stills and synopses on it.
pub(super) fn merge_meta(mut meta: Value, bp: Option<&Value>) -> Value {
    let Some(bp) = bp.filter(|b| b.is_object()) else { return meta };
    if !meta.is_object() { return bp.clone(); }
    for key in ["logo", "runtime", "imdbRating", "released", "country", "description", "genres", "director", "cast", "writer"] {
        if let Some(v) = bp.get(key).filter(|v| filled(v)) { meta[key] = v.clone(); }
    }
    for key in ["trailers", "trailerStreams", "links"] {
        let mut list: Vec<Value> = bp[key].as_array().cloned().unwrap_or_default();
        list.extend(meta[key].as_array().cloned().unwrap_or_default());
        if !list.is_empty() { meta[key] = Value::Array(list); }
    }
    let episodes: HashMap<String, &Value> = bp["videos"].as_array().into_iter().flatten().map(|e| (s(e, "id"), e)).collect();
    match meta["videos"].as_array_mut().filter(|v| !v.is_empty()) {
        Some(videos) => for e in videos {
            if let Some(b) = episodes.get(&s(e, "id")) {
                for key in ["title", "thumbnail", "overview"] {
                    if let Some(v) = b.get(key).filter(|v| filled(v)) { e[key] = v.clone(); }
                }
            }
        },
        None => if !episodes.is_empty() { meta["videos"] = bp["videos"].clone(); },
    }
    meta
}

pub(super) static IDENTS: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

/// The studio logos that read as white idents: transparent around the mark. A logo printed on its
/// own solid background would turn into a white box, so it is left out. Kept for the session.
pub async fn idents(studios: Vec<Studio>) -> Vec<Studio> {
    let cache = IDENTS.get_or_init(Default::default);
    let mut results: Vec<Option<Studio>> = vec![None; studios.len()];
    let mut pending = Vec::new();
    for (i, st) in studios.into_iter().enumerate() {
        let known = cache.lock().ok().and_then(|c| c.get(&st.logo).copied());
        match known {
            Some(true) => results[i] = Some(st),
            Some(false) => {}
            None => pending.push((i, st)),
        }
    }
    if !pending.is_empty() {
        let checked = on_runtime(async move {
            let mut jobs = tokio::task::JoinSet::new();
            for (i, st) in pending {
                jobs.spawn(async move {
                    let probe = st.logo
                        .replace("/w500/", "/w92/")
                        .replace("/w300/", "/w92/")
                        .replace("/w185/", "/w92/")
                        .replace("?width=360", "?width=92");
                    let ok = transparent_edges(probe).await.unwrap_or(false);
                    (i, st, ok)
                });
            }
            let mut done = Vec::new();
            while let Some(res) = jobs.join_next().await {
                if let Ok(item) = res { done.push(item); }
            }
            done
        }).await.unwrap_or_default();
        if let Ok(mut c) = cache.lock() {
            for (_, st, ok) in &checked { cache_insert(&mut c, 500, st.logo.clone(), *ok); }
        }
        for (i, st, ok) in checked {
            if ok { results[i] = Some(st); }
        }
    }
    results.into_iter().flatten().collect()
}

/// Whether the logo is a mark on see-through ground: a transparent corner, and ink over less than
/// about half of it (a solid plate with rounded corners is not an ident).
/// Called from inside `idents`'s `on_runtime` task with bounded byte and decode limits.
pub(super) async fn transparent_edges(url: String) -> Option<bool> {
    const LIMIT: usize = 2 * 1024 * 1024;
    let mut response = backend().http.get(url).timeout(Duration::from_secs(6)).send().await.ok()?.error_for_status().ok()?;
    if response.content_length().is_some_and(|n| n > LIMIT as u64) { return None; }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > LIMIT { return None; }
        bytes.extend_from_slice(&chunk);
    }
    tokio::task::spawn_blocking(move || decode_transparent_edges(&bytes)).await.ok().flatten()
}

pub(super) fn decode_transparent_edges(bytes: &[u8]) -> Option<bool> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(32 * 1024 * 1024);
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    reader.limits(limits);
    let img = reader.decode().ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 { return None; }
    let corner = [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)].iter().any(|&(x, y)| img.get_pixel(x, y)[3] < 128);
    let ink = img.pixels().filter(|p| p[3] >= 128).count() as f64 / (w as f64 * h as f64);
    Some(corner && ink < 0.55)
}

/// Its place in BetterPosters' "today" chart (TMDB's most watched today; 1 is the top), if it is in it.
pub async fn today_rank(m: &Media) -> Option<u64> {
    if !better_posters_on() { return None; }
    let id = imdb_of(m)?;
    let chart = if m.kind == "series" { "catalog/series/tmdb-today-shows.json" } else { "catalog/movie/tmdb-today.json" };
    let v = bp_get(chart.into()).await?;
    v["metas"].as_array()?.iter().find(|x| s(x, "id") == id).and_then(|x| x["_rank"].as_u64())
}

/// Without a TMDB key, BetterPosters lends the same TMDB backdrop, logo and trailers.
pub(super) async fn bp_artwork(m: &Media) -> Option<Artwork> {
    let meta = bp_meta(m).await?;
    let art = Artwork {
        genres: meta["genres"].as_array().into_iter().flatten().filter_map(Value::as_str).map(super::canonical_genre).collect(),
        backdrop: Some(s(&meta, "background")).filter(|u| u.starts_with("https://")),
        logo: Some(s(&meta, "logo")).filter(|u| u.starts_with("https://")),
        trailers: meta["trailers"].as_array().into_iter().flatten().map(|t| s(t, "source")).filter(|k| !k.is_empty()).collect(),
        ..Artwork::default()
    };
    (art.backdrop.is_some() || art.logo.is_some()).then_some(art)
}

// ---------------------------------------------------------------- TMDB artwork

/// Wide artwork from TMDB: a text-free backdrop and an English logo. Empty fields fall back
/// to the add-on's (Metahub) images.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Artwork {
    pub genres: Vec<String>,
    pub backdrop: Option<String>,
    /// True only when `backdrop` comes from a verified textless / ad-free tier (e.g. TMDB
    /// `iso_639_1: null`, which bans streaming-service watermarks like Netflix/Prime Video).
    pub backdrop_clean: bool,
    pub logo: Option<String>, pub poster: Option<String>,
    /// TMDB's YouTube trailers and teasers (video keys), best first: alternatives for regions
    /// where one is blocked.
    pub trailers: Vec<String>,
    pub tmdb: Option<(String, i64)>,
    /// The TMDB collection a film belongs to (a series of films), for "watch part 1 first".
    pub collection: Option<i64>,
    /// The title page's own material from the real production, all from the same TMDB answer:
    /// more official frames (textless), its tagline, cast with photos and parts, key crew,
    /// studio and network logos, the certificate, the original language, each season's
    /// poster and the videos (trailers, featurettes, behind the scenes).
    pub stills: Vec<String>,
    pub tagline: Option<String>,
    pub cast: Vec<Person>,
    /// Key crew by job ("Directed by", "Created by", "Written by", "Music by", "Cinematography by"),
    /// at most three each, with their photos where known.
    pub crew: Vec<(String, Vec<Person>)>,
    pub studios: Vec<Studio>,
    pub certificate: Option<String>,
    pub language: Option<String>,
    pub season_posters: HashMap<i64, String>,
    pub videos: Vec<Video>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Person { pub name: String, pub role: String, pub photo: Option<String> }
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Studio { pub name: String, pub logo: String }
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Video { pub youtube: String, pub name: String, pub kind: String, pub official: bool }

pub(super) const TMDB_IMG: &str = "https://image.tmdb.org/t/p";

/// Bayesian quality score for a TMDB image: prefers high-resolution (>= 1280px wide) community-
/// verified art over 1-vote uploads (which often have misleadingly high raw vote_average).
fn tmdb_image_score(i: &Value) -> (bool, i64) {
    let w = i["width"].as_i64().unwrap_or(0);
    let votes = i["vote_count"].as_f64().unwrap_or(0.0).max(0.0);
    let avg = i["vote_average"].as_f64().unwrap_or(0.0).clamp(0.0, 10.0);
    // Prior of 3 votes at 5.0 prevents a single 10/10 upload from beating a 30-vote 8.5 still.
    let bayesian = (avg * votes + 15.0) / (votes + 3.0);
    (w >= 1280, (bayesian * 1000.0) as i64)
}

/// The production's own material from one TMDB details answer (see Artwork).
pub(super) fn production(details: &Value, kind: &str, backdrop: Option<&str>) -> Artwork {
    let img = |size: &str, v: &Value| v.as_str().filter(|p| !p.is_empty()).map(|p| format!("{TMDB_IMG}/{size}{p}"));
    let mut frames: Vec<&Value> = details["images"]["backdrops"].as_array().into_iter().flatten().filter(|i| i["iso_639_1"].is_null()).collect();
    frames.sort_by_key(|i| std::cmp::Reverse(tmdb_image_score(i)));
    let stills: Vec<String> = frames.iter().filter_map(|i| img("w1280", &i["file_path"])).filter(|u| Some(u.as_str()) != backdrop).take(4).collect();
    let tv = kind == "tv";
    let credits = if tv { &details["aggregate_credits"] } else { &details["credits"] };
    let cast = credits["cast"].as_array().into_iter().flatten().take(15).map(|c| Person {
        name: s(c, "name"),
        role: if tv { c["roles"][0]["character"].as_str().unwrap_or_default().to_owned() } else { s(c, "character") },
        photo: img("h632", &c["profile_path"]),
    }).filter(|p| !p.name.is_empty()).collect();
    let mut crew: Vec<(String, Vec<Person>)> = Vec::new();
    let mut credit = |label: &str, c: &Value| {
        let person = Person { name: s(c, "name"), role: String::new(), photo: img("h632", &c["profile_path"]) };
        if person.name.is_empty() { return; }
        match crew.iter_mut().find(|(l, _)| l == label) {
            Some((_, people)) => if !people.iter().any(|p| p.name == person.name) && people.len() < 3 { people.push(person) },
            None => crew.push((label.to_owned(), vec![person])),
        }
    };
    for c in details["created_by"].as_array().into_iter().flatten() { credit("Created by", c); }
    for c in credits["crew"].as_array().into_iter().flatten() {
        let jobs: Vec<String> = if tv { c["jobs"].as_array().into_iter().flatten().map(|j| s(j, "job")).collect() } else { vec![s(c, "job")] };
        for job in jobs {
            let label = match job.as_str() { "Director" if !tv => "Directed by", "Screenplay" | "Writer" if !tv => "Written by",
                "Original Music Composer" => "Music by", "Director of Photography" if !tv => "Cinematography by", _ => continue };
            credit(label, c);
        }
    }
    let order = ["Created by", "Directed by", "Written by", "Music by", "Cinematography by"];
    crew.sort_by_key(|(l, _)| order.iter().position(|o| o == l));
    let mut studios: Vec<Studio> = Vec::new();
    for c in details["networks"].as_array().into_iter().flatten().chain(details["production_companies"].as_array().into_iter().flatten()) {
        if let Some(logo) = img("w300", &c["logo_path"]) {
            if studios.len() < 4 && !studios.iter().any(|x| x.name == s(c, "name")) { studios.push(Studio { name: s(c, "name"), logo }); }
        }
    }
    let rated = |list: &Value, field: &dyn Fn(&Value) -> Option<String>| {
        let all: Vec<&Value> = list["results"].as_array().into_iter().flatten().collect();
        all.iter().find(|r| r["iso_3166_1"] == "US").and_then(|r| field(r)).or_else(|| all.iter().find_map(|r| field(r)))
    };
    let certificate = if tv { rated(&details["content_ratings"], &|r| Some(s(r, "rating")).filter(|x| !x.is_empty())) }
        else { rated(&details["release_dates"], &|r| r["release_dates"].as_array().into_iter().flatten().map(|d| s(d, "certification")).find(|x| !x.is_empty())) };
    let original = s(details, "original_language");
    let language = details["spoken_languages"].as_array().into_iter().flatten().find(|l| l["iso_639_1"] == original.as_str())
        .map(|l| s(l, "english_name")).filter(|x| !x.is_empty());
    let season_posters = details["seasons"].as_array().into_iter().flatten()
        .filter_map(|x| Some((x["season_number"].as_i64()?, img("w500", &x["poster_path"])?))).collect();
    // The production's own videos first: official trailers, teasers, clips, then the rest.
    let rank = |kind: &str| match kind { "Trailer" => 4, "Teaser" => 3, "Clip" => 2, "Featurette" => 1, _ => 0 };
    let mut videos: Vec<Video> = details["videos"]["results"].as_array().into_iter().flatten()
        .filter(|v| v["site"] == "YouTube" && matches!(v["type"].as_str(), Some("Trailer" | "Teaser" | "Featurette" | "Behind the Scenes" | "Clip")))
        .map(|v| Video { youtube: s(v, "key"), name: s(v, "name"), kind: s(v, "type"), official: v["official"].as_bool().unwrap_or(false) })
        .filter(|v| !v.youtube.is_empty()).collect();
    videos.sort_by_key(|v| std::cmp::Reverse((v.official, rank(&v.kind))));
    videos.truncate(12);
    Artwork { stills, tagline: Some(s(details, "tagline")).filter(|t| !t.is_empty()), cast, crew, studios, certificate, language, season_posters, videos, ..Artwork::default() }
}

pub(super) static ARTWORK: OnceLock<Mutex<HashMap<String, Artwork>>> = OnceLock::new();

pub(super) fn imdb_of(m: &Media) -> Option<String> {
    remote_db::extract_imdb_id(&m.id, &m.raw)
}

pub(super) static TMDB_KEY: Mutex<Option<String>> = Mutex::new(None);
pub(super) fn set_tmdb_key(key: Option<String>) { if let Ok(mut k) = TMDB_KEY.lock() { *k = key; } }
pub(super) fn tmdb_key() -> Option<String> { TMDB_KEY.lock().ok().and_then(|k| k.clone()) }

/// GET a TMDB v3 endpoint. The key travels as a query parameter and is never logged.
pub(super) async fn tmdb(path: String, query: Vec<(&'static str, String)>) -> Option<Value> {
    let key = tmdb_key()?;
    on_runtime(async move {
        let response = backend().http.get(format!("https://api.themoviedb.org/3/{path}"))
            .query(&[("api_key", key)]).query(&query).send().await.ok()?.error_for_status().ok()?;
        response.json::<Value>().await.ok()
    }).await.ok().flatten()
}

pub(super) fn best_image(list: &Value, languages: &[Option<&str>]) -> Option<String> {
    let images: Vec<&Value> = list.as_array().into_iter().flatten().collect();
    languages.iter().find_map(|lang| {
        images.iter().filter(|i| i["iso_639_1"].as_str() == *lang)
            .max_by_key(|i| tmdb_image_score(i))
            .and_then(|i| i["file_path"].as_str().map(str::to_owned))
    })
}

/// TMDB artwork and production details for a title; cached for the session. Returns None when no
/// TMDB API key is configured. Used strictly as fallback / gap-filler behind free keyless sources,
/// except for guaranteed textless (`iso_639_1: null`) backdrops which prevent streaming-service
/// ("Netflix" / "Prime Video") promotional banners.
pub async fn tmdb_artwork(m: &Media) -> Option<Artwork> {
    if !tmdb_ready() { return None; }
    let imdb = imdb_of(m);
    let cache_key = imdb.clone().unwrap_or_else(|| format!("{}|{}", m.name.to_lowercase(), m.first_year().unwrap_or(0)));
    let cache = ARTWORK.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok()?.get(&cache_key).cloned() { return Some(hit); }
    let by_imdb = match &imdb {
        Some(id) => tmdb(format!("find/{id}"), vec![("external_source", "imdb_id".into())]).await.and_then(|f| {
            f["movie_results"][0]["id"].as_i64().map(|i| ("movie", i)).or_else(|| f["tv_results"][0]["id"].as_i64().map(|i| ("tv", i)))
        }),
        None => None,
    };
    // No IMDb id (Kitsu anime, for one) or unknown to TMDB: search by name, accepting only a
    // result whose year agrees within one year, so another title's art is never borrowed.
    let found = match by_imdb { Some(x) => Some(x), None => tmdb_by_name(m).await };
    let (kind, id) = found?;
    // Images and videos in one request.
    let extra = if kind == "tv" { "images,videos,aggregate_credits,content_ratings" } else { "images,videos,credits,release_dates" };
    let details = tmdb(format!("{kind}/{id}"), vec![("append_to_response", extra.into()), ("include_image_language", "en,null".into())]).await.unwrap_or_default();
    let images = &details["images"];
    let mut videos: Vec<&Value> = details["videos"]["results"].as_array().into_iter().flatten()
        .filter(|v| v["site"] == "YouTube" && (v["type"] == "Trailer" || v["type"] == "Teaser")).collect();
    // The production's own videos first: official trailers, then official teasers, then the rest.
    videos.sort_by_key(|v| std::cmp::Reverse((v["official"].as_bool().unwrap_or(false), v["type"] == "Trailer", v["iso_639_1"] == "en", v["size"].as_i64().unwrap_or(0))));
    let trailers: Vec<String> = videos.iter().filter_map(|v| v["key"].as_str().map(str::to_owned)).collect();
    // Strict textless (`iso_639_1: null`) backdrop first — guaranteed free of Netflix/Prime banners.
    // Use w1280 (7x smaller than 4K original, 9x less GPU texture memory) for fast cold starts and
    // 120fps WebView hero animations while staying razor-sharp on tablets and desktops.
    let textless = best_image(&images["backdrops"], &[None]);
    let backdrop_clean = textless.is_some();
    let backdrop_path = textless.or_else(|| best_image(&images["backdrops"], &[Some("en")]));
    let backdrop = backdrop_path.as_ref().map(|p| format!("https://image.tmdb.org/t/p/w1280{p}"));
    let art = Artwork {
        genres: details["genres"].as_array().into_iter().flatten().filter_map(|g| g["name"].as_str()).map(super::canonical_genre).collect(),
        // Text-free backdrops first (no language), then English ones.
        backdrop: backdrop.clone(),
        backdrop_clean,
        logo: best_image(&images["logos"], &[Some("en"), None]).map(|p| format!("https://image.tmdb.org/t/p/w500{p}")),
        poster: best_image(&images["posters"], &[Some("en"), None]).or_else(|| details["poster_path"].as_str().map(str::to_owned))
            .map(|p| format!("https://image.tmdb.org/t/p/w780{p}")),
        trailers,
        tmdb: Some((kind.to_owned(), id)),
        collection: details["belongs_to_collection"]["id"].as_i64(),
        ..production(&details, kind, backdrop.as_deref())
    };
    if let Ok(mut c) = cache.lock() { cache_insert(&mut c, 800, cache_key, art.clone()); }
    Some(art)
}

/// Free-first artwork for a title across the whole app (Home, Library, Picks, Related):
/// BetterPosters (and TVmaze untouched originals for series) come first; when a TMDB API key is
/// configured, TMDB runs in parallel and fills ONLY what free sources lack — plus guaranteed
/// textless (`iso_639_1: null`) backdrops that eliminate Netflix/Prime Video promotional banners.
pub async fn artwork(m: &Media) -> Option<Artwork> {
    let (bp, tmdb) = tokio::join!(bp_artwork(m), tmdb_artwork(m));
    let free = match bp {
        Some(art) if art.backdrop.is_some() => Some(art),
        other => match tvmaze_backdrop_artwork(m).await {
            Some(tv) => Some(with_extras(other.unwrap_or_default(), tv)),
            None => other,
        },
    };
    match (free, tmdb) {
        (Some(f), Some(t)) => Some(with_extras(f, t)),
        (Some(f), None) => Some(f),
        (None, Some(t)) => Some(t),
        (None, None) => None,
    }
}

/// Shrink a TMDB backdrop URL to `w300` for the blurred 72px `.ambient` glow: visually identical
/// after blur, 16x smaller GPU texture, and decodes in ~10ms instead of stalling WebView frames.
pub fn ambient_url(url: &str) -> String {
    if url.contains("image.tmdb.org/t/p/") {
        url.replacen("/original/", "/w300/", 1)
            .replacen("/w1280/", "/w300/", 1)
            .replacen("/w780/", "/w300/", 1)
    } else {
        url.to_owned()
    }
}

static TITLE_ART_CACHE: OnceLock<Mutex<HashMap<String, Artwork>>> = OnceLock::new();

/// Clear session artwork and title metadata caches (called when the user toggles BetterPosters or updates TMDB key).
pub fn clear_artwork_caches() {
    if let Some(c) = TITLE_META_CACHE.get() { if let Ok(mut m) = c.lock() { m.clear(); } }
    if let Some(c) = TITLE_ART_CACHE.get() { if let Ok(mut m) = c.lock() { m.clear(); } }
    if let Some(c) = ARTWORK.get() { if let Ok(mut m) = c.lock() { m.clear(); } }
    if let Some(c) = BP_CACHE.get() { if let Ok(mut m) = c.lock() { m.clear(); } }
}

/// Synchronously read cached title hero artwork from this session so re-opening a title paints
/// its clean backdrop, logo, tagline, and videos on Frame 0 with zero loading state.
pub fn cached_title_artwork(m: &Media) -> Option<Artwork> {
    TITLE_ART_CACHE.get()?.lock().ok()?.get(&m.key()).cloned()
}

/// Synchronously read cached card artwork (Home Tonight's pick, Library week cards, Picks duel)
/// from session caches so returning to a screen paints artwork on Frame 0.
pub fn cached_artwork(m: &Media) -> Option<Artwork> {
    if let Some(hit) = cached_title_artwork(m) { return Some(hit); }
    let cache_key = imdb_of(m).unwrap_or_else(|| format!("{}|{}", m.name.to_lowercase(), m.first_year().unwrap_or(0)));
    ARTWORK.get()?.lock().ok()?.get(&cache_key).cloned()
}

/// Fast title hero artwork (~150-250ms): runs ONLY BetterPosters + optional TMDB gap-filler
/// (never blocked by slower TVmaze/AniList/Wikidata series calls, which stream in below the fold
/// via `open_series_free` and `open_wikidata` with free-first priority).
pub async fn title_artwork_fast(m: &Media) -> Option<Artwork> {
    if let Some(hit) = cached_title_artwork(m) {
        return Some(hit);
    }
    let (bp, tmdb) = tokio::join!(bp_artwork(m), tmdb_artwork(m));
    let out = match (bp, tmdb) {
        (Some(f), Some(t)) => Some(with_extras(f, t)),
        (Some(f), None) => Some(f),
        (None, Some(t)) => Some(t),
        (None, None) => None,
    };
    if let Some(ref art) = out {
        if let Ok(mut c) = TITLE_ART_CACHE.get_or_init(Default::default).lock() {
            cache_insert(&mut c, 400, m.key(), art.clone());
        }
    }
    out
}

/// Whether TMDB can be asked (a key is set).
pub fn tmdb_ready() -> bool { tmdb_key().is_some() }

pub(super) fn tvmaze_genres(s: &Value) -> Vec<String> {
    let mut genres: Vec<String> = s["genres"].as_array().into_iter().flatten().filter_map(Value::as_str)
        .map(|g| if g == "Science-Fiction" { "Sci-Fi".to_owned() } else { g.to_owned() }).collect();
    if s["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("animation") || t.eq_ignore_ascii_case("anime"))
        && !genres.iter().any(|g| g.eq_ignore_ascii_case("animation")) { genres.push("Animation".into()); }
    genres
}

pub(super) async fn tmdb_by_name(m: &Media) -> Option<(&'static str, i64)> {
    let v = tmdb("search/multi".into(), vec![("query", m.name.clone()), ("include_adult", "false".into())]).await?;
    let year = m.first_year();
    let want_tv = m.kind != "movie";
    let mut fits: Vec<(&'static str, i64, bool)> = v["results"].as_array().into_iter().flatten().filter_map(|r| {
        let kind = match r["media_type"].as_str()? { "movie" => "movie", "tv" => "tv", _ => return None };
        let date = r["release_date"].as_str().or(r["first_air_date"].as_str()).unwrap_or_default();
        let ry: Option<i32> = date.get(..4).and_then(|y| y.parse().ok());
        let year_ok = match (year, ry) { (Some(a), Some(b)) => (a - b).abs() <= 1, (None, _) => true, (Some(_), None) => false };
        year_ok.then_some((kind, r["id"].as_i64()?, (kind == "tv") == want_tv))
    }).collect();
    fits.sort_by_key(|f| !f.2);
    fits.first().map(|f| (f.0, f.1))
}

/// Episode stills for one season: episode number -> high-resolution image URL.
pub async fn season_stills(m: &Media, season: i64) -> HashMap<i64, String> {
    let (mut out, anime) = tokio::join!(tvmaze_stills(m, season), anime_stills(m, season));
    for (ep, url) in anime {
        out.entry(ep).or_insert(url);
    }
    if !tmdb_ready() { return out; }
    let Some(Artwork { tmdb: Some((kind, id)), .. }) = tmdb_artwork(m).await else { return out };
    if kind != "tv" { return out; }
    let Some(v) = tmdb(format!("tv/{id}/season/{season}"), vec![]).await else { return out };
    for e in v["episodes"].as_array().into_iter().flatten() {
        if let (Some(num), Some(path)) = (e["episode_number"].as_i64(), e["still_path"].as_str().filter(|p| !p.is_empty())) {
            out.entry(num).or_insert_with(|| format!("https://image.tmdb.org/t/p/w780{path}"));
        }
    }
    out
}
