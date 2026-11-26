//! The title page: a full page, not a pop-up. A film has one Play button that picks
//! its source the way the Android app does; a series or anime lists its seasons and
//! episodes, and tapping an episode plays it the same way. Opening the page never
//! plays anything and never starts P2P.
use crate::{backend::{self, Episode, Media, Source, SourceKind, TitleMeta}, icons, player::PlayRequest, PlayerStatus, Shell};
use dioxus::prelude::*;

#[derive(Clone, PartialEq)]
enum Load<T> { Loading, Ready(T), Failed(String) }

fn episodic(media: &Media) -> bool { media.kind != "movie" }

/// The manual chooser: which video it lists.
#[derive(Clone, PartialEq)]
struct Chooser { video: String, label: Option<String> }

#[derive(Clone, PartialEq)]
enum DownloadScope {
    Movie,
    Episode(Episode),
    Season { season_name: String, episodes: Vec<Episode> },
    FullShow { episodes: Vec<Episode> },
}

#[derive(Clone, PartialEq)]
struct DownloadPrompt {
    scope: DownloadScope,
    preselected_source: Option<Source>,
    preloaded_sources: Option<Vec<Source>>,
}

#[component]
pub fn TitlePage(media: Media) -> Element {
    let mut shell = use_context::<Shell>();
    let cached_meta = backend::cached_title_meta(&media);
    let init_season = cached_meta.as_ref().and_then(|m| {
        media.last_video_id.as_ref().and_then(|v| m.episodes.iter().find(|e| &e.id == v))
            .or_else(|| m.episodes.iter().find(|e| e.season > 0)).or(m.episodes.first()).map(|e| e.season)
    });
    let need_meta = cached_meta.is_none();
    let mut meta = use_signal(move || match cached_meta {
        Some(m) => Load::Ready(m),
        None => Load::Loading,
    });
    let mut season = use_signal(move || init_season);
    let mut liked = use_signal(|| media.liked);
    let mut saved = use_signal(|| media.saved);
    // Films: one "Already watched" switch (marked, or played through). Series: per episode.
    let mut watched = use_signal(|| media.watched);
    let mut seen = use_signal(std::collections::HashSet::<String>::new);
    // Up next is only named once the watched episodes are known (no "S1 E1" flashing first).
    let mut seen_ready = use_signal(|| false);
    let for_seen = media.clone();
    use_hook(move || { spawn(async move { seen.set(backend::watched_episodes(&for_seen).await); seen_ready.set(true); }); });
    // A title opened from a shelf, Search or Picks carries the add-on's data only: what you did
    // with it (saved, watched, liked) comes from the store.
    // A choice made before the answer arrives wins (`touched`).
    let mut touched = use_signal(|| false);
    let for_store = media.clone();
    use_hook(move || { spawn(async move {
        if let Some(m) = backend::stored(&for_store).await {
            if *touched.peek() { return; }
            saved.set(m.saved);
            liked.set(m.liked);
            if !episodic(&m) { watched.set(m.watched); }
        }
    }); });
    let mut chooser = use_signal(|| None::<Chooser>);
    let mut dl_modal = use_signal(|| None::<DownloadPrompt>);
    let mut save_note = use_signal(|| None::<String>);
    let mut logo_failed = use_signal(|| false);
    let mut failed_frames = use_signal(std::collections::HashSet::<String>::new);
    // Two-slot hero crossfade state:
    // - `shown`:    currently displayed (fully decoded) backdrop URL on top.
    // - `under`:    previous backdrop held solid underneath during a 1.4s crossfade.
    // - `seq`:      monotonically increases on every real image change so the incoming `top` slot
    //               always gets a fresh DOM key and replays its smooth animation cleanly.
    // - `top_ready`: true once the current `top` image has fired `onload` (never fades a half-decoded image).
    let mut seq = use_signal(|| 0usize);
    let mut top_url = use_signal(String::new);
    let mut under_url = use_signal(String::new);
    let mut top_ready = use_signal(|| false);
    // Grace window on cold start: let the clean widescreen backdrop + logo arrive (~150-450ms)
    // so the hero fades in ONCE with the real art instead of flashing a promotional backdrop or
    // plain text and swapping it half a second later.
    let mut allow_fallback = use_signal(|| false);
    use_hook(move || {
        spawn(async move {
            backend::sleep(600).await;
            allow_fallback.set(true);
        });
    });
    let mut busy = use_signal(|| None::<String>);
    // Its place in today's chart (BetterPosters), shown as a fact.
    let mut rank = use_signal(|| None::<u64>);
    let for_rank = media.clone();
    use_hook(move || { spawn(async move { rank.set(backend::today_rank(&for_rank).await); }); });
    // The backdrop dissolves through the production's own textless frames every 9 seconds.
    let mut frame = use_signal(|| 0usize);
    use_future(move || async move {
        let mut waited = 0;
        while !*top_ready.peek() && waited < 10_000 { backend::sleep(100).await; waited += 100; }
        loop { backend::sleep(9000).await; *frame.write() += 1; }
    });

    let for_meta = media.clone();
    use_hook(move || {
        if !need_meta { return; }
        spawn(async move {
            match backend::title_meta(&for_meta).await {
                Ok(m) => {
                    let start = for_meta.last_video_id.as_ref().and_then(|v| m.episodes.iter().find(|e| &e.id == v))
                        .or_else(|| m.episodes.iter().find(|e| e.season > 0)).or(m.episodes.first()).map(|e| e.season);
                    season.set(start);
                    meta.set(Load::Ready(m));
                }
                Err(e) => meta.set(Load::Failed(format!("{e:#}"))),
            }
        });
    });

    let episodes = move || match meta() { Load::Ready(m) => m.episodes, _ => Vec::new() };

    // What the Play button would start, asked of the add-ons now: Play then only checks sources.
    let for_ahead = media.clone();
    use_effect(move || {
        let id = if !episodic(&for_ahead) { Some(for_ahead.id.clone()) } else if seen_ready() { up_next(&episodes(), &for_ahead, &seen()) } else { None };
        if let Some(id) = id { backend::prefetch_sources(&for_ahead.kind, &id); }
    });

    // Play: the Android app's order, then the one-byte check on at most four candidates.
    let play_media = media.clone();
    let autoplay = use_callback(move |(video, label): (String, Option<String>)| {
        let media = play_media.clone();
        if busy.peek().is_some() { return; }
        let title = label.as_ref().map(|l| format!("{} {l}", media.name)).unwrap_or_else(|| media.name.clone());
        busy.set(Some(video.clone()));
        let ticket = crate::finding(shell, title.clone());
        // Season packs: a labelled size covers the whole season.
        let pack = label.as_ref().and_then(|_| {
            let list = episodes();
            let s = list.iter().find(|e| e.id == video)?.season;
            Some(list.iter().filter(|e| e.season == s).count())
        });
        let next = next_episode(&episodes(), &video);
        dioxus::core::spawn_forever(async move {
            let saved_lock = backend::load_binge_lock(&media);
            let pick = backend::autoplay_with_lock(
                &media.kind,
                &video,
                pack,
                &media.name,
                (media.kind == "movie").then(|| media.first_year()).flatten(),
                saved_lock.as_ref(),
            )
            .await;
            if let Ok(mut b) = busy.try_write() { *b = None; }
            if !crate::still_wanted(shell, ticket) { return; }
            // Nothing played: every source can be chosen by hand.
            if !play_pick(shell, &media, &video, &title, pick, pack, next.map(|e| (e.id.clone(), Some(e.label())))) {
                if let Ok(mut c) = chooser.try_write() { *c = Some(Chooser { video, label }); }
            }
        });
    });

    let (m1, m4) = (media.clone(), media.clone());
    // Like (1) or Not for me (-1); the same again takes it back. The buttons and the L and D keys.
    let rate = use_callback(move |value: i64| {
        let media = m1.clone();
        touched.set(true);
        let before = *liked.peek();
        let next = if before == value { 0 } else { value };
        liked.set(next);
        spawn(async move { if backend::feedback(&media, next).await.is_ok() { *shell.history_dirty.write() += 1; } else { liked.set(before); } });
    });
    // Save to the Library (S), and the "Already watched" switch (W): the button and the key alike.
    let for_save = media.clone();
    let toggle_save = use_callback(move |_: ()| {
        touched.set(true);
        let next = !*saved.peek();
        saved.set(next);
        let m = for_save.clone();
        spawn(async move { if backend::set_saved(&m, next).await.is_err() { saved.set(!next); } });
    });
    let for_watched = media.clone();
    let toggle_watched = use_callback(move |_: ()| {
        touched.set(true);
        let next = !*watched.peek();
        watched.set(next);
        let m = for_watched.clone();
        spawn(async move { if backend::set_watched(&m, next).await.is_err() { watched.set(!next); } else { *shell.history_dirty.write() += 1; } });
    });

    let art = crate::components::use_title_artwork(&media);
    // A series' next episode, or that it ended (TVmaze).
    let mut status = use_signal(|| None::<String>);
    let for_status = media.clone();
    use_hook(move || { if for_status.kind != "movie" { spawn(async move { status.set(backend::series_status(&for_status).await); }); } });
    // Keys from the app root: P play/resume, L like, D not for me, S save, W already watched, T trailer, O sources.
    let k_media = media.clone();
    use_effect(move || {
        let Some(key) = (shell.title_command)() else { return };
        shell.title_command.set(None);
        let film = !episodic(&k_media);
        match key {
            'p' => if film { autoplay.call((k_media.id.clone(), None)) }
                   else if let Some(id) = up_next(&episodes(), &k_media, &seen.peek()).filter(|_| *seen_ready.peek()) { autoplay.call((id.clone(), backend::episode_label(&id))) },
            'l' => rate.call(1),
            'd' => rate.call(-1),
            's' => toggle_save.call(()),
            'w' if film => toggle_watched.call(()),
            't' => if let Load::Ready(m) = &*meta.peek() {
                let videos = art.peek().clone().map(|a| a.videos).unwrap_or_default();
                if !m.trailers.is_empty() || !videos.is_empty() {
                    let about = crate::theatre::About::of(&k_media, Some(m), art.peek().as_ref());
                    crate::theatre::watch_videos(shell, about, videos, m.trailers.clone(), None);
                }
            },
            'o' if film => chooser.set(Some(Chooser { video: k_media.id.clone(), label: None })),
            _ => {}
        }
    });
    let facts = match meta() { Load::Ready(m) => Some(m), _ => None };
    let year = media.year.clone();
    let film = !episodic(&media);
    let logo = art().and_then(|a| a.logo)
        .or_else(|| facts.as_ref().map(|m| m.logo.clone()).filter(|l| !l.is_empty()))
        .or_else(|| media.raw["logo"].as_str().map(str::to_owned).filter(|l| !l.is_empty()));
    let backdrop = art().and_then(|a| a.backdrop).unwrap_or_else(|| media.backdrop().to_owned());
    let production = art().unwrap_or_default();
    // Combine add-on trailers with free/TMDB artwork trailers without blocking `title_meta`.
    let trailer = {
        let mut all = facts.as_ref().map(|m| m.trailers.clone()).unwrap_or_default();
        for key in &production.trailers {
            let t = backend::Trailer { youtube: Some(key.clone()), url: None };
            if !all.contains(&t) { all.push(t); }
        }
        (!all.is_empty() || !production.videos.is_empty()).then_some(all)
    };
    // Build the clean backdrop rotation list. Only use a true widescreen `media.background` as
    // fallback (never stretch a vertical `media.poster` across the 16:9 hero!). The poster still
    // seeds the blurred `.ambient` room glow immediately on cold start so the screen feels alive.
    let fallback_bg = use_hook(|| media.background.clone());
    let ambient_fallback = use_hook(|| media.backdrop().to_owned());
    let has_art_backdrop = production.backdrop.is_some();
    let mut frames: Vec<String> = if has_art_backdrop {
        let mut list = vec![backdrop.clone()];
        for s in &production.stills { list.push(s.clone()); }
        list
    } else if allow_fallback() && !fallback_bg.is_empty() {
        vec![fallback_bg.clone()]
    } else {
        Vec::new()
    };
    let mut unique = std::collections::HashSet::new();
    frames.retain(|f| unique.insert(f.clone()));
    {
        let broken = failed_frames.read();
        frames.retain(|f| !f.is_empty() && !broken.contains(f));
    }
    let active_idx = if frames.is_empty() { 0 } else { frame() % frames.len() };
    let desired = frames.get(active_idx).cloned().unwrap_or_default();
    let preload_next = if frames.len() > 1 {
        frames.get((active_idx + 1) % frames.len()).cloned().unwrap_or_default()
    } else {
        String::new()
    };
    // Promote `desired` into the two-slot crossfade whenever the target URL changes.
    use_effect(use_reactive!(|desired| {
        if desired.is_empty() { return; }
        let prev = top_url.peek().clone();
        if prev == desired { return; }
        if !prev.is_empty() && *top_ready.peek() {
            under_url.set(prev);
        }
        top_ready.set(false);
        top_url.set(desired);
        *seq.write() += 1;
    }));
    // The ambient glow follows the hero's backdrop (resized to w300 in main.rs for 120fps blur),
    // and can immediately warm the room with the poster's colors while the widescreen hero loads.
    let mut lamp = shell.lamp_source;
    let lamp_src = if has_art_backdrop { backdrop.clone() } else { ambient_fallback.clone() };
    use_effect(use_reactive!(|lamp_src| if !lamp_src.is_empty() && *lamp.peek() != lamp_src { lamp.set(lamp_src); }));
    // Only logos that read as white idents (see backend::idents).
    let mut idents = use_signal(Vec::<backend::Studio>::new);
    let studios = production.studios.clone();
    use_effect(use_reactive!(|studios| { spawn(async move { idents.set(backend::idents(studios).await); }); }));
    // Use card metadata immediately on Frame 0 so facts don't pop in after `title_meta`.
    let imdb_rating = facts.as_ref().map(|m| m.rating.clone()).filter(|r| !r.is_empty())
        .unwrap_or_else(|| backend::s(&media.raw, "imdbRating"));
    let runtime = facts.as_ref().map(|m| m.runtime.clone()).filter(|r| !r.is_empty())
        .or_else(|| Some(backend::s(&media.raw, "runtime")).filter(|r| !r.is_empty()));
    let cur_top = top_url();
    let cur_under = under_url();
    let cur_seq = seq();
    let cur_ready = top_ready();
    let is_initial = cur_under.is_empty();
    let logo_pending = art().is_none() && facts.is_none() && !allow_fallback();

    rsx! {
        article { class: "title-page", aria_label: "{media.name}",
            section { class: "title-hero",
                div { class: "title-art-stack",
                    if !cur_under.is_empty() {
                        for k in [format!("u-{cur_under}")] {
                            img {
                                key: "{k}",
                                class: "title-art under",
                                src: "{cur_under}",
                                alt: "",
                                decoding: "async",
                            }
                        }
                    }
                    if !cur_top.is_empty() {
                        for k in [format!("t-{cur_seq}")] {
                            img {
                                key: "{k}",
                                class: format!(
                                    "title-art top{}{}",
                                    if is_initial { " initial" } else { "" },
                                    if cur_ready { " ready" } else { "" }
                                ),
                                src: "{cur_top}",
                                alt: "",
                                decoding: "async",
                                onload: move |_| {
                                    let my_seq = cur_seq;
                                    top_ready.set(true);
                                    // Retire the old `under` layer after the 0.95s crossfade completes.
                                    if !under_url.peek().is_empty() {
                                        spawn(async move {
                                            backend::sleep(1050).await;
                                            if *seq.peek() == my_seq {
                                                under_url.set(String::new());
                                            }
                                        });
                                    }
                                },
                                onerror: {
                                    let failed_src = cur_top.clone();
                                    move |_| { failed_frames.write().insert(failed_src.clone()); }
                                },
                            }
                        }
                    }
                    if !preload_next.is_empty() && preload_next != cur_top && preload_next != cur_under {
                        for k in [format!("p-{preload_next}")] {
                            img {
                                key: "{k}",
                                class: "title-art preload",
                                src: "{preload_next}",
                                alt: "",
                                aria_hidden: "true",
                            }
                        }
                    }
                }
                div { class: "title-copy",
                    match logo.filter(|_| !logo_failed()) {
                        Some(src) => rsx! { img { class: "title-logo", src: "{src}", alt: "{media.name}", onerror: move |_| logo_failed.set(true) } },
                        None if logo_pending => rsx! { div { class: "title-logo", style: "height: 76px" } },
                        None => rsx! { h1 { class: "display title-name", "{media.name}" } },
                    }
                    if let Some(t) = production.tagline.clone() { p { class: "tagline", "{t}" } }
                    div { class: "facts",
                        if let Some(r) = rank() { span { class: "fact today", title: "Most watched today on TMDB (BetterPosters)", icons::Trophy {} span { class: "tnum", "#{r} today" } } }
                        crate::components::Scores { media: media.clone(), imdb: imdb_rating }
                        if let Some(r) = runtime {
                            span { class: "fact", title: "Runtime", icons::Clock {} span { "{r}" } }
                        }
                        if !year.is_empty() { span { class: "fact", title: "Released", icons::Calendar {} span { class: "tnum", "{year}" } } }
                        // A series that is on says when its next episode airs (or that it ended) in place of its type.
                        match status() {
                            Some(t) => rsx! { span { class: "fact", title: "Series (TVmaze)", icons::Tv {} span { "{t}" } } },
                            None => rsx! { span { class: "fact", title: "Type",
                                match media.kind.as_str() { "movie" => rsx! { icons::Film {} }, "series" => rsx! { icons::Tv {} }, _ => rsx! { icons::Blossom {} } }
                                span { "{backend::type_label(&media.kind)}" }
                            } },
                        }
                        if !media.genres.is_empty() { span { class: "fact genres", title: media.genres.iter().take(3).cloned().collect::<Vec<_>>().join(", "), icons::Tag {} span { {media.genres.iter().take(3).cloned().collect::<Vec<_>>().join(", ")} } } }
                        if let Some(c) = production.certificate.clone() { span { class: "fact cert", title: "Certificate", "{c}" } }
                    }
                    div { class: "title-actions",
                        // A series carries on where you are: the started episode, else the one after the last you watched.
                        if !film {
                            if let Some(next) = up_next(&episodes(), &media, &seen()).filter(|_| seen_ready()) {
                                button { class: "pill accent play-big", disabled: busy().is_some(),
                                    onclick: { let l = next.clone(); move |_| autoplay.call((l.clone(), backend::episode_label(&l))) },
                                    if busy().is_some() { span { class: "spinner", aria_hidden: "true" } span { "Finding source" } }
                                    else { icons::Play {} span { {
                                        let verb = if media.last_video_id.as_deref() == Some(next.as_str()) && backend::in_progress(&media) { "Resume" } else { "Play" };
                                        match backend::episode_label(&next) { Some(l) => format!("{verb} {l}"), None => verb.into() }
                                    } } }
                                }
                            }
                        }
                        if film {
                            button { class: "pill accent play-big", disabled: busy().is_some(),
                                onclick: { let id = media.id.clone(); move |_| autoplay.call((id.clone(), None)) },
                                if busy().is_some() { span { class: "spinner", aria_hidden: "true" } span { "Finding source" } }
                                else { icons::Play {} span { if backend::in_progress(&media) { "Resume" } else { "Play" } } }
                            }
                        }
                        icons::IconButton { label: if saved() { "Saved to Library (S)" } else { "Save to Library (S)" }, class: if saved() { "glass on" } else { "glass" },
                            pressed: saved(), onclick: move |_| toggle_save.call(()), icons::Bookmark {} }
                        icons::IconButton { label: if liked() > 0 { "Liked" } else { "Like" }, class: if liked() > 0 { "glass on" } else { "glass" },
                            pressed: liked() > 0, onclick: move |_| rate.call(1), icons::ThumbUp {} }
                        icons::IconButton { label: "Not for me", class: if liked() < 0 { "glass on" } else { "glass" },
                            pressed: liked() < 0, onclick: move |_| rate.call(-1), icons::ThumbDown {} }
                        if film {
                            button { class: if watched() { "pill glass watched-switch on" } else { "pill glass watched-switch" }, role: "switch", aria_checked: "{watched()}",
                                title: "Already watched (W)",
                                onclick: move |_| toggle_watched.call(()),
                                if watched() { icons::Check {} } else { icons::EyeOff {} }
                                span { "Already watched" }
                            }
                        }
                        if let Some(t) = trailer {
                            icons::IconButton { label: "Trailer and videos (T)", class: "glass",
                                onclick: { let (about, videos) = (crate::theatre::About::of(&media, facts.as_ref(), Some(&production)), production.videos.clone());
                                    move |_| crate::theatre::watch_videos(shell, about.clone(), videos.clone(), t.clone(), None) }, icons::Clapper {} }
                        }
                        if film {
                            button { class: "pill glass",
                                onclick: move |_| dl_modal.set(Some(DownloadPrompt {
                                    scope: DownloadScope::Movie,
                                    preselected_source: None,
                                    preloaded_sources: None,
                                })),
                                icons::Download {} span { "Save" }
                            }
                            icons::IconButton { label: "Choose a source", class: "glass", onclick: { let id = media.id.clone(); move |_| chooser.set(Some(Chooser { video: id.clone(), label: None })) }, icons::List {} }
                        } else if !episodes().is_empty() {
                            button { class: "pill glass",
                                onclick: move |_| {
                                    let all = episodes();
                                    let mut regular: Vec<Episode> = all.iter().filter(|e| e.season != 0).cloned().collect();
                                    if regular.is_empty() { regular = all; }
                                    dl_modal.set(Some(DownloadPrompt {
                                        scope: DownloadScope::FullShow { episodes: regular },
                                        preselected_source: None,
                                        preloaded_sources: None,
                                    }));
                                },
                                icons::Download {} span { "Save show" }
                            }
                        }
                    }
                    if let Some(note) = save_note() {
                        p { class: "check-note", role: "status", icons::Check {} span { "{note}" } }
                    }
                }
            }

            div { class: "title-body",
                if !media.description.is_empty() { p { class: "title-synopsis", "{media.description}" } }
                else if let Some(d) = facts.as_ref().map(|m| m.description.clone()).filter(|d| !d.is_empty()) { p { class: "title-synopsis", "{d}" } }
                // The studios and networks behind it, as quiet idents.
                if !idents().is_empty() {
                    div { class: "studios", aria_label: "Made by",
                        for st in idents() { img { key: "{st.logo}", src: "{st.logo}", alt: "{st.name}", title: "{st.name}" } }
                    }
                }

                match meta() {
                    Load::Loading => rsx! {
                        if !film { div { class: "rows", for i in 0..3 { div { key: "{i}", class: "row skeleton-row" } } } }
                    },
                    Load::Failed(e) => rsx! { div { class: "notice glass", span { class: "warn-icon", icons::Alert {} } span { "{e}" } } },
                    Load::Ready(m) => rsx! {
                        if !film { Episodes { list: m.episodes.clone(), media: media.clone(), season: season, last: media.last_video_id.clone(), busy: busy(), seen: seen,
                            posters: production.season_posters.clone(),
                            on_play: move |(id, label): (String, String)| autoplay.call((id, Some(label))),
                            on_sources: move |(id, label): (String, String)| chooser.set(Some(Chooser { video: id, label: Some(label) })),
                            on_download: move |prompt: DownloadPrompt| dl_modal.set(Some(prompt)) } }
                        Videos { videos: production.videos.clone(), about: crate::theatre::About::of(&media, Some(&m), Some(&production)), trailers: m.trailers.clone() }
                        Credits { meta: m.clone(), production: production.clone() }
                    },
                }
            }

            if let Some(c) = chooser() {
                for c in [c] {
                    SourceSheet {
                        key: "{c.video}",
                        media: m4.clone(),
                        chooser: c.clone(),
                        episodes: episodes(),
                        next: next_episode(&episodes(), &c.video).map(|e| (e.id.clone(), Some(e.label()))),
                        on_open_download: move |prompt: DownloadPrompt| {
                            chooser.set(None);
                            dl_modal.set(Some(prompt));
                        },
                        on_close: move |_| chooser.set(None),
                    }
                }
            }

            if let Some(prompt) = dl_modal() {
                DownloadModal {
                    media: media.clone(),
                    prompt,
                    on_saved: move |msg: String| {
                        save_note.set(Some(msg));
                        dl_modal.set(None);
                    },
                    on_close: move |_| dl_modal.set(None),
                }
            }
        }
    }
}

/// Where a series carries on: the episode you started, unless you have watched it; else the
/// first unwatched one after the furthest you watched; a series not begun starts at its first
/// regular episode. None once everything after the furthest watched episode is seen.
fn up_next(list: &[Episode], media: &Media, seen: &std::collections::HashSet<String>) -> Option<String> {
    if let Some(last) = media.last_video_id.as_ref().filter(|l| !seen.contains(*l) && backend::in_progress(media)) { return Some(last.clone()); }
    let start = list.iter().rposition(|e| e.season != 0 && seen.contains(&e.id)).map_or(0, |i| i + 1);
    list.iter().skip(start).find(|e| e.season != 0 && !seen.contains(&e.id)).map(|e| e.id.clone())
}

fn next_episode(list: &[Episode], video: &str) -> Option<Episode> {
    let at = list.iter().position(|e| e.id == video)?;
    list.get(at + 1).filter(|e| e.season != 0).cloned()
}

/// Start the platform player for a checked source and record progress for this video.
/// `rest`: the Play-order candidates after this one, for "Another source".
fn start(shell: Shell, media: &Media, video: &str, title: &str, src: Source, pack: Option<usize>, next: Option<(String, Option<String>)>) {
    start_with(shell, media, video, title, src, pack, next, Vec::new(), None, 0)
}

/// How many times an automatic choice may move on because its audio is not in your language.
const LANGUAGE_RETRIES: u8 = 2;

/// `at`: where the previous, failed source stopped, so the next one carries on from there.
/// `checks`: while above 0 (automatic choices), a file whose tagged audio lacks the Audio-language
/// setting's language makes way for the next source instead of turning subtitles on.
#[allow(clippy::too_many_arguments)]
fn start_with(mut shell: Shell, media: &Media, video: &str, title: &str, src: Source, pack: Option<usize>, next: Option<(String, Option<String>)>, mut rest: Vec<Source>, at: Option<i64>, checks: u8) {
    // The same file from another scraper would fail the same way.
    if !src.filename.is_empty() { rest.retain(|s| s.filename != src.filename); }
    let language_check = checks > 0 && rest.iter().any(backend::claims_language);
    let resume = at.filter(|p| *p > 0).unwrap_or_else(|| if backend::in_progress(media) && (media.last_video_id.as_deref() == Some(video) || media.kind == "movie") { media.position_ms } else { 0 });
    let full_title = title.to_owned();
    let subtitle = if video != media.id { media.name.clone() } else { media.meta_line(true) };
    let title = title.strip_prefix(&format!("{} ", media.name)).filter(|_| video != media.id).unwrap_or(title).to_owned();
    let next_video = next.as_ref().map(|(id, _)| (media.kind.clone(), id.clone()));
    let next_label = next.as_ref().and_then(|(_, label)| label.clone());
    // Save/update the series' bingeGroup lock on normal starts (manual chooser or initial episode),
    // never overwriting it when an automatic mid-stream `another` fallback (`at.is_some()`) triggers.
    let next_lock = if src.saved.is_none() && media.kind != "movie" && at.is_none() {
        let lock = backend::StreamMatcherLock::from_source_with_pack(&src, pack);
        backend::save_binge_lock(media, &lock);
        Some(lock)
    } else {
        backend::load_binge_lock(media)
    };
    let another = {
        let (media, video, full_title, next, rest_for_another) = (media.clone(), video.to_owned(), full_title.clone(), next.clone(), rest.clone());
        Box::new(move |shell: Shell, position: i64, language: bool| {
            let (media, video, full_title, next, rest) = (media.clone(), video.clone(), full_title.clone(), next.clone(), rest_for_another.clone());
            let ticket = crate::finding(shell, full_title.clone());
            dioxus::core::spawn_forever(async move {
                if let Some(first_saved_pos) = rest.iter().position(|s| s.saved.is_some()) {
                    let mut rem = rest;
                    let found = rem.remove(first_saved_pos);
                    if !crate::still_wanted(shell, ticket) { return; }
                    start_with(shell, &media, &video, &full_title, found, pack, next, rem, Some(position), 0);
                    return;
                }
                let ((claimed_direct, other_direct), all_torrents) = partition_fallback_sources(rest, language);
                let mut pick = backend::pick_source(claimed_direct, pack).await;
                if pick.best.is_none() {
                    let provider_status = pick.provider_status;
                    pick = backend::pick_source(other_direct, pack).await;
                    if pick.provider_status.is_none() { pick.provider_status = provider_status; }
                } else {
                    pick.rest.extend(other_direct);
                }
                if pick.best.is_none() {
                    let mut iter = all_torrents.into_iter();
                    pick.best = iter.next();
                    pick.rest = iter.collect();
                } else {
                    pick.rest.extend(all_torrents);
                }
                backend::dev_log(&format!("another source{}: {}", if language { " (language)" } else { "" },
                    pick.best.as_ref().map(backend::source_summary).unwrap_or_else(|| "nothing".into())));
                if !crate::still_wanted(shell, ticket) { return; }
                match pick.best {
                    Some(found) => start_with(shell, &media, &video, &full_title, found, pack, next, pick.rest, Some(position), if pick.audio_known { 0 } else { checks.saturating_sub(1) }),
                    None => { no_source(shell, pick.provider_status.as_deref()); open_page(shell, &media); }
                }
            });
        }) as crate::TryAnother
    };
    if let Some(saved) = src.saved.clone() {
        let record_media = media.clone();
        let record_video = (video != media.id).then(|| video.to_owned());
        let size = src.size.clone();
        let hdr = src.hdr.map(str::to_owned);
        let quality = src.quality.map(str::to_owned);
        if saved.remote.is_none() {
            let lock_for_next = next_lock;
            crate::start_playback_with(shell, PlayRequest {
                title, subtitle, url: saved.path.to_string_lossy().into_owned(), start_ms: resume,
                size, hdr, quality,
                record: Some((record_media.clone(), record_video)), has_next: next.is_some(), next_label, next_video, language_check: false,
                ..Default::default()
            }, next.map(|(id, label)| {
                Box::new(move |shell: Shell| play_next_with_lock(shell, record_media.clone(), id.clone(), label.clone(), lock_for_next.clone())) as Box<dyn Fn(Shell)>
            }), Some(another));
            return;
        }
        let ticket = *shell.play_ticket.peek();
        let video_owned = video.to_owned();
        let lock_for_next = next_lock;
        dioxus::core::spawn_forever(async move {
            match backend::saved_source_stream_url(&saved).await {
                Ok(url) => {
                    if !crate::still_wanted(shell, ticket) { return; }
                    crate::start_playback_with(shell, PlayRequest {
                        title, subtitle, url, start_ms: resume,
                        size, hdr, quality,
                        record: Some((record_media.clone(), record_video)), has_next: next.is_some(), next_label, next_video, language_check: false,
                        ..Default::default()
                    }, next.map(|(id, label)| {
                        Box::new(move |shell: Shell| play_next_with_lock(shell, record_media.clone(), id.clone(), label.clone(), lock_for_next.clone())) as Box<dyn Fn(Shell)>
                    }), Some(another));
                }
                Err(e) => {
                    if !crate::still_wanted(shell, ticket) { return; }
                    if let Some(next_src) = (!rest.is_empty()).then(|| rest.remove(0)) {
                        start_with(shell, &record_media, &video_owned, &full_title, next_src, pack, next, rest, Some(resume), checks);
                        return;
                    }
                    let mut sh = shell;
                    sh.player_status.set(Some(PlayerStatus::Failed(format!("{e:#}"))));
                }
            }
        });
        return;
    }
    if src.kind == SourceKind::Torrent {
        let Some(hash) = src.info_hash.clone() else {
            shell.player_status.set(Some(PlayerStatus::Failed("Torrent source has no infoHash".into())));
            return;
        };
        let ticket = *shell.play_ticket.peek();
        shell.player_status.set(Some(PlayerStatus::Connecting(full_title.clone())));
        let file_idx = src.file_idx;
        let trackers = src.tracker_sources.clone();
        let headers = src.headers.clone();
        let size = src.size.clone();
        let hdr = src.hdr.map(str::to_owned);
        let quality = src.quality.map(str::to_owned);
        let record_media = media.clone();
        let video_owned = video.to_owned();
        let record_video = (video != media.id).then(|| video.to_owned());
        let ep_hint = record_video.clone();
        let lock_for_next = next_lock;
        // Torrent bootstrap must outlive `SourceSheet` / `TitlePage` unmounting when selected.
        dioxus::core::spawn_forever(async move {
            match backend::prepare_torrent(&hash, file_idx, &trackers, ep_hint.as_deref()).await {
                Ok((url, lease_id)) => {
                    if !crate::still_wanted(shell, ticket) {
                        if let Some(lid) = lease_id { backend::stop_torrent_lease(lid); }
                        return;
                    }
                    crate::start_playback_with(shell, PlayRequest {
                        title, subtitle, url, start_ms: resume, headers,
                        size, hdr, quality,
                        record: Some((record_media.clone(), record_video)), has_next: next.is_some(), next_label, next_video, language_check,
                        is_torrent: true, torrent_lease: lease_id,
                    }, next.map(|(id, label)| {
                        Box::new(move |shell: Shell| play_next_with_lock(shell, record_media.clone(), id.clone(), label.clone(), lock_for_next.clone())) as Box<dyn Fn(Shell)>
                    }), Some(another));
                }
                Err(e) => {
                    if !crate::still_wanted(shell, ticket) { return; }
                    backend::dev_log(&format!("torrent prepare failed: {e:#}"));
                    if let Some(next_src) = (!rest.is_empty()).then(|| rest.remove(0)) {
                        start_with(shell, &record_media, &video_owned, &full_title, next_src, pack, next, rest, Some(resume), checks.saturating_sub(1));
                        return;
                    }
                    let mut sh = shell;
                    sh.player_status.set(Some(PlayerStatus::Failed(format!("{e:#}"))));
                }
            }
        });
        return;
    }
    let lock_for_next = next_lock;
    crate::start_playback_with(shell, PlayRequest {
        title, subtitle, url: src.url, start_ms: resume, headers: src.headers,
        size: src.size.clone(), hdr: src.hdr.map(str::to_owned), quality: src.quality.map(str::to_owned),
        record: Some((media.clone(), (video != media.id).then(|| video.to_owned()))), has_next: next.is_some(), next_label, next_video, language_check,
        ..Default::default()
    }, next.map(|(id, label)| {
        let media = media.clone();
        Box::new(move |shell: Shell| play_next_with_lock(shell, media.clone(), id.clone(), label.clone(), lock_for_next.clone())) as Box<dyn Fn(Shell)>
    }), Some(another));
}

/// Ask for the sources `resume_now` would play while its button is on screen: the film, or the
/// series' unfinished episode.
pub fn prefetch_resume(media: &Media) {
    let id = if media.kind == "movie" { Some(media.id.clone()) } else { media.last_video_id.clone().filter(|_| !media.video_watched) };
    if let Some(id) = id { backend::prefetch_sources(&media.kind, &id); }
}

/// Resume from Home or Continue watching: the film, or the series' last episode, starts at once
/// with the automatic choice. Without a known episode the title page opens instead.
pub fn resume_now(mut shell: Shell, media: Media) {
    if media.kind == "movie" { let id = media.id.clone(); play_next(shell, media, id, None); return; }
    match media.last_video_id.clone() {
        // The last episode is finished: carry on with the one after it, not that episode again.
        Some(video) if media.video_watched => {
            let ticket = crate::finding(shell, media.name.clone());
            dioxus::core::spawn_forever(async move {
                let list = backend::title_meta(&media).await.map(|m| m.episodes).unwrap_or_default();
                if !crate::still_wanted(shell, ticket) { return; }
                let next = next_episode(&list, &video).map(|e| e.id).unwrap_or(video);
                let label = backend::episode_label(&next);
                play_next(shell, media, next, label);
            });
        }
        Some(video) => {
            // The label comes from the video id's own season/episode numbers (series:season:episode).
            let label = backend::episode_label(&video);
            play_next(shell, media, video, label);
        }
        None => shell.opened.set(Some(media)),
    }
}

fn partition_fallback_sources(rest: Vec<Source>, language: bool) -> ((Vec<Source>, Vec<Source>), Vec<Source>) {
    let (direct, torrents): (Vec<Source>, Vec<Source>) = rest
        .into_iter()
        .filter(|s| s.kind == SourceKind::Direct || (s.kind == SourceKind::Torrent && s.info_hash.is_some()))
        .partition(|s| s.kind == SourceKind::Direct);
    let direct_groups = direct.into_iter().partition(|s| language && backend::claims_language(s));
    let (mut claimed_torrents, other_torrents): (Vec<Source>, Vec<Source>) =
        torrents.into_iter().partition(|s| language && backend::claims_language(s));
    claimed_torrents.extend(other_torrents);
    (direct_groups, claimed_torrents)
}

/// Play a saved local file or rclone remote/crypt stream immediately, keeping Next episode wired.
pub fn play_offline(shell: Shell, entry: backend::DownloadEntry) {
    let resume = if backend::in_progress(&entry.media)
        && (entry.media.last_video_id.as_deref() == Some(entry.video_id.as_str()) || entry.media.kind == "movie")
    {
        entry.media.position_ms
    } else {
        0
    };
    let source_tag = entry.remote.as_ref().map(|r| format!("Rclone ({r})")).unwrap_or_else(|| "Offline".into());
    let ticket = crate::finding(shell, entry.title.clone());
    dioxus::core::spawn_forever(async move {
        let after = if entry.media.kind == "movie" {
            None
        } else {
            let list = backend::title_meta(&entry.media).await.map(|m| m.episodes).unwrap_or_default();
            next_episode(&list, &entry.video_id).map(|e| (e.id.clone(), Some(e.label())))
        };
        let next_video = after.as_ref().map(|(id, _)| (entry.media.kind.clone(), id.clone()));
        let next_label = after.as_ref().and_then(|(_, l)| l.clone());
        let lock = backend::load_binge_lock(&entry.media);
        match backend::download_stream_url(&entry).await {
            Ok(url) => {
                if !crate::still_wanted(shell, ticket) { return; }
                let media_for_next = entry.media.clone();
                crate::start_playback(shell, PlayRequest {
                    title: entry.title,
                    subtitle: format!("{source_tag} · {}", entry.subtitle),
                    url,
                    start_ms: resume,
                    size: Some(backend::file_size_label(entry.bytes)),
                    record: Some((entry.media.clone(), (entry.video_id != entry.media.id).then_some(entry.video_id))),
                    has_next: after.is_some(),
                    next_label,
                    next_video,
                    ..Default::default()
                }, after.map(|(id, label)| {
                    Box::new(move |shell: Shell| play_next_with_lock(shell, media_for_next.clone(), id.clone(), label.clone(), lock.clone())) as Box<dyn Fn(Shell)>
                }));
            }
            Err(e) => {
                if !crate::still_wanted(shell, ticket) { return; }
                let mut sh = shell;
                sh.player_status.set(Some(PlayerStatus::Failed(format!("{e:#}"))));
            }
        }
    });
}

/// Play one video now with the automatic choice, without the page (Resume, Next episode).
fn play_next(shell: Shell, media: Media, video: String, label: Option<String>) {
    play_next_with_lock(shell, media, video, label, None);
}

fn play_next_with_lock(
    shell: Shell,
    media: Media,
    video: String,
    label: Option<String>,
    lock: Option<backend::StreamMatcherLock>,
) {
    let title = label.as_ref().map(|l| format!("{} {l}", media.name)).unwrap_or_else(|| media.name.clone());
    let ticket = crate::finding(shell, title.clone());
    dioxus::core::spawn_forever(async move {
        let list = if media.kind == "movie" { Vec::new() } else { backend::title_meta(&media).await.map(|m| m.episodes).unwrap_or_default() };
        let label = label.or_else(|| list.iter().find(|e| e.id == video).map(Episode::label));
        let title = label.as_ref().map(|l| format!("{} {l}", media.name)).unwrap_or_else(|| media.name.clone());
        let after = next_episode(&list, &video).map(|e| (e.id.clone(), Some(e.label())));
        let pack = list.iter().find(|e| e.id == video).map(|e| list.iter().filter(|x| x.season == e.season).count());
        let active_lock = lock.or_else(|| backend::load_binge_lock(&media));
        let pick = backend::autoplay_with_lock(
            &media.kind,
            &video,
            pack,
            &media.name,
            (media.kind == "movie").then(|| media.first_year()).flatten(),
            active_lock.as_ref(),
        )
        .await;
        if !crate::still_wanted(shell, ticket) { return; }
        // Nothing played: the title page, where every source can be chosen.
        if !play_pick(shell, &media, &video, &title, pick, pack, after) { open_page(shell, &media); }
    });
}

/// Play Play's automatic choice; false (having said why) when there is none.
fn play_pick(shell: Shell, media: &Media, video: &str, title: &str, pick: backend::Pick, pack: Option<usize>, next: Option<(String, Option<String>)>) -> bool {
    match pick.best {
        Some(src) => { start_with(shell, media, video, title, src, pack, next, pick.rest, None, if pick.audio_known { 0 } else { LANGUAGE_RETRIES }); true }
        None => { no_source(shell, pick.provider_status.as_deref()); false }
    }
}

/// Say why nothing could play (the player status line).
fn no_source(mut shell: Shell, provider_status: Option<&str>) {
    shell.player_status.set(Some(PlayerStatus::Failed(backend::no_source_message(provider_status).into())));
}

/// The title page, unless one is open already.
fn open_page(mut shell: Shell, media: &Media) {
    if shell.opened.peek().is_none() { shell.opened.set(Some(media.clone())); }
}

/// Episodes shown at once; a longer season gets range chips (divisible by 2, 3 and 4 columns).
const EPISODE_PAGE: usize = 24;

/// Season chips, then the season's episodes as a grid. Tapping an episode plays it with the
/// automatic choice; its list icon opens the manual chooser instead.
#[component]
fn Episodes(list: Vec<Episode>, media: Media, season: Signal<Option<i64>>, last: Option<String>, busy: Option<String>,
            seen: Signal<std::collections::HashSet<String>>,
            on_play: EventHandler<(String, String)>, on_sources: EventHandler<(String, String)>,
            on_download: EventHandler<DownloadPrompt>,
            #[props(default)] posters: std::collections::HashMap<i64, String>) -> Element {
    let mut shell = use_context::<Shell>();
    let mut season = season;
    // Free-first episode stills (TVmaze / ani.zip, then TMDB fallback) keyed by the active season.
    let mut stills = use_signal(|| (None::<i64>, std::collections::HashMap::<i64, String>::new()));
    // The range of a long season you chose, for that season; otherwise the one with your next episode.
    let mut range = use_signal(|| None::<(Option<i64>, usize)>);
    let for_stills = media.clone();
    use_effect(move || {
        let Some(n) = season() else { return };
        let m = for_stills.clone();
        spawn(async move {
            let got = backend::season_stills(&m, n).await;
            if *season.peek() == Some(n) {
                stills.set((Some(n), got));
            }
        });
    });
    let mut seasons: Vec<i64> = list.iter().map(|e| e.season).collect();
    seasons.sort_by_key(|s| if *s == 0 { i64::MAX } else { *s });
    seasons.dedup();
    let shown: Vec<Episode> = list.iter().filter(|e| Some(e.season) == season()).cloned().collect();
    // Marking an episode watched marks the ones before it in its season ("season 1 up to
    // episode 3"); unmarking clears it and the ones after. The series counts as watched once
    // every episode is.
    let mut seen = seen;
    let (for_mark, all) = (media.clone(), list.clone());
    let mut series_done = use_signal(|| media.watched);
    let mark = use_callback(move |(ids, on): (Vec<String>, bool)| {
        if ids.is_empty() { return; }
        let before = seen.peek().clone();
        seen.with_mut(|s| for id in &ids { if on { s.insert(id.clone()); } else { s.remove(id); } });
        let complete = backend::series_complete(&all,&seen.peek(),backend::now_secs());
        let m = for_mark.clone();
        let changed = complete != *series_done.peek();
        if changed { series_done.set(complete); }
        spawn(async move {
            if backend::set_episodes_watched(&m, ids.clone(), on).await.is_ok() {
                if changed { let _ = backend::set_watched(&m, complete).await; }
                *shell.history_dirty.write() += 1;
            } else {
                // Not saved: the marks go back to what they were.
                seen.with_mut(|s| for id in &ids { if before.contains(id) { s.insert(id.clone()); } else { s.remove(id); } });
                if changed { series_done.set(!complete); }
            }
        });
    });
    let season_ids: Vec<String> = shown.iter().map(|e| e.id.clone()).collect();
    let season_done = !season_ids.is_empty() && season_ids.iter().all(|id| seen.read().contains(id));
    let season_name = match season() { Some(0) => "Extras".to_string(), Some(n) => format!("Season {n}"), None => String::new() };
    // Where you are in this season: the episode you stopped in, else the one after the last you
    // watched, else the first you haven't seen.
    let next_index = { let s = seen.read(); let at = last.as_ref().and_then(|l| shown.iter().position(|e| &e.id == l));
        match at { Some(i) if !s.contains(&shown[i].id) => Some(i), Some(i) if i + 1 < shown.len() => Some(i + 1), _ => shown.iter().position(|e| !s.contains(&e.id)) } };
    let next_id = next_index.map(|i| shown[i].id.clone());
    let pages = shown.len().div_ceil(EPISODE_PAGE);
    let current = match range() { Some((s, p)) if s == season() && p < pages => p, _ => next_index.map(|i| i / EPISODE_PAGE).unwrap_or(0) };
    let visible: Vec<Episode> = shown.iter().skip(current * EPISODE_PAGE).take(EPISODE_PAGE).cloned().collect();
    rsx! {
        section { class: "episodes-block",
            // Each season by its own poster when the production has them, else a chip.
            if seasons.len() > 1 && seasons.iter().any(|n| posters.contains_key(n)) {
                div { class: "season-cards", role: "radiogroup", aria_label: "Season",
                    for n in seasons.clone() {
                        button { key: "{n}", class: if season() == Some(n) { "season-card on" } else { "season-card" },
                            role: "radio", aria_checked: "{season() == Some(n)}", onclick: move |_| season.set(Some(n)),
                            div { class: "sp",
                                if let Some(src) = posters.get(&n) {
                                    crate::components::FallbackImg {
                                        sources: vec![src.clone()],
                                        lazy: true,
                                        span { class: "display", if n == 0 { "Extras" } else { "{n}" } }
                                    }
                                }
                                else { span { class: "display", if n == 0 { "Extras" } else { "{n}" } } }
                            }
                            span { if n == 0 { "Extras" } else { "Season {n}" } }
                        }
                    }
                }
            } else if seasons.len() > 1 {
                div { class: "season-chips", role: "radiogroup", aria_label: "Season",
                    for n in seasons {
                        button { class: if season() == Some(n) { "chip-btn glass on small-chip" } else { "chip-btn glass small-chip" },
                            role: "radio", aria_checked: "{season() == Some(n)}", aria_label: if n == 0 { "Extras".to_string() } else { format!("Season {n}") },
                            onclick: move |_| season.set(Some(n)),
                            if n == 0 { icons::Sparkles {} } else { span { class: "tnum", "S{n}" } } }
                    }
                }
            }
            if !season_ids.is_empty() {
                div { class: "title-actions",
                    button { class: if season_done { "pill glass watched-switch on" } else { "pill glass watched-switch" }, role: "switch", aria_checked: "{season_done}",
                        onclick: { let ids = season_ids.clone(); move |_| mark.call((ids.clone(), !season_done)) },
                        if season_done { icons::Check {} } else { icons::EyeOff {} }
                        span { "{season_name} watched" }
                    }
                    if season().is_some() {
                        button { class: "pill glass",
                            onclick: {
                                let (eps, name) = (shown.clone(), season_name.clone());
                                move |_| {
                                    on_download.call(DownloadPrompt {
                                        scope: DownloadScope::Season {
                                            season_name: name.clone(),
                                            episodes: eps.clone(),
                                        },
                                        preselected_source: None,
                                        preloaded_sources: None,
                                    });
                                }
                            },
                            icons::Download {} span { "Save {season_name}" }
                        }
                    }
                }
            }
            if pages > 1 {
                // A long season in ranges of 24; the dot marks the range holding your next episode.
                div { class: "range-chips", role: "radiogroup", aria_label: "Episodes",
                    for p in 0..pages {
                        {
                            let chunk = &shown[p * EPISODE_PAGE..((p + 1) * EPISODE_PAGE).min(shown.len())];
                            let (first, to) = (chunk[0].number, chunk[chunk.len() - 1].number);
                            let here = next_index.is_some_and(|i| i / EPISODE_PAGE == p);
                            rsx! {
                                button { class: if p == current { "range on" } else { "range" }, role: "radio", aria_checked: "{p == current}",
                                    aria_label: "Episodes {first} to {to}", onclick: move |_| range.set(Some((season(), p))),
                                    span { class: "tnum", "{first}–{to}" }
                                    if here { i { class: "range-dot", title: "Up next is here" } }
                                }
                            }
                        }
                    }
                }
            }
            // Keyed by season and range, so a new page of episodes lifts in.
            for k in [format!("{:?}-{current}", season())] {
            div { key: "{k}", class: "ep-grid",
                for (i, e) in visible.iter().cloned().enumerate() {
                    {
                    let done = seen.read().contains(&e.id);
                    let is_last = last.as_deref() == Some(e.id.as_str());
                    let is_next = next_id.as_deref() == Some(e.id.as_str());
                    let ids: Vec<String> = shown.iter().filter(|x| if done { x.number >= e.number } else { x.number <= e.number }).map(|x| x.id.clone()).collect();
                    rsx! {
                    div { key: "{e.id}", style: "--i: {i.min(8)}",
                        class: format!("ep-tile{}{}{}", if is_next { " next" } else { "" }, if done { " seen" } else { "" }, if is_last { " last" } else { "" }),
                        button { class: "ep-main", disabled: busy.is_some(),
                            aria_label: "Play episode {e.number}: {e.title}",
                            onclick: { let (id, l) = (e.id.clone(), e.label()); move |_| on_play.call((id.clone(), l.clone())) },
                            div { class: "ep-still",
                                {
                                    let mut candidates = Vec::new();
                                    {
                                        let st = stills.read();
                                        if st.0 == Some(e.season) {
                                            if let Some(s) = st.1.get(&e.number) {
                                                if !s.is_empty() { candidates.push(s.clone()); }
                                            }
                                        }
                                    }
                                    if !e.thumbnail.is_empty() && !candidates.contains(&e.thumbnail) {
                                        candidates.push(e.thumbnail.clone());
                                    }
                                    rsx! {
                                        crate::components::FallbackImg { sources: candidates, lazy: true }
                                    }
                                }
                                if is_next { span { class: "ep-next", "Up next" } }
                                span { class: "ep-play", if busy.as_deref() == Some(e.id.as_str()) { span { class: "spinner" } } else { icons::Play {} } }
                            }
                            div { class: "ep-copy",
                                div { class: "ep-head",
                                    span { class: "tag q tnum", "E{e.number}" }
                                    span { class: "ep-name", "{e.title}" }
                                    if is_last { span { class: "lamp", title: "Last watched", icons::History {} } }
                                }
                                if !e.overview.is_empty() { p { class: "ep-overview clamp-2", "{e.overview}" } }
                            }
                        }
                        div { class: "ep-foot",
                            span { class: "ep-date dim tnum", if !e.released.is_empty() { "{backend::date_label(&e.released)}" } }
                            icons::IconButton { label: if done { format!("Watched E{}: unmark it and later episodes", e.number) } else { format!("Watched up to E{}", e.number) },
                                class: if done { "small ep-seen on" } else { "small ep-seen" }, pressed: done,
                                onclick: move |_| mark.call((ids.clone(), !done)), icons::Check {} }
                            icons::IconButton { label: "Save episode", class: "small",
                                onclick: {
                                    let ep = e.clone();
                                    move |_| {
                                        on_download.call(DownloadPrompt {
                                            scope: DownloadScope::Episode(ep.clone()),
                                            preselected_source: None,
                                            preloaded_sources: None,
                                        });
                                    }
                                },
                                icons::Download {} }
                            icons::IconButton { label: "Choose a source", class: "small",
                                onclick: { let (id, l) = (e.id.clone(), e.label()); move |_| on_sources.call((id.clone(), l.clone())) },
                                icons::List {} }
                        }
                    }
                    }
                    }
                }
            }
            }
        }
    }
}

#[component]
fn Credits(meta: TitleMeta, #[props(default)] production: backend::Artwork) -> Element {
    let mut shell = use_context::<Shell>();
    let row = |label: &'static str, icon: Element, names: Vec<String>| rsx! {
        if !names.is_empty() {
            div { class: "credit-row",
                span { class: "credit-icon", title: label, aria_label: label, {icon} }
                div { class: "credit-chips",
                    for n in names.into_iter().take(12) {
                        button { class: "name-chip glass", title: "Search {n}",
                            onclick: { let n = n.clone(); move |_| { shell.search_query.set(Some(n.clone())); shell.search_open.set(true); } }, "{n}" }
                    }
                }
            }
        }
    };
    let fact = |label: &'static str, icon: Element, value: String| rsx! {
        if !value.is_empty() { div { class: "credit-row", span { class: "credit-icon", title: label, aria_label: label, {icon} } span { class: "credit-text", "{value}" } } }
    };
    // The real cast, with their photos and parts, when TMDB (or TVmaze, Wikidata) has them; else
    // the add-on's names. The add-on's top-billed names lead, for sources that keep no billing order.
    let faces = production.cast.iter().any(|p| p.photo.is_some());
    let mut people = production.cast.clone();
    people.sort_by_key(|p| meta.cast.iter().position(|n| n.eq_ignore_ascii_case(&p.name)).unwrap_or(usize::MAX));
    people.truncate(15);
    // Who made it leads the row as cards of their own: a film's directors, a series' creators.
    let mut makers: Vec<backend::Person> = Vec::new();
    for (label, crew_people) in &production.crew {
        let role = match label.as_str() { "Directed by" => "Director", "Created by" => "Creator", _ => continue };
        for p in crew_people {
            if !makers.iter().any(|m| m.name == p.name) { makers.push(backend::Person { role: role.into(), ..p.clone() }); }
        }
    }
    let cards = faces || makers.iter().any(|p| p.photo.is_some());
    if !cards { makers.clear(); }
    // The rest of the crew as names (and the makers too when nobody has a photo); each one searches.
    let crew_rows: Vec<(String, Vec<backend::Person>)> = production.crew.iter()
        .filter(|(l, _)| !(cards && matches!(l.as_str(), "Directed by" | "Created by"))).cloned().collect();
    let crew = !crew_rows.is_empty();
    // The add-on's director and writers stay unless the crew list names them (a keyless source
    // may know only the composer).
    let has = |jobs: &[&str]| production.crew.iter().any(|(l, _)| jobs.contains(&l.as_str()));
    let (directed, written) = (has(&["Directed by", "Created by"]), has(&["Written by"]));
    let language = production.language.clone().unwrap_or_else(|| meta.language.clone());
    rsx! {
        if cards {
            section { class: "people-block",
                div { class: "group-head", span { class: "group-icon", icons::Users {} } span { class: "group-name", if makers.is_empty() { "Cast" } else { "Cast & crew" } } }
                div { class: "people",
                    for p in makers.into_iter().chain(if faces { people } else { Vec::new() }) {
                        button { key: "{p.role}|{p.name}", class: "person", title: "Search {p.name}",
                            onclick: { let n = p.name.clone(); move |_| { shell.search_query.set(Some(n.clone())); shell.search_open.set(true); } },
                            span { class: "face",
                                if let Some(src) = p.photo.clone() {
                                    crate::components::FallbackImg {
                                        sources: vec![src],
                                        lazy: true,
                                        span { class: "display", "{p.name.chars().next().unwrap_or(' ')}" }
                                    }
                                }
                                else { span { class: "display", "{p.name.chars().next().unwrap_or(' ')}" } }
                            }
                            span { class: "person-name", "{p.name}" }
                            if !p.role.is_empty() { span { class: "person-role dim", "{p.role}" } }
                        }
                    }
                }
            }
        }
        if crew {
            dl { class: "crew",
                for (label, crew_people) in crew_rows {
                    div { key: "{label}", dt { "{label}" }
                        dd {
                            for (i, p) in crew_people.into_iter().enumerate() {
                                span { key: "{p.name}",
                                    if i > 0 { ", " }
                                    button { class: "crew-name", title: "Search {p.name}",
                                        onclick: { let n = p.name.clone(); move |_| { shell.search_query.set(Some(n.clone())); shell.search_open.set(true); } }, "{p.name}" }
                                }
                            }
                        }
                    }
                }
            }
        }
        section { class: "credits",
            if !directed { {row("Director", rsx! { icons::Clapper {} }, meta.directors.clone())} }
            if !faces { {row("Cast", rsx! { icons::Users {} }, meta.cast.clone())} }
            if !written { {row("Writers", rsx! { icons::Pen {} }, meta.writers.clone())} }
            {fact("Released", rsx! { icons::Calendar {} }, meta.released.clone())}
            {fact("Country", rsx! { icons::Globe {} }, meta.country.clone())}
            {fact("Language", rsx! { icons::Speech {} }, language)}
            {fact("Awards", rsx! { icons::Trophy {} }, meta.awards.clone())}
        }
    }
}

/// The production's videos (trailers, featurettes, behind the scenes) as a row of thumbnails.
#[component]
fn Videos(videos: Vec<backend::Video>, about: crate::theatre::About, trailers: Vec<backend::Trailer>) -> Element {
    let shell = use_context::<Shell>();
    rsx! {
        if !videos.is_empty() {
            section { class: "videos",
                div { class: "group-head", span { class: "group-icon", icons::Clapper {} } span { class: "group-name", "Videos" } }
                div { class: "video-row",
                    for v in videos.iter().cloned() {
                        button { key: "{v.youtube}", class: "video", aria_label: "Play {v.name}",
                            onclick: { let (k, all, a, tr) = (v.youtube.clone(), videos.clone(), about.clone(), trailers.clone());
                                move |_| crate::theatre::watch_videos(shell, a.clone(), all.clone(), tr.clone(), Some(k.clone())) },
                            span { class: "video-thumb",
                                crate::components::FallbackImg {
                                    sources: vec![
                                        format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", v.youtube),
                                        format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", v.youtube),
                                    ],
                                    lazy: true,
                                }
                                span { class: "ep-play", icons::Play {} }
                            }
                            span { class: "video-name", "{v.name}" }
                            span { class: if v.official { "video-kind official" } else { "video-kind dim" },
                                {crate::theatre::Clip { youtube: String::new(), name: String::new(), kind: v.kind.clone(), official: v.official }.label()} }
                        }
                    }
                }
            }
        }
    }
}

/// Manual choice: every add-on's sources for one video, in a sheet from the bottom.
#[component]
fn SourceSheet(
    media: Media,
    chooser: Chooser,
    episodes: Vec<Episode>,
    next: Option<(String, Option<String>)>,
    on_open_download: EventHandler<DownloadPrompt>,
    on_close: EventHandler<()>,
) -> Element {
    let shell = use_context::<Shell>();
    let mut sources = use_signal(|| Load::<(Vec<Source>, Vec<String>)>::Loading);
    let mut expanded = use_signal(|| None::<usize>);
    let mut pressed_backdrop = use_signal(|| false);
    let (kind, video) = (media.kind.clone(), chooser.video.clone());
    use_hook(move || { spawn(async move {
        sources.set(match backend::sources(&kind, &video).await { Ok(r) => Load::Ready(r), Err(e) => Load::Failed(format!("{e:#}")) });
    }); });
    let title = chooser.label.as_ref().map(|l| format!("{} {l}", media.name)).unwrap_or_else(|| media.name.clone());
    let pack = episodes
        .iter()
        .find(|e| e.id == chooser.video)
        .map(|ep| episodes.iter().filter(|x| x.season == ep.season).count());
    let (m, t, v) = (media.clone(), title.clone(), chooser.video.clone());
    let play = use_callback(move |src: Source| { start(shell, &m, &v, &t, src, pack, next.clone()); on_close.call(()); });
    let (dm, dv, dlbl, eps_for_dl) = (media.clone(), chooser.video.clone(), chooser.label.clone(), episodes);
    let save_offline = use_callback(move |src: Source| {
        let preloaded = match &*sources.peek() {
            Load::Ready((list, _)) => Some(list.clone()),
            _ => None,
        };
        let scope = if dv == dm.id {
            DownloadScope::Movie
        } else {
            let ep = eps_for_dl.iter().find(|e| e.id == dv).cloned().unwrap_or_else(|| Episode {
                id: dv.clone(),
                season: 1,
                number: 1,
                title: dlbl.clone().unwrap_or_else(|| dv.clone()),
                thumbnail: String::new(),
                overview: String::new(),
                released: String::new(),
            });
            DownloadScope::Episode(ep)
        };
        on_open_download.call(DownloadPrompt {
            scope,
            preselected_source: Some(src),
            preloaded_sources: preloaded,
        });
    });
    rsx! {
        div { class: "scrim bottom",
            onpointerdown: move |_| pressed_backdrop.set(true),
            onclick: move |_| { if pressed_backdrop() { on_close.call(()); } pressed_backdrop.set(false); },
            section { class: "bottom-sheet glass", role: "dialog", aria_modal: "true", aria_label: "Sources for {title}",
                onpointerdown: move |e| e.stop_propagation(), onclick: move |e| e.stop_propagation(),
                div { class: "sheet-head",
                    span { class: "group-icon", icons::List {} }
                    span { class: "group-name", "{title}" }
                    if let Load::Ready((list, _)) = sources() { span { class: "tnum dim", "{list.len()}" } }
                    icons::IconButton { label: "Close", class: "glass", onclick: move |_| on_close.call(()), icons::Close {} }
                }
                div { class: "sheet-scroll",
                    match sources() {
                        Load::Loading => rsx! { div { class: "rows", for i in 0..4 { div { key: "{i}", class: "row skeleton-row" } } } },
                        Load::Failed(e) => rsx! { div { class: "notice glass", span { class: "warn-icon", icons::Alert {} } span { "{e}" } } },
                        Load::Ready((list, failed)) => rsx! {
                            if !failed.is_empty() { div { class: "notice glass", role: "alert", span { class: "warn-icon", icons::Alert {} } span { "{failed.join(\", \")} offline" } } }
                            if list.is_empty() { p { class: "dim", "No sources" } }
                            div { class: "rows",
                                for (i, src) in list.into_iter().enumerate() {
                                    SourceRow { key: "{i}", source: src.clone(), open: expanded() == Some(i),
                                        on_details: move |_| expanded.set(if expanded() == Some(i) { None } else { Some(i) }),
                                        on_play: move |s: Source| play.call(s),
                                        on_download: move |s: Source| save_offline.call(s) }
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

/// Unified Download Modal for Movie, single Episode, Season, and Full Show batch archiving.
#[component]
fn DownloadModal(
    media: Media,
    prompt: DownloadPrompt,
    on_saved: EventHandler<String>,
    on_close: EventHandler<()>,
) -> Element {
    let mut target = use_signal(backend::default_save_target);
    let mut remotes = use_signal(Vec::<rstream_rclone::RemoteInfo>::new);
    let mut pressed_backdrop = use_signal(|| false);
    let mut modal_err = use_signal(|| None::<String>);

    use_hook(move || {
        spawn(async move {
            if let Ok(list) = backend::list_rclone_remotes().await {
                remotes.set(list);
            }
        });
    });

    let is_batch = matches!(prompt.scope, DownloadScope::Season { .. } | DownloadScope::FullShow { .. });
    let batch_episodes: Vec<Episode> = match &prompt.scope {
        DownloadScope::Season { episodes, .. } | DownloadScope::FullShow { episodes } => episodes.clone(),
        _ => Vec::new(),
    };
    let missing_episodes: Vec<Episode> = if is_batch {
        backend::missing_episodes_for_batch(&media, &batch_episodes)
    } else {
        Vec::new()
    };

    let ref_ep: Option<Episode> = match &prompt.scope {
        DownloadScope::Movie => None,
        DownloadScope::Episode(ep) => Some(ep.clone()),
        DownloadScope::Season { episodes, .. } | DownloadScope::FullShow { episodes } => {
            missing_episodes.first().or_else(|| episodes.first()).cloned()
        }
    };

    let ref_video_id = ref_ep.as_ref().map(|e| e.id.clone()).unwrap_or_else(|| media.id.clone());
    let ref_code = ref_ep
        .as_ref()
        .map(|e| {
            let c = backend::episode_code_hint(e.season, e.number);
            if c.is_empty() { e.label() } else { c }
        })
        .unwrap_or_default();

    let header_title = match &prompt.scope {
        DownloadScope::FullShow { .. } => format!("Download Full Show — {}", media.name),
        DownloadScope::Season { season_name, .. } => format!("Download {season_name} — {}", media.name),
        DownloadScope::Episode(_) => format!("Download {ref_code} — {}", media.name),
        DownloadScope::Movie => format!("Download — {}", media.name),
    };
    let header_subtitle = if is_batch {
        "Pick ONE reference release below to lock bingeGroup & fallback release tags across all missing episodes."
    } else {
        "Pick a stream and destination below to archive this video for offline playback."
    };

    let preloaded = prompt.preloaded_sources.clone();
    let preselected = prompt.preselected_source.clone();
    let mut loaded_streams = use_signal(move || match preloaded {
        Some(list) => Load::Ready((list, Vec::<String>::new())),
        None => Load::Loading,
    });
    let mut selected_idx = use_signal(|| 0usize);

    let (fetch_kind, fetch_vid, has_preloaded, pre_for_hook) = (
        media.kind.clone(),
        ref_video_id.clone(),
        prompt.preloaded_sources.is_some(),
        preselected.clone(),
    );
    use_hook(move || {
        if has_preloaded {
            if let (Some(pre), Load::Ready((list, _))) = (&pre_for_hook, &*loaded_streams.peek()) {
                let playable: Vec<&Source> = list
                    .iter()
                    .filter(|s| s.saved.is_none() && (s.kind == SourceKind::Direct || (s.kind == SourceKind::Torrent && s.info_hash.is_some())))
                    .collect();
                if let Some(pos) = playable.iter().position(|s| *s == pre || (s.info_hash.is_some() && s.info_hash == pre.info_hash && s.file_idx == pre.file_idx)) {
                    selected_idx.set(pos);
                }
            }
            return;
        }
        spawn(async move {
            match backend::sources(&fetch_kind, &fetch_vid).await {
                Ok((list, failed)) => {
                    if let Some(pre) = &pre_for_hook {
                        let playable: Vec<&Source> = list
                            .iter()
                            .filter(|s| s.saved.is_none() && (s.kind == SourceKind::Direct || (s.kind == SourceKind::Torrent && s.info_hash.is_some())))
                            .collect();
                        if let Some(pos) = playable.iter().position(|s| *s == pre || (s.info_hash.is_some() && s.info_hash == pre.info_hash && s.file_idx == pre.file_idx)) {
                            selected_idx.set(pos);
                        }
                    }
                    loaded_streams.set(Load::Ready((list, failed)));
                }
                Err(e) => loaded_streams.set(Load::Failed(format!("{e:#}"))),
            }
        });
    });

    let playable_streams: Vec<Source> = match loaded_streams() {
        Load::Ready((list, _)) => {
            let mut filtered: Vec<Source> = list
                .into_iter()
                .filter(|s| s.saved.is_none() && (s.kind == SourceKind::Direct || (s.kind == SourceKind::Torrent && s.info_hash.is_some())))
                .collect();
            if let Some(pre) = preselected.as_ref().filter(|s| s.saved.is_none()) {
                let exists = filtered.iter().any(|s| s == pre || (s.info_hash.is_some() && s.info_hash == pre.info_hash && s.file_idx == pre.file_idx));
                if !exists {
                    filtered.insert(0, pre.clone());
                }
            }
            filtered
        }
        _ => Vec::new(),
    };

    let active_idx = if playable_streams.is_empty() { 0 } else { selected_idx().min(playable_streams.len() - 1) };
    let active_source = playable_streams.get(active_idx).cloned();

    let cur_target = target();
    let cur_remote_val = match &cur_target {
        backend::SaveTarget::Local => String::new(),
        backend::SaveTarget::Remote(r) => r.clone(),
    };
    let missing_saved_remote = (!cur_remote_val.is_empty() && !remotes.read().iter().any(|r| r.name == cur_remote_val))
        .then(|| cur_remote_val.clone());

    let dest_word = match &cur_target {
        backend::SaveTarget::Local => "Local",
        backend::SaveTarget::Remote(_) => "Cloud",
    };
    let confirm_label = if is_batch {
        format!("Queue {} Episode(s) to {dest_word}", missing_episodes.len())
    } else {
        format!("Save to {dest_word}")
    };
    let confirm_disabled = active_source.is_none() || (is_batch && missing_episodes.is_empty());

    let scope_for_confirm = prompt.scope.clone();
    let media_for_confirm = media.clone();
    let ref_ep_for_confirm = ref_ep.clone();
    let chosen_for_confirm = active_source.clone();

    rsx! {
        div {
            class: "scrim dl-modal-scrim",
            onpointerdown: move |_| pressed_backdrop.set(true),
            onclick: move |_| {
                if pressed_backdrop() { on_close.call(()); }
                pressed_backdrop.set(false);
            },
            section {
                class: "dl-modal glass",
                role: "dialog",
                aria_modal: "true",
                aria_label: "{header_title}",
                onpointerdown: move |e| e.stop_propagation(),
                onclick: move |e| e.stop_propagation(),

                div { class: "dl-modal-head",
                    span { class: "dl-head-icon", icons::Download {} }
                    div { class: "dl-head-copy",
                        h2 { class: "dl-head-title", "{header_title}" }
                        p { class: "dim dl-head-sub", "{header_subtitle}" }
                    }
                    button {
                        class: "bare dl-close-btn",
                        aria_label: "Close download modal",
                        onclick: move |_| on_close.call(()),
                        icons::Close {}
                    }
                }

                div { class: "dl-modal-body",
                    div { class: "dl-summary-card",
                        div { class: "dl-summary-copy",
                            if is_batch {
                                span { class: "dl-summary-title", "Missing Episodes to Archive: {missing_episodes.len()}" }
                                if let Some(ep) = &ref_ep {
                                    span { class: "dl-ref-line",
                                        "Reference streams loaded from {ref_code} — {ep.title}"
                                    }
                                }
                            } else if let Some(ep) = &ref_ep {
                                span { class: "dl-summary-title", "{ref_code} — {ep.title}" }
                                span { class: "dl-ref-line", "Single episode archive" }
                            } else {
                                span { class: "dl-summary-title", "{media.name}" }
                                span { class: "dl-ref-line", "Feature film archive" }
                            }
                        }
                        div { class: "dl-target-wrap",
                            label { class: "dl-target-label", "Target Remote:" }
                            select {
                                class: "dl-target-select",
                                aria_label: "Target Remote",
                                value: "{cur_remote_val}",
                                onkeydown: move |e| e.stop_propagation(),
                                onchange: move |e| {
                                    let v = e.value().trim().to_owned();
                                    target.set(if v.is_empty() { backend::SaveTarget::Local } else { backend::SaveTarget::Remote(v) });
                                },
                                option {
                                    value: "",
                                    selected: matches!(cur_target, backend::SaveTarget::Local),
                                    "Local Storage"
                                }
                                if let Some(saved) = missing_saved_remote {
                                    option { key: "saved-{saved}", value: "{saved}", selected: true, "{saved}" }
                                }
                                for r in remotes.read().iter() {
                                    option {
                                        key: "rem-{r.name}",
                                        value: "{r.name}",
                                        selected: cur_remote_val == r.name,
                                        "{r.name} ({r.kind})"
                                    }
                                }
                            }
                        }
                    }

                    if let Some(sel) = &active_source {
                        {
                            let lock = backend::StreamMatcherLock::from_source(sel);
                            let transport_tag = if lock.kind == SourceKind::Torrent {
                                "Transport: P2P Torrent"
                            } else {
                                "Transport: Direct HTTP"
                            };
                            let aud_tag = backend::audio_tag_of(sel);
                            rsx! {
                                div { class: "dl-matcher-card",
                                    div { class: "dl-matcher-head",
                                        span { class: "dl-matcher-title", "Episode Matcher Lock Preview" }
                                        if let Some(bg) = &lock.binge_group {
                                            span { class: "dl-bg-pill green", "bingeGroup: {bg}" }
                                        }
                                    }
                                    div { class: "dl-lock-tags",
                                        span { class: "dl-tag neutral", "{transport_tag}" }
                                        if let Some(q) = &lock.quality {
                                            span { class: "dl-tag indigo", "Res: {q}" }
                                        }
                                        if let Some(g) = &lock.release_group {
                                            span { class: "dl-tag purple", "Group: {g}" }
                                        }
                                        if let Some(c) = &lock.codec {
                                            span { class: "dl-tag cyan", "Codec: {c}" }
                                        }
                                        if let Some(a) = aud_tag {
                                            span { class: "dl-tag emerald", "{a}" }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if let Some(e) = modal_err() {
                        p { class: "addon-problem", role: "alert", icons::Alert {} span { "{e}" } }
                    }

                    match loaded_streams() {
                        Load::Loading => rsx! {
                            div { class: "rows",
                                for i in 0..3 { div { key: "sk-{i}", class: "row skeleton-row", style: "min-height: 92px;" } }
                            }
                        },
                        Load::Failed(e) => rsx! {
                            div { class: "notice glass", span { class: "warn-icon", icons::Alert {} } span { "{e}" } }
                        },
                        Load::Ready((_, failed)) => rsx! {
                            if !failed.is_empty() {
                                div { class: "notice glass", role: "alert",
                                    span { class: "warn-icon", icons::Alert {} }
                                    span { "{failed.join(\", \")} offline" }
                                }
                            }
                            if playable_streams.is_empty() {
                                p { class: "dim", "No downloadable streams available from installed add-ons." }
                            } else {
                                div { class: "dl-stream-list", role: "radiogroup", aria_label: "Reference streams",
                                    for (idx, src) in playable_streams.iter().enumerate() {
                                        {
                                            let is_sel = active_idx == idx;
                                            let transport_badge = match (src.kind, src.pack) {
                                                (SourceKind::Torrent, true) => "P2P / SEASON PACK",
                                                (SourceKind::Torrent, false) => "P2P",
                                                _ => "DIRECT",
                                            };
                                            let desc_lines: Vec<String> = src
                                                .description
                                                .lines()
                                                .map(str::trim)
                                                .filter(|l| !l.is_empty())
                                                .map(str::to_owned)
                                                .collect();
                                            let has_inline_stats = src.description.contains('👤') || src.description.contains('💾');
                                            let mut fallback_stats = Vec::new();
                                            if !has_inline_stats {
                                                if let Some(sd) = src.seeders { fallback_stats.push(format!("👤 {sd}")); }
                                                if let Some(sz) = &src.size { fallback_stats.push(format!("💾 {sz}")); }
                                                if let Some(ix) = &src.indexer { fallback_stats.push(format!("⚙ {ix}")); }
                                            }
                                            let fallback_stat_line = fallback_stats.join("   ");
                                            rsx! {
                                                button {
                                                    key: "dl-s-{idx}",
                                                    class: if is_sel { "dl-stream-card on" } else { "dl-stream-card" },
                                                    r#type: "button",
                                                    role: "radio",
                                                    aria_checked: "{is_sel}",
                                                    onclick: move |_| selected_idx.set(idx),
                                                    div { class: "dl-card-top",
                                                        div { class: "dl-card-addon",
                                                            span { class: "dl-addon-name", "{src.addon}" }
                                                            span { class: "dl-addon-res", "{src.quality.unwrap_or(\"Stream\")}" }
                                                        }
                                                        span { class: "dl-transport-badge", "{transport_badge}" }
                                                        if let Some(bg) = &src.binge_group {
                                                            span { class: "dl-bg-pill", "{bg}" }
                                                        }
                                                        span { class: "topbar-spacer" }
                                                        span { class: if is_sel { "dl-radio on" } else { "dl-radio" }, aria_hidden: "true" }
                                                    }
                                                    div { class: "dl-card-lines",
                                                        if desc_lines.is_empty() {
                                                            if !src.filename.is_empty() {
                                                                p { class: "dl-line", "{src.filename}" }
                                                            } else if !src.name.is_empty() {
                                                                p { class: "dl-line", "{src.name}" }
                                                            }
                                                        } else {
                                                            for (li, line) in desc_lines.iter().enumerate() {
                                                                p { key: "ln-{li}", class: "dl-line", "{line}" }
                                                            }
                                                        }
                                                        if !fallback_stat_line.is_empty() {
                                                            p { class: "dl-line dim tnum", "{fallback_stat_line}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                    }
                }

                div { class: "dl-modal-foot",
                    button {
                        class: "pill glass",
                        r#type: "button",
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    button {
                        class: "pill accent dl-confirm-btn",
                        r#type: "button",
                        disabled: confirm_disabled,
                        onclick: move |_| {
                            let Some(chosen) = chosen_for_confirm.clone() else { return };
                            let tgt = target();
                            modal_err.set(None);
                            let sub = [chosen.quality.map(str::to_owned), chosen.hdr.map(str::to_owned)]
                                .into_iter()
                                .flatten()
                                .collect::<Vec<_>>()
                                .join(" ");
                            let subtitle = if sub.is_empty() { chosen.addon.clone() } else { sub };

                            match &scope_for_confirm {
                                DownloadScope::Movie => {
                                    match backend::save_source(
                                        media_for_confirm.clone(),
                                        media_for_confirm.id.clone(),
                                        media_for_confirm.name.clone(),
                                        subtitle,
                                        chosen,
                                        None,
                                        tgt.clone(),
                                    ) {
                                        Ok(()) => on_saved.call(format!("Saving to {} — see Library → Downloads.", tgt.label())),
                                        Err(e) => modal_err.set(Some(format!("Save couldn't start: {e}"))),
                                    }
                                }
                                DownloadScope::Episode(ep) => {
                                    let hint = backend::episode_code_hint(ep.season, ep.number);
                                    let ep_hint = (!hint.is_empty()).then_some(hint).or_else(|| Some(ep.id.clone()));
                                    let full_title = format!("{} {}", media_for_confirm.name, ep.label());
                                    match backend::save_source(
                                        media_for_confirm.clone(),
                                        ep.id.clone(),
                                        full_title,
                                        subtitle,
                                        chosen,
                                        ep_hint,
                                        tgt.clone(),
                                    ) {
                                        Ok(()) => on_saved.call(format!("Saving {} to {} — see Library → Downloads.", ep.label(), tgt.label())),
                                        Err(e) => modal_err.set(Some(format!("Save couldn't start: {e}"))),
                                    }
                                }
                                DownloadScope::Season { season_name, episodes } => {
                                    let Some(ref_episode) = ref_ep_for_confirm.as_ref() else { return };
                                    let count = backend::queue_batch_with_lock(
                                        &media_for_confirm,
                                        ref_episode,
                                        chosen,
                                        episodes,
                                        &tgt,
                                    );
                                    on_saved.call(format!("Queued {count} episode(s) of {season_name} to {} — see Library → Downloads.", tgt.label()));
                                }
                                DownloadScope::FullShow { episodes } => {
                                    let Some(ref_episode) = ref_ep_for_confirm.as_ref() else { return };
                                    let count = backend::queue_batch_with_lock(
                                        &media_for_confirm,
                                        ref_episode,
                                        chosen,
                                        episodes,
                                        &tgt,
                                    );
                                    on_saved.call(format!("Queued {count} episode(s) to {} — see Library → Downloads.", tgt.label()));
                                }
                            }
                        },
                        icons::Download {}
                        span { "{confirm_label}" }
                    }
                }
            }
        }
    }
}

/// One source: quality and HDR chips, the file, its size and add-on; Details shows
/// the add-on's own text verbatim. Torrents and external links are shown, not played.
#[component]
fn SourceRow(source: Source, open: bool, on_details: EventHandler<()>, on_play: EventHandler<Source>, on_download: EventHandler<Source>) -> Element {
    let playable = source.kind == SourceKind::Direct || (source.kind == SourceKind::Torrent && source.info_hash.is_some());
    let label = if !source.filename.is_empty() { source.filename.clone() }
        else { source.description.lines().next().unwrap_or(&source.name).to_owned() };
    let why = match source.kind {
        SourceKind::Direct => None,
        SourceKind::Torrent if source.info_hash.is_some() => None,
        SourceKind::Torrent => Some("Torrent source has no infoHash"),
        SourceKind::External => Some("Links that open outside RStream aren't supported yet"),
    };
    let src = source.clone();
    let src_dl = source.clone();
    rsx! {
        div { class: if playable { "source glass" } else { "source glass muted" },
            button { class: "source-main", disabled: !playable, title: why.unwrap_or(""),
                aria_label: "Play {label} from {source.addon}", onclick: move |_| on_play.call(src.clone()),
                div { class: "source-tags",
                    span { class: "tag q", {source.quality.unwrap_or("?")} }
                    if let Some(sv) = &source.saved {
                        span { class: "tag hdr", if sv.remote.is_some() { "CLOUD" } else { "SAVED" } }
                    }
                    if let Some(h) = source.hdr { span { class: "tag hdr", "{h}" } }
                }
                div { class: "source-text",
                    span { class: "source-file", "{label}" }
                    span { class: "source-meta dim", "{source.addon}" if let Some(size) = &source.size { span { class: "tnum", ", {size}" } } }
                }
                span { class: "source-go", match source.kind {
                    SourceKind::Direct => rsx! { icons::Play {} },
                    SourceKind::Torrent => rsx! { icons::Magnet {} },
                    SourceKind::External => rsx! { icons::Link {} },
                } }
            }
            if playable && source.saved.is_none() {
                icons::IconButton { label: "Download for offline playback", class: "small", onclick: move |_| on_download.call(src_dl.clone()), icons::Download {} }
            }
            icons::IconButton { label: if open { "Hide details" } else { "Details" }, class: "small", pressed: open, onclick: move |_| on_details.call(()), icons::Info {} }
            if open {
                div { class: "source-details",
                    if !source.name.is_empty() { p { "{source.name}" } }
                    if !source.description.is_empty() { p { class: "dim", "{source.description}" } }
                    if !source.filename.is_empty() { p { class: "dim", "{source.filename}" } }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_source(name: &str, kind: SourceKind, info_hash: Option<&str>) -> Source {
        Source {
            name: name.into(),
            description: name.into(),
            filename: name.into(),
            label: name.into(),
            url: if kind == SourceKind::Direct { format!("https://example.com/{name}") } else { String::new() },
            headers: Vec::new(),
            kind,
            quality: Some("1080p"),
            hdr: None,
            size: None,
            size_bytes: None,
            declared_size: false,
            labelled_bytes: None,
            addon: "test".into(),
            pack: false,
            info_hash: info_hash.map(str::to_owned),
            file_idx: None,
            seeders: Some(10),
            tracker_sources: Vec::new(),
            binge_group: None,
            indexer: None,
            off_title: false,
            picture: None,
            saved: None,
        }
    }

    #[test]
    fn fallback_partition_keeps_direct_before_torrents_without_duplicating_torrents() {
        let rest = vec![
            dummy_source("Movie.2024.1080p.TorrentA", SourceKind::Torrent, Some("1111111111111111111111111111111111111111")),
            dummy_source("Movie.2024.1080p.DirectA", SourceKind::Direct, None),
            dummy_source("Movie.2024.1080p.NoHash", SourceKind::Torrent, None),
            dummy_source("Movie.2024.1080p.DirectB", SourceKind::Direct, None),
            dummy_source("Movie.2024.1080p.TorrentB", SourceKind::Torrent, Some("2222222222222222222222222222222222222222")),
        ];
        let ((claimed_direct, other_direct), torrents) = partition_fallback_sources(rest, false);
        assert!(claimed_direct.is_empty());
        assert_eq!(other_direct.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec![
            "Movie.2024.1080p.DirectA",
            "Movie.2024.1080p.DirectB",
        ]);
        assert_eq!(torrents.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec![
            "Movie.2024.1080p.TorrentA",
            "Movie.2024.1080p.TorrentB",
        ]);
    }
}
