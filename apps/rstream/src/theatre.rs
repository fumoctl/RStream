//! The in-app theatre: a title's trailers and other videos as the official YouTube embed, driven
//! by its player API, with what the title is beside it and every video as a thumbnail to switch to.
use crate::{backend::{self, Media}, icons, open_external, start_playback, player, Shell};
use dioxus::prelude::*;

/// One video in the theatre: a YouTube id, its name and kind ("Trailer", "Teaser", ...).
#[derive(Clone, Debug, PartialEq)]
pub struct Clip { pub youtube: String, pub name: String, pub kind: String, pub official: bool }

/// The in-app video player: whose videos they are, the videos and the one playing.
#[derive(Clone, Debug, PartialEq)]
pub struct Theatre { pub about: About, pub clips: Vec<Clip>, pub at: usize }

/// The title the videos belong to, from what its page or card already loaded: its logo and
/// backdrop, year, runtime, certificate and genres, rating, tagline, director and leading cast.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct About {
    pub title: String, pub backdrop: String, pub logo: Option<String>,
    pub facts: Vec<String>, pub rating: Option<String>, pub tagline: Option<String>,
    pub director: Option<(String, Vec<String>)>, pub cast: Vec<String>,
}

impl About {
    pub fn of(media: &Media, meta: Option<&backend::TitleMeta>, art: Option<&backend::Artwork>) -> Self {
        let filled = |s: &str| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
        let genres: Vec<String> = media.genres.iter().take(2).cloned().collect();
        let facts = [filled(&media.year), meta.and_then(|m| filled(&m.runtime)), art.and_then(|a| a.certificate.clone()),
            Some(genres.join(", ")).filter(|g| !g.is_empty())].into_iter().flatten().collect();
        // Key crew from TMDB (directed or created by), else the add-on's directors.
        let director = art.and_then(|a| a.crew.iter().find(|(role, _)| role.starts_with("Directed") || role.starts_with("Created"))
                .map(|(role, people)| (role.clone(), people.iter().map(|p| p.name.clone()).collect())))
            .or_else(|| meta.filter(|m| !m.directors.is_empty()).map(|m| ("Directed by".to_owned(), m.directors.iter().take(2).cloned().collect())));
        let cast: Vec<String> = art.map(|a| a.cast.iter().take(4).map(|p| p.name.clone()).collect::<Vec<_>>()).filter(|c| !c.is_empty())
            .or_else(|| meta.map(|m| m.cast.iter().take(4).cloned().collect())).unwrap_or_default();
        About {
            title: media.name.clone(),
            backdrop: art.and_then(|a| a.backdrop.clone()).unwrap_or_else(|| media.backdrop().to_owned()),
            logo: art.and_then(|a| a.logo.clone()).or_else(|| meta.and_then(|m| filled(&m.logo))),
            facts, rating: meta.and_then(|m| filled(&m.rating)), tagline: art.and_then(|a| a.tagline.clone()), director, cast,
        }
    }
}

impl Clip {
    /// What the clip is, in words: "Official trailer", "Teaser", "Behind the scenes".
    pub fn label(&self) -> String {
        let kind = if self.kind == "Behind the Scenes" { "Behind the scenes".to_owned() } else if self.kind.is_empty() { "Video".to_owned() } else { self.kind.clone() };
        if self.official && matches!(self.kind.as_str(), "Trailer" | "Teaser") { format!("Official {}", kind.to_lowercase()) } else { kind }
    }
}

/// A title's videos in the in-app theatre, starting at `start` (a YouTube id) or the best one.
/// TMDB's videos lead (official trailers first); the add-on's YouTube trailers follow as
/// alternatives. With no YouTube video at all, an add-on's direct trailer file plays in the player.
pub fn watch_videos(mut shell: Shell, about: About, videos: Vec<backend::Video>, trailers: Vec<backend::Trailer>, start: Option<String>) {
    let mut clips: Vec<Clip> = videos.into_iter().map(|v| Clip { youtube: v.youtube, name: v.name, kind: v.kind, official: v.official }).collect();
    for id in trailers.iter().filter_map(|t| t.youtube.clone()) {
        if !clips.iter().any(|c| c.youtube == id) {
            let n = clips.iter().filter(|c| c.kind == "Trailer").count();
            clips.push(Clip { youtube: id, name: if n == 0 { "Trailer".into() } else { format!("Trailer {}", n + 1) }, kind: "Trailer".into(), official: false });
        }
    }
    if clips.is_empty() {
        if let Some(url) = trailers.iter().find_map(|t| t.url.clone()) {
            start_playback(shell, player::PlayRequest { title: format!("{}: trailer", about.title), url, ..Default::default() }, None);
        }
        return;
    }
    let at = start.and_then(|s| clips.iter().position(|c| c.youtube == s)).unwrap_or(0);
    shell.trailer.set(Some(Theatre { about, clips, at }));
}

/// Drive the theatre's YouTube embed through its player API (postMessage). The embed reports its
/// state, time and mute back (`theatre_listen`), so toggles and seeks start from what it is doing.
pub fn command(command: &'static str) {
    document::eval(&format!(r#"(() => {{
        const f = document.querySelector('.theatre iframe'); if (!f || !f.contentWindow) return;
        const s = window.__veyloYt || (window.__veyloYt = {{}});
        const post = (func, args) => f.contentWindow.postMessage(JSON.stringify({{ event: 'command', func, args: args || [] }}), 'https://www.youtube-nocookie.com');
        switch ('{command}') {{
            case 'toggle': post(s.state === 1 ? 'pauseVideo' : 'playVideo'); s.state = s.state === 1 ? 2 : 1; break;
            case 'back': post('seekTo', [Math.max(0, (s.time || 0) - 10), true]); s.time = Math.max(0, (s.time || 0) - 10); break;
            case 'forward': post('seekTo', [(s.time || 0) + 10, true]); s.time = (s.time || 0) + 10; break;
            case 'mute': post(s.muted ? 'unMute' : 'mute'); s.muted = !s.muted; break;
            case 'full': {{ const go = f.requestFullscreen || f.webkitRequestFullscreen; if (go) go.call(f); break; }}
        }}
    }})()"#));
}

/// Listen to the embed's reports once per app, and handshake with the embed while the theatre is open
/// (stopping its interval as soon as the theatre closes so nothing polls in the background).
fn theatre_listen() {
    document::eval(r#"(() => {
        if (!window.__veyloYtListening) {
            window.__veyloYtListening = true;
            window.__veyloYt = {};
            window.addEventListener('message', e => {
                if (e.origin !== 'https://www.youtube-nocookie.com' && e.origin !== 'https://www.youtube.com') return;
                let d; try { d = typeof e.data === 'string' ? JSON.parse(e.data) : e.data; } catch (_) { return; }
                const s = window.__veyloYt;
                if (d && d.event === 'onStateChange') s.state = d.info;
                if (d && d.event === 'infoDelivery' && d.info) {
                    if ('playerState' in d.info) s.state = d.info.playerState;
                    if ('currentTime' in d.info) s.time = d.info.currentTime;
                    if ('muted' in d.info) s.muted = d.info.muted;
                }
            });
        }
        if (window.__veyloYtTimer) clearInterval(window.__veyloYtTimer);
        window.__veyloYtTimer = setInterval(() => {
            const f = document.querySelector('.theatre iframe');
            if (!f || !f.contentWindow) { clearInterval(window.__veyloYtTimer); window.__veyloYtTimer = null; return; }
            f.contentWindow.postMessage(JSON.stringify({ event: 'listening', id: 'veylo' }), 'https://www.youtube-nocookie.com');
        }, 1000);
    })()"#);
}

/// The theatre: the official YouTube embed large, what is playing and its kind, and every video of
/// the title as a strip of thumbnails to switch between. Nothing autoplays until it is opened, and
/// closing it stops it.
#[component]
pub fn TheatreSheet(theatre: Theatre) -> Element {
    let mut shell = use_context::<Shell>();
    let n = theatre.clips.len();
    let at = theatre.at.min(n.saturating_sub(1));
    let Some(clip) = theatre.clips.get(at).cloned() else { return rsx! {}; };
    let mut go = move |to: usize| shell.trailer.with_mut(|t| if let Some(t) = t { t.at = to; });
    let link = format!("https://www.youtube.com/watch?v={}", clip.youtube);
    let about = theatre.about.clone();
    let mut logo_failed = use_signal(|| false);
    use_hook(theatre_listen);
    rsx! {
        div { class: "scrim theatre-scrim", onclick: move |_| shell.trailer.set(None),
            if !about.backdrop.is_empty() { img { class: "theatre-glow", src: "{backend::ambient_url(&about.backdrop)}", alt: "", decoding: "async" } }
            section { class: "theatre glass", role: "dialog", aria_modal: "true", aria_label: "Videos: {about.title}", onclick: move |e| e.stop_propagation(),
                // The video is the main actor: it takes all the room the side column leaves.
                div { class: "theatre-stage",
                    div { class: "trailer-frame",
                        // A new video gets a new frame (a key only renews an element inside a list).
                        for id in [clip.youtube.clone()] {
                            iframe { key: "{id}", src: "https://www.youtube-nocookie.com/embed/{id}?autoplay=1&playsinline=1&rel=0&modestbranding=1&enablejsapi=1", title: "{clip.name}",
                                allow: "autoplay; encrypted-media; picture-in-picture; fullscreen", allowfullscreen: "true", referrerpolicy: "strict-origin-when-cross-origin" }
                        }
                    }
                }
                aside { class: "theatre-side",
                    div { class: "theatre-tools",
                        if n > 1 {
                            icons::IconButton { label: "Previous video (←)", class: "glass small", onclick: move |_| go((at + n - 1) % n), icons::ArrowLeft {} }
                            span { class: "theatre-count tnum", "{at + 1} / {n}" }
                            icons::IconButton { label: "Next video (→)", class: "glass small", onclick: move |_| go((at + 1) % n), icons::ArrowRight {} }
                        }
                        span { class: "theatre-spacer" }
                        icons::IconButton { label: "Open on YouTube", class: "glass small", onclick: move |_| open_external(&link), icons::Link {} }
                        icons::IconButton { label: "Close (Esc)", class: "glass small", onclick: move |_| shell.trailer.set(None), icons::Close {} }
                    }
                    div { class: "theatre-titles",
                        match about.logo.clone().filter(|_| !logo_failed()) {
                            Some(src) => rsx! { img { class: "theatre-logo", src: "{src}", alt: "{about.title}", onerror: move |_| logo_failed.set(true) } },
                            None => rsx! { h2 { class: "display theatre-title", "{about.title}" } },
                        }
                        div { class: "theatre-facts",
                            if let Some(r) = about.rating.clone() { span { class: "theatre-rating", icons::Star {} span { class: "tnum", "{r}" } } }
                            for f in about.facts.iter() { span { "{f}" } }
                        }
                    }
                    div { class: "theatre-now",
                        span { class: if clip.official { "theatre-kind official" } else { "theatre-kind" },
                            if clip.official { icons::Check {} } else { icons::Clapper {} } span { {clip.label()} } }
                        if !clip.name.is_empty() && clip.name != clip.label() { span { class: "theatre-name", "{clip.name}" } }
                    }
                    if let Some(t) = about.tagline.clone() { p { class: "theatre-tagline", "“{t}”" } }
                    if about.director.is_some() || !about.cast.is_empty() {
                        div { class: "theatre-credits",
                            if let Some((role, names)) = about.director.clone() { p { span { class: "dim", "{role} " } {names.join(", ")} } }
                            if !about.cast.is_empty() { p { span { class: "dim", "With " } {about.cast.join(", ")} } }
                        }
                    }
                    if n > 1 {
                        div { class: "theatre-list", role: "list", aria_label: "All videos",
                            for (i, c) in theatre.clips.iter().cloned().enumerate() {
                                button { key: "{c.youtube}", class: if i == at { "theatre-clip on" } else { "theatre-clip" }, role: "listitem",
                                    aria_current: if i == at { "true" } else { "false" }, aria_label: "{c.label()}: {c.name}", onclick: move |_| go(i),
                                    span { class: "theatre-thumb",
                                        img { src: "https://i.ytimg.com/vi/{c.youtube}/mqdefault.jpg", alt: "", loading: "lazy" }
                                        if i == at { span { class: "now-playing", icons::Play {} } }
                                        if i < 9 { kbd { class: "keycap clip-key", aria_hidden: "true", "{i + 1}" } }
                                    }
                                    span { class: "theatre-clip-text",
                                        span { class: "theatre-clip-name", "{c.name}" }
                                        span { class: if c.official { "theatre-clip-kind official" } else { "theatre-clip-kind" }, {c.label()} }
                                    }
                                }
                            }
                        }
                    }
                    // The keys, at a glance (? lists them all).
                    div { class: "theatre-keys", aria_hidden: "true",
                        span { kbd { class: "keycap", "Space" } "Play" }
                        span { kbd { class: "keycap", "J" } kbd { class: "keycap", "L" } "10 s" }
                        span { kbd { class: "keycap", "M" } "Mute" }
                        span { kbd { class: "keycap", "F" } "Full" }
                        if n > 1 { span { kbd { class: "keycap", "←" } kbd { class: "keycap", "→" } "Videos" } }
                    }
                }
            }
        }
    }
}
