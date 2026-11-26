//! Home: each catalog as a quiet poster shelf. What you are watching lives in the Library, and
//! the last thing you played waits top right (main.rs, LastPlayed).
use crate::{backend::{self, CatalogRow, Media}, components::PosterCard, icons, player::PlayRequest, Shell};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq)]
enum Shelf { Loading, Ready(Vec<Media>), Failed(String) }

#[derive(Clone, Default)]
struct HomeSnapshot {
    rows: Vec<(CatalogRow, Shelf)>,
    library: Vec<Media>,
    at: i64,
}

static HOME_SNAPSHOT: std::sync::OnceLock<std::sync::Mutex<HomeSnapshot>> = std::sync::OnceLock::new();
const HOME_TTL_SECS: i64 = 300;

/// Drop cached Home shelves (called when the user switches BetterPosters/add-on settings).
pub fn invalidate_cache() {
    if let Some(m) = HOME_SNAPSHOT.get() {
        if let Ok(mut s) = m.lock() { *s = HomeSnapshot::default(); }
    }
}

fn read_home_snapshot() -> Option<HomeSnapshot> {
    let snap = HOME_SNAPSHOT.get()?.lock().ok()?.clone();
    (!snap.rows.is_empty()).then_some(snap)
}

fn write_home_snapshot(update: impl FnOnce(&mut HomeSnapshot)) {
    if let Ok(mut s) = HOME_SNAPSHOT.get_or_init(Default::default).lock() {
        update(&mut s);
    }
}

#[component]
pub fn Home() -> Element {
    let shell = use_context::<Shell>();
    let snap = read_home_snapshot();
    let fresh_rows = snap.as_ref().is_some_and(|s| backend::now_secs() - s.at < HOME_TTL_SECS
        && s.rows.iter().all(|(_, st)| matches!(st, Shelf::Ready(_))));
    let init_rows = snap.as_ref().map(|s| s.rows.clone()).unwrap_or_default();
    let init_lib = snap.as_ref().map(|s| s.library.clone()).unwrap_or_default();
    let mut rows = use_signal(move || init_rows);
    let mut rows_error = use_signal(|| None::<String>);
    let mut library = use_signal(move || init_lib);
    let mut loading = use_signal(move || snap.is_none());

    let load_row = move |row: CatalogRow| {
        spawn(async move {
            let result = backend::catalog(row.clone()).await;
            let next_state = match result { Ok(items) => Shelf::Ready(items), Err(e) => Shelf::Failed(format!("{e:#}")) };
            rows.with_mut(|all| if let Some(slot) = all.iter_mut().find(|(r, _)| r.key == row.key) {
                slot.1 = next_state.clone();
            });
            write_home_snapshot(|s| {
                if let Some(slot) = s.rows.iter_mut().find(|(r, _)| r.key == row.key) {
                    slot.1 = next_state;
                }
                s.at = backend::now_secs();
            });
        });
    };
    let mut opened = shell.opened;
    use_future(move || async move {
        if fresh_rows {
            if let Ok(items) = backend::library().await {
                library.set(items.clone());
                write_home_snapshot(|s| s.library = items);
            }
            loading.set(false);
            return;
        }
        let (lib_res, rows_res) = tokio::join!(backend::library(), backend::home_rows());
        if let Ok(items) = lib_res {
            if let Ok(want) = std::env::var("RSTREAM_OPEN") {
                let want = want.to_lowercase();
                if let Some(m) = items.iter().find(|m| m.name.to_lowercase().contains(&want)) { opened.set(Some(m.clone())); }
            }
            library.set(items.clone());
            write_home_snapshot(|s| s.library = items);
        }
        match rows_res {
            Ok(list) => {
                let prev: std::collections::HashMap<String, Shelf> = rows.peek().iter().cloned().map(|(r, st)| (r.key, st)).collect();
                let next: Vec<(CatalogRow, Shelf)> = list.iter().cloned().map(|r| {
                    let st = prev.get(&r.key).cloned().unwrap_or(Shelf::Loading);
                    (r, st)
                }).collect();
                rows.set(next.clone());
                write_home_snapshot(|s| { s.rows = next; s.at = backend::now_secs(); });
                for row in list { load_row(row); }
            }
            Err(e) => if rows.peek().is_empty() { rows_error.set(Some(format!("{e:#}"))); },
        }
        loading.set(false);
    });

    let continuing: Vec<Media> = library.read().iter().filter(|m| backend::in_progress(m)).cloned().collect();
    let mut new_for_you = use_signal(Vec::<Media>::new);
    use_effect(move || {
        let lib = library.read();
        let tracked: std::collections::HashSet<String> = lib.iter()
            .filter(|m| m.kind == "series" && (m.saved || backend::in_progress(m) || m.position_ms > 0))
            .map(|m| m.id.clone()).collect();
        if tracked.is_empty() { return; }
        let matched: Vec<Media> = rows.read().iter()
            .find(|(r, _)| r.id == "new-episodes")
            .and_then(|(_, st)| match st { Shelf::Ready(items) => Some(items), _ => None })
            .map(|items| items.iter().filter(|m| tracked.contains(&m.id)).cloned().collect())
            .unwrap_or_default();
        if *new_for_you.peek() != matched { new_for_you.set(matched); }
    });
    // The room takes its light from what you are watching.
    let mut lamp = shell.lamp_source;
    let first_catalog = rows.read().iter().find_map(|(_, st)| match st {
        Shelf::Ready(items) => items.first().map(|m| m.backdrop().to_owned()),
        _ => None,
    }).unwrap_or_default();
    let room = continuing.first().map(|m| m.backdrop().to_owned()).unwrap_or(first_catalog);
    use_effect(use_reactive!(|room| if *lamp.peek() != room { lamp.set(room); }));

    rsx! {
        div { class: "home",
            div { class: "bar-clearance" }
            div { class: "shelves",
                if !continuing.is_empty() {
                    section { class: "shelf",
                        SectionHeader { title: "Continue watching", meta: None }
                        div { class: "strip poster-strip",
                            for m in continuing { PosterCard { key: "cw-{m.key()}", media: m.clone() } }
                        }
                    }
                }

                if !new_for_you.read().is_empty() {
                    section { class: "shelf",
                        SectionHeader { title: "New for your shows", meta: Some("Tracked series with newly aired episodes".into()) }
                        div { class: "strip poster-strip",
                            for m in new_for_you.read().iter() { PosterCard { key: "nfy-{m.key()}", media: m.clone() } }
                        }
                    }
                }

                if loading() && rows.read().is_empty() { SkeletonStrip {} }
                if let Some(error) = rows_error() {
                    section { class: "shelf",
                        div { class: "row-error", span { class: "warn-icon", icons::Alert {} } span { "{error}" } }
                    }
                }
                for (row, state) in rows.read().iter() {
                    section { class: "shelf", key: "{row.key}",
                        SectionHeader { title: row.title.clone(), meta: row.subtitle.clone() }
                        match state {
                            Shelf::Loading => rsx! { SkeletonStrip {} },
                            Shelf::Failed(error) => rsx! {
                                div { class: "row-error",
                                    span { class: "warn-icon", icons::Alert {} }
                                    span { "{error}" }
                                    icons::IconButton { label: "Try again", class: "glass small", onclick: { let r = row.clone(); move |_| {
                                        rows.with_mut(|all| if let Some(s) = all.iter_mut().find(|(x, _)| x.key == r.key) { s.1 = Shelf::Loading; });
                                        load_row(r.clone());
                                    } }, icons::Refresh {} }
                                }
                            },
                            Shelf::Ready(items) if items.is_empty() => rsx! { p { class: "meta dim", "Empty" } },
                            Shelf::Ready(items) => rsx! {
                                div { class: "strip poster-strip",
                                    for m in items.iter().take(shelf_len()) { PosterCard { key: "{m.key()}", media: m.clone() } }
                                }
                            },
                        }
                    }
                }
                DemoNote {}
            }
        }
    }
}

fn shelf_len() -> usize {
    if crate::is_tv() { 36 } else { 40 }
}

#[component]
fn SectionHeader(title: String, #[props(default)] meta: Option<String>) -> Element {
    rsx! {
        div { class: "section-header",
            h2 { class: "display section", "{title}" }
            if let Some(m) = meta { p { class: "micro dim", "{m}" } }
        }
    }
}

#[component]
fn SkeletonStrip() -> Element {
    rsx! {
        div { class: "strip poster-strip", aria_hidden: "true",
            for i in 0..6 { div { key: "{i}", class: "card", div { class: "card-art tall skeleton-card" } } }
        }
    }
}

/// The legal player test; here it opens libmpv.
#[component]
fn DemoNote() -> Element {
    let shell = use_context::<Shell>();
    rsx! {
        div { class: "demo",
            button { class: "demo-row glass", aria_label: "Test the player with the Sintel trailer",
                onclick: move |_| crate::start_playback(shell, PlayRequest {
                    title: "Sintel trailer".into(), subtitle: "Test player".into(), url: backend::DEMO_STREAM.into(), ..Default::default() }, None),
                span { class: "lamp", icons::PlayCircle {} }
                span { class: "demo-copy",
                    span { class: "body-strong", "Test player" }
                    span { class: "credit dim", "Sintel, CC BY 3.0 Blender Foundation" }
                }
            }
        }
    }
}
