//! Pieces more than one screen uses: images that fall back through a title's artwork, posters,
//! TMDB artwork for a title, critics' scores with their sources' logos, and the confirm-first
//! "Start over".
use crate::{backend::{self, Media}, icons, Shell};
use dioxus::prelude::*;

/// The first of `sources` that loads, each broken image giving way to the next; `children` when
/// none does. Remembers failed URLs so asynchronously arriving higher-priority sources load
/// immediately without re-trying a broken URL or flashing a broken image icon.
#[component]
pub fn FallbackImg(sources: Vec<String>, #[props(default)] lazy: bool, children: Element) -> Element {
    let mut failed = use_signal(std::collections::HashSet::<String>::new);
    let current = {
        let f = failed.read();
        sources.iter().find(|u| !u.is_empty() && !f.contains(*u)).cloned()
    };
    match current {
        Some(src) => rsx! {
            img {
                key: "{src}",
                src: "{src}",
                alt: "",
                loading: if lazy { "lazy" } else { "eager" },
                decoding: "async",
                onerror: move |_| { failed.write().insert(src.clone()); },
            }
        },
        None => children,
    }
}

/// Upright poster with the name under it (PosterCard on touch screens). A broken
/// image falls back to the other artwork, then to the initial: never an empty card.
#[component]
pub fn PosterCard(media: Media) -> Element {
    let mut shell = use_context::<Shell>();
    let fraction = backend::progress_fraction(&media);
    let done = media.watched;
    let m = media.clone();
    let pre = media.clone();
    rsx! {
        button { class: "card", aria_label: "Open {media.name}",
            onpointerdown: move |_| backend::prefetch_title(&pre),
            onclick: move |_| shell.opened.set(Some(m.clone())),
            div { class: "card-art tall",
                // The poster carries the title; only when no image works is the name written.
                FallbackImg { sources: backend::posters(&media), lazy: true, span { class: "initial display", "{media.name}" } }
                if done { span { class: "watched", aria_label: "Watched", icons::Check {} } }
                if fraction > 0.0 && !done { div { class: "card-progress", div { style: "width: {fraction * 100.0}%" } } }
            }
        }
    }
}

/// Artwork for a card (Tonight's pick, Library week cards, Picks duel): initializes synchronously
/// from session cache so revisiting a screen paints on Frame 0, otherwise loads once.
pub fn use_artwork(media: &Media) -> Signal<Option<backend::Artwork>> {
    let init = backend::cached_artwork(media);
    let need = init.is_none();
    let mut art = use_signal(move || init);
    let m = media.clone();
    use_hook(move || {
        if need {
            spawn(async move { if let Some(a) = backend::artwork(&m).await { art.set(Some(a)); } });
        }
    });
    art
}

/// The title page's artwork:
/// 1. Fast hero art (`title_artwork_fast`: BetterPosters + optional TMDB clean textless backdrop/logo)
///    arrives in ~150-250ms without waiting on series/Wikidata calls.
/// 2. Free series/anime art (`open_series_free`: TVmaze untouched season posters & billing-order
///    cast, ARM + AniList extraLarge season posters) merges with FREE-FIRST priority for season
///    posters, cast and crew while keeping the clean textless hero backdrop untouched.
/// 3. Slower Wikidata enrichment (`open_wikidata`) fills remaining studios, certificate and film
///    credits silently without touching the hero.
///
/// All three initialize synchronously from session caches so revisited titles paint on Frame 0.
pub fn use_title_artwork(media: &Media) -> Memo<Option<backend::Artwork>> {
    let init_hero = backend::cached_title_artwork(media);
    let init_series = backend::cached_series_free(media);
    let init_wiki = backend::cached_wikidata(media);
    let need_hero = init_hero.is_none();
    let need_series = init_series.is_none() && media.kind != "movie";
    let need_wiki = init_wiki.is_none();
    let mut hero = use_signal(move || init_hero);
    let mut series = use_signal(move || init_series);
    let mut wiki = use_signal(move || init_wiki);
    let (m1, m2, m3) = (media.clone(), media.clone(), media.clone());
    use_hook(move || {
        if need_hero {
            spawn(async move { if let Some(x) = backend::title_artwork_fast(&m1).await { hero.set(Some(x)); } });
        }
        if need_series {
            spawn(async move { if let Some(x) = backend::open_series_free(&m2).await { series.set(Some(x)); } });
        }
        if need_wiki {
            spawn(async move { if let Some(x) = backend::open_wikidata(&m3).await { wiki.set(Some(x)); } });
        }
    });
    use_memo(move || {
        // Free-first merge: TVmaze/AniList season posters & billing-order cast win over TMDB,
        // while verified clean textless hero backdrop & logo from `hero` always win over TVmaze.
        let combined = match (series(), hero()) {
            (Some(s), Some(h)) => {
                // Keep hero's backdrop/logo/tagline/videos/stills primary, free series' season_posters/cast/crew primary.
                let mut free_first = backend::with_extras(s, h.clone());
                if h.backdrop.is_some() {
                    free_first.backdrop = h.backdrop;
                    free_first.backdrop_clean = h.backdrop_clean;
                }
                if h.logo.is_some() { free_first.logo = h.logo; }
                Some(free_first)
            }
            (Some(s), None) => Some(s),
            (None, Some(h)) => Some(h),
            (None, None) => None,
        };
        match (combined, wiki()) {
            (Some(c), Some(w)) => Some(backend::with_extras(c, w)),
            (Some(c), None) => Some(c),
            (None, Some(w)) => Some(w),
            (None, None) => None,
        }
    })
}

fn svg_data(svg: &str) -> String {
    use base64::Engine;
    format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg))
}
// The scores' own logos (assets/ratings/NOTICE.md says where they come from).
static IMDB_LOGO: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| svg_data(include_str!("../assets/ratings/imdb.svg")));
static METACRITIC_LOGO: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| svg_data(include_str!("../assets/ratings/metacritic.svg")));
static FRESH_LOGO: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| svg_data(include_str!("../assets/ratings/rotten-tomatoes-fresh.svg")));
static ROTTEN_LOGO: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| svg_data(include_str!("../assets/ratings/rotten-tomatoes-rotten.svg")));

/// A title's scores in one fact, each beside its source's logo: IMDb's (from the add-on,
/// `imdb`), and where Wikidata has them Rotten Tomatoes' Tomatometer (the tomato from 60%, the
/// splat below, as on their site) and the Metascore. Key it by the title.
#[component]
pub fn Scores(media: Media, imdb: String) -> Element {
    let mut scores = use_signal(backend::Scores::default);
    let m = media.clone();
    use_hook(move || { spawn(async move { scores.set(backend::review_scores(&m).await); }); });
    let s = scores();
    let mut said = Vec::new();
    if !imdb.is_empty() { said.push(format!("IMDb {imdb}")); }
    if let Some(t) = s.tomatometer { said.push(format!("Rotten Tomatoes {t}%")); }
    if let Some(n) = s.metascore { said.push(format!("Metascore {n}")); }
    if said.is_empty() { return rsx! {}; }
    let label = said.join(", ");
    rsx! {
        span { class: "fact scores", title: "{label}", aria_label: "{label}",
            if !imdb.is_empty() { span { class: "score", img { class: "score-logo imdb", src: IMDB_LOGO.as_str(), alt: "" } span { class: "tnum", "{imdb}" } } }
            if let Some(t) = s.tomatometer {
                span { class: "score", img { class: "score-logo", src: if t >= 60 { FRESH_LOGO.as_str() } else { ROTTEN_LOGO.as_str() }, alt: "" } span { class: "tnum", "{t}%" } }
            }
            if let Some(n) = s.metascore { span { class: "score", img { class: "score-logo", src: METACRITIC_LOGO.as_str(), alt: "" } span { class: "tnum", "{n}" } } }
        }
    }
}

/// "Start over" forgets everything Picks learned and can't be undone, so the first press only
/// asks. `labelled` is the Settings row's worded button; elsewhere an icon.
#[component]
pub fn StartOver(on_reset: EventHandler<()>, #[props(default)] labelled: bool) -> Element {
    let mut armed = use_signal(|| false);
    rsx! {
        if armed() {
            span { class: "confirm", role: "group", aria_label: "Forget what Picks learned?",
                button { class: "pill forget-confirm", onclick: move |_| { armed.set(false); on_reset.call(()) }, icons::Trash {} span { "Forget taste" } }
                icons::IconButton { label: "Keep it", class: "glass small", onclick: move |_| armed.set(false), icons::Close {} }
            }
        } else if labelled {
            button { class: "pill glass", onclick: move |_| armed.set(true), icons::Undo {} span { "Start over" } }
        } else {
            icons::IconButton { label: "Start over: forget what Picks learned", class: "glass small", onclick: move |_| armed.set(true), icons::Trash {} }
        }
    }
}

/// A poster with the name under it; used by Search.
#[component]
pub fn PosterButton(media: Media, on_press: EventHandler<Media>) -> Element {
    let m = media.clone();
    let pre = media.clone();
    rsx! {
        button { class: "poster", aria_label: "{media.name}",
            onpointerdown: move |_| backend::prefetch_title(&pre),
            onclick: move |_| on_press.call(m.clone()),
            div { class: "frame",
                FallbackImg { sources: backend::posters(&media), lazy: true, span { class: "frame-name", "{media.name}" } }
            }
            span { class: "poster-meta", "{media.meta_line(false)}" }
        }
    }
}
