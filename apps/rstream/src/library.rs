//! Library as a viewing history: the last week as big cards, then every month as a grid,
//! with filters, in-library search, and offline downloads.
use crate::{backend::{self, DownloadState, Media, Seen}, components::PosterCard, icons, Shell};
use dioxus::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LibFilter { All, Movies, Series, InProgress, Watched, Saved, Downloads }

impl LibFilter {
    const ALL: [LibFilter; 7] = [
        LibFilter::All, LibFilter::Movies, LibFilter::Series,
        LibFilter::InProgress, LibFilter::Watched, LibFilter::Saved, LibFilter::Downloads,
    ];
    fn label(self) -> &'static str {
        match self {
            LibFilter::All => "All",
            LibFilter::Movies => "Movies",
            LibFilter::Series => "Series",
            LibFilter::InProgress => "In progress",
            LibFilter::Watched => "Watched",
            LibFilter::Saved => "Saved",
            LibFilter::Downloads => "Downloads",
        }
    }
    fn matches(self, m: &Media) -> bool {
        match self {
            LibFilter::All => true,
            LibFilter::Movies => m.kind == "movie",
            LibFilter::Series => m.kind != "movie",
            LibFilter::InProgress => backend::in_progress(m),
            LibFilter::Watched => m.watched,
            LibFilter::Saved => m.saved,
            LibFilter::Downloads => false,
        }
    }
}

#[derive(Clone, Default)]
struct LibSnapshot {
    history: Option<Vec<Seen>>,
    saved: Option<Vec<Media>>,
}

static LIB_SNAPSHOT: std::sync::OnceLock<std::sync::Mutex<LibSnapshot>> = std::sync::OnceLock::new();

#[component]
pub fn Library() -> Element {
    // No title in front here: the accent goes back to neutral.
    let shell = use_context::<Shell>();
    let mut lamp = shell.lamp_source;
    use_hook(move || lamp.set(String::new()));
    let snap = LIB_SNAPSHOT.get().and_then(|m| m.lock().ok()).map(|s| s.clone()).unwrap_or_default();
    let mut history = use_signal(move || snap.history.map(Ok::<Vec<Seen>, String>));
    let mut saved = use_signal(move || snap.saved);
    // Refresh from SQLite silently in the background (and whenever `history_dirty` bumps) so
    // returning to Library paints on Frame 0 while staying reactive to watch/bookmark/sync updates.
    use_effect(move || {
        let _ = (shell.history_dirty)();
        spawn(async move {
            let (hist_res, lib_res) = tokio::join!(backend::viewing_history(), backend::library());
            match hist_res {
                Ok(list) => {
                    if let Ok(mut s) = LIB_SNAPSHOT.get_or_init(Default::default).lock() { s.history = Some(list.clone()); }
                    history.set(Some(Ok(list)));
                }
                Err(e) => if history.peek().is_none() { history.set(Some(Err(format!("{e:#}")))); },
            }
            if let Ok(all) = lib_res {
                let s_list: Vec<Media> = all.into_iter().filter(|m| m.saved).collect();
                if let Ok(mut s) = LIB_SNAPSHOT.get_or_init(Default::default).lock() { s.saved = Some(s_list.clone()); }
                saved.set(Some(s_list));
            }
        });
    });
    // Titles removed this visit vanish at once; the store already forgot them.
    let gone = use_signal(std::collections::HashSet::<String>::new);
    use_context_provider(|| Gone(gone));
    let mut filter = use_signal(|| LibFilter::All);
    let mut query = use_signal(String::new);
    let mut page_limit = use_signal(|| 100usize);
    let mut dl_refresh = use_signal(|| 0u64);
    let mut active_dl = use_signal(backend::active_downloads);
    // Sync cloud remote `.db/` logs on Library open and poll active downloads gently.
    use_hook(move || {
        spawn(async move {
            if let Ok(changed) = backend::sync_rclone_databases().await {
                if changed > 0 {
                    dl_refresh += 1;
                }
            }
        });
    });
    use_future(move || async move {
        loop {
            backend::sleep(1000).await;
            let now_active = backend::active_downloads();
            let any_gone = active_dl.peek().iter().any(|prev| !now_active.iter().any(|cur| cur.key == prev.key));
            if *active_dl.peek() != now_active { active_dl.set(now_active); }
            if any_gone { dl_refresh += 1; }
        }
    });
    let downloads_paged = use_memo(move || {
        let _ = dl_refresh();
        let q = query().trim().to_lowercase();
        backend::list_downloads_paged(&q, page_limit(), 0)
    });
    let q = query().trim().to_lowercase();
    let f = filter();
    let text_match = |name: &str| q.is_empty() || name.to_lowercase().contains(&q);
    let now = backend::now_secs();
    let (dl_list, total_dl) = downloads_paged();
    let active_list: Vec<backend::DownloadProgress> = active_dl.read().iter().filter(|d| text_match(&d.title)).cloned().collect();
    let show_downloads = f == LibFilter::Downloads || (f == LibFilter::All && (!dl_list.is_empty() || !active_list.is_empty()));
    let show_saved = matches!(f, LibFilter::All | LibFilter::Saved | LibFilter::Movies | LibFilter::Series);
    let show_history = f != LibFilter::Downloads && f != LibFilter::Saved;
    rsx! {
        div { class: "library",
            h1 { class: "display title", "Library" }
            div { class: "lib-toolbar",
                div { class: "lib-filters", role: "tablist", aria_label: "Filter library",
                    for opt in LibFilter::ALL {
                        button { key: "{opt.label()}", class: if f == opt { "pill accent small" } else { "pill glass small" },
                            role: "tab", aria_selected: "{f == opt}",
                            onclick: move |_| filter.set(opt),
                            "{opt.label()}"
                        }
                    }
                }
                div { class: "lib-search",
                    span { class: "sub-search-icon", aria_hidden: "true", icons::Search {} }
                    input { r#type: "search", placeholder: "Filter library…", aria_label: "Filter library", value: "{query}",
                        oninput: move |e| query.set(e.value()),
                        onkeydown: move |e| e.stop_propagation(),
                    }
                    if !q.is_empty() {
                        button { class: "bare sub-search-clear", aria_label: "Clear filter", onclick: move |_| query.set(String::new()), icons::Close {} }
                    }
                }
            }
            if show_downloads {
                section { class: "lib-section",
                    div { class: "group-head", span { class: "group-icon", icons::Download {} } span { class: "group-name", "Downloads" }
                        span { class: "tnum dim", "{total_dl}" }
                        if active_list.len() > 1 {
                            span { class: "topbar-spacer" }
                            button { class: "pill glass small", onclick: move |_| { backend::cancel_all_downloads(); active_dl.set(backend::active_downloads()); }, icons::Close {} span { "Cancel all" } }
                        }
                    }
                    if dl_list.is_empty() && active_list.is_empty() {
                        p { class: "dim", "No saved files yet. Open any title or Sources list and tap Save." }
                    }
                    div { class: "addon-list",
                        for job in active_list {
                            {
                                let pct = job.total.filter(|&t| t > 0).map(|t| (job.downloaded as f64 / t as f64 * 100.0).clamp(0.0, 100.0));
                                let status = match &job.state {
                                    DownloadState::Queued => format!("Queued · {}", job.target),
                                    DownloadState::Running => match (pct, job.total) {
                                        (Some(p), Some(t)) => format!("{p:.0}% · {} / {} · {}", backend::file_size_label(job.downloaded), backend::file_size_label(t), job.target),
                                        _ => format!("Saving… {} · {}", backend::file_size_label(job.downloaded), job.target),
                                    },
                                    DownloadState::Paused => match (pct, job.total) {
                                        (Some(p), Some(t)) => format!("Paused · {p:.0}% · {} / {} · {}", backend::file_size_label(job.downloaded), backend::file_size_label(t), job.target),
                                        _ if job.downloaded > 0 => format!("Paused · {} · {}", backend::file_size_label(job.downloaded), job.target),
                                        _ => format!("Paused · {}", job.target),
                                    },
                                    DownloadState::Failed(err) => format!("Failed ({}): {err}", job.target),
                                };
                                let is_running_or_queued = matches!(job.state, DownloadState::Running | DownloadState::Queued);
                                let is_paused = matches!(job.state, DownloadState::Paused);
                                let is_failed = matches!(job.state, DownloadState::Failed(_));
                                let pause_key = job.key.clone();
                                let resume_key = job.key.clone();
                                let retry_key = job.key.clone();
                                let cancel_key = job.key.clone();
                                rsx! {
                                    div { key: "dl-act-{job.key}", class: "addon-row glass",
                                        span { class: "addon-name", "{job.title}" }
                                        span { class: "dim tnum addon-offers", "{status}" }
                                        span { class: "topbar-spacer" }
                                        if is_running_or_queued {
                                            button { class: "pill glass small",
                                                onclick: move |_| { backend::pause_download(&pause_key); active_dl.set(backend::active_downloads()); },
                                                icons::Pause {} span { "Pause" }
                                            }
                                        } else if is_paused {
                                            button { class: "pill accent small",
                                                onclick: move |_| { let _ = backend::resume_download(&resume_key); active_dl.set(backend::active_downloads()); },
                                                icons::Play {} span { "Resume" }
                                            }
                                        } else if is_failed {
                                            button { class: "pill glass small",
                                                onclick: move |_| { let _ = backend::resume_download(&retry_key); active_dl.set(backend::active_downloads()); },
                                                icons::Refresh {} span { "Retry" }
                                            }
                                        }
                                        button { class: "pill glass small",
                                            onclick: move |_| { backend::cancel_download(&cancel_key); active_dl.set(backend::active_downloads()); },
                                            icons::Close {} span { "Cancel" }
                                        }
                                    }
                                }
                            }
                        }
                        for entry in dl_list {
                            {
                                let row_id = entry.id.clone();
                                let play_entry = entry.clone();
                                let del_id = entry.id.clone();
                                let dest_label = entry.remote.as_ref().map(|r| format!("Rclone ({r})")).unwrap_or_else(|| "Local".into());
                                rsx! {
                                    div { key: "dl-{row_id}", class: "addon-row glass",
                                        span { class: "addon-name", "{entry.title}" }
                                        span { class: "dim tnum addon-offers", "{entry.subtitle} · {backend::file_size_label(entry.bytes)} · {dest_label}" }
                                        span { class: "topbar-spacer" }
                                        button { class: "pill accent small", onclick: move |_| crate::title::play_offline(shell, play_entry.clone()), icons::Play {} span { "Play" } }
                                        button { class: "pill glass small", aria_label: "Delete download", onclick: move |_| { let _ = backend::remove_download(&del_id); dl_refresh += 1; }, icons::Trash {} }
                                    }
                                }
                            }
                        }
                        if total_dl > page_limit() {
                            button {
                                class: "pill glass small",
                                style: "align-self: center; margin-top: 8px;",
                                onclick: move |_| page_limit.set(page_limit() + 100),
                                span { "Show more ({page_limit().min(total_dl)} of {total_dl})" }
                            }
                        }
                    }
                }
            }
            if show_saved {
                if let Some(list) = saved() {
                    {
                        let list: Vec<Media> = list.into_iter().filter(|m| !gone.read().contains(&m.key()) && f.matches(m) && text_match(&m.name)).collect();
                        rsx! {
                            if !list.is_empty() {
                                section { class: "lib-section",
                                    div { class: "group-head", span { class: "group-icon", icons::Bookmark {} } span { class: "group-name", "Saved" }
                                        span { class: "tnum dim", "{list.len()}" } }
                                    div { class: "lib-grid",
                                        for m in list { div { key: "saved-{m.key()}", class: "lib-item", PosterCard { media: m.clone() } } }
                                    }
                                }
                            } else if f == LibFilter::Saved {
                                p { class: "dim", "No saved titles match." }
                            }
                        }
                    }
                }
            }
            if show_history {
                match &*history.read() {
                    None => rsx! { div { class: "week-strip", for i in 0..3 { div { key: "{i}", class: "week-card skeleton-card" } } } },
                    Some(Err(e)) => rsx! { div { class: "notice glass", span { class: "warn-icon", icons::Alert {} } span { "{e}" } } },
                    Some(Ok(list)) if list.is_empty() && saved().is_some_and(|s| !s.is_empty()) => rsx! {},
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        div { class: "center-note", div { class: "glass panel", span { class: "big-icon dim", icons::Stack {} } p { "Nothing watched yet" } } }
                    },
                    Some(Ok(list)) => {
                        let list: Vec<Seen> = list.iter()
                            .filter(|s| !gone.read().contains(&s.media.key()) && f.matches(&s.media) && text_match(&s.media.name))
                            .cloned().collect();
                        let week: Vec<Seen> = list.iter().filter(|s| now - s.at < 7 * 86_400).cloned().collect();
                        // Older viewing, grouped by calendar month, latest month first.
                        let mut months: Vec<((i64, u32), Vec<Seen>)> = Vec::new();
                        for s in list.iter().filter(|s| now - s.at >= 7 * 86_400) {
                            let (y, m, _, _) = backend::civil(s.at);
                            match months.last_mut() { Some((k, v)) if *k == (y, m) => v.push(s.clone()), _ => months.push(((y, m), vec![s.clone()])) }
                        }
                        let this_year = backend::civil(now).0;
                        rsx! {
                            if list.is_empty() {
                                p { class: "dim", "No titles match this filter." }
                            }
                            if !week.is_empty() {
                                section { class: "lib-section",
                                    div { class: "group-head", span { class: "group-icon", icons::Clock {} } span { class: "group-name", "This week" }
                                        span { class: "tnum dim", "{week.len()}" } }
                                    div { class: "week-strip",
                                        // Only the first few ask for their sources ahead: the ones Resume is likeliest on.
                                        for (i, s) in week.into_iter().enumerate() { WeekCard { key: "{s.media.key()}", seen: s.clone(), now, ahead: i < 2 } }
                                    }
                                }
                            }
                            for ((y, m), items) in months {
                                section { class: "lib-section", key: "{y}-{m}",
                                    div { class: "group-head",
                                        span { class: "group-icon", icons::Calendar {} }
                                        span { class: "group-name", if y == this_year { "{backend::month_name(m)}" } else { "{backend::month_name(m)} {y}" } }
                                        span { class: "tnum dim", "{items.len()}" }
                                    }
                                    div { class: "lib-grid",
                                        for s in items { div { key: "{s.media.key()}", class: "lib-item", PosterCard { media: s.media.clone() } Forget { media: s.media.clone() } } }
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

/// A wide card for this week: artwork, title, where you are, when you watched.
#[component]
fn WeekCard(seen: Seen, now: i64, ahead: bool) -> Element {
    let mut shell = use_context::<Shell>();
    let m = seen.media.clone();
    let art = crate::components::use_artwork(&m);
    let mut bg_sources = Vec::new();
    if let Some(b) = art().and_then(|a| a.backdrop) { if !b.is_empty() { bg_sources.push(b); } }
    let fb = m.backdrop().to_owned();
    if !fb.is_empty() && !bg_sources.contains(&fb) { bg_sources.push(fb); }
    if !m.poster.is_empty() && !bg_sources.contains(&m.poster) { bg_sources.push(m.poster.clone()); }
    let fraction = backend::progress_fraction(&m);
    // With the episode shown as a tag, the text only says what is left.
    let context = backend::resume_context(&m);
    let context = match &seen.episode { Some(ep) => context.strip_prefix(ep.as_str()).map(|r| r.trim_start_matches(", ").to_owned()).unwrap_or(context), None => context };
    let done = m.watched;
    let state = if done { "Watched".to_string() } else if backend::in_progress(&m) { context } else { "Started".into() };
    let (open, resume, pre) = (m.clone(), m.clone(), m.clone());
    // Its Resume button starts at once: the sources are asked for while the card is shown.
    let for_ahead = m.clone();
    use_hook(move || if ahead && !done { crate::title::prefetch_resume(&for_ahead) });
    rsx! {
        div { class: "week-card",
            button { class: "week-open", aria_label: "Open {m.name}",
                onpointerdown: move |_| backend::prefetch_title(&pre),
                onclick: move |_| shell.opened.set(Some(open.clone())),
                crate::components::FallbackImg { sources: bg_sources, lazy: true }
                div { class: "week-copy",
                    span { class: "week-when", "{backend::when_label(seen.at, now)}" }
                    span { class: "display week-title", "{m.name}" }
                    span { class: "week-state",
                        if let Some(ep) = &seen.episode { span { class: "tag q tnum", "{ep}" } }
                        if done { span { class: "week-check", icons::Check {} } }
                        span { "{state}" }
                    }
                }
                if fraction > 0.0 && !done { div { class: "card-progress", div { style: "width: {fraction * 100.0}%" } } }
            }
            Forget { media: m.clone() }
            if !done {
                icons::IconButton { label: "Resume {m.name}", class: "glass week-play", onclick: move |_| crate::title::resume_now(shell, resume.clone()), icons::Play {} }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Gone(Signal<std::collections::HashSet<String>>);

/// Remove from the Library: a first press asks, a second deletes the title's history.
#[component]
fn Forget(media: Media) -> Element {
    let mut shell = use_context::<Shell>();
    let Gone(mut gone) = use_context::<Gone>();
    let mut armed = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let name = media.name.clone();
    rsx! {
        div { class: if armed() { "forget armed" } else { "forget" },
            onmouseleave: move |_| armed.set(false),
            if armed() {
                button { class: "pill forget-confirm", disabled: busy(), aria_label: "Remove {name} and its history",
                    onclick: move |e| {
                        e.stop_propagation();
                        let m = media.clone();
                        busy.set(true);
                        spawn(async move {
                            if backend::forget(&m).await.is_ok() {
                                let key = m.key();
                                if let Some(lock) = LIB_SNAPSHOT.get() {
                                    if let Ok(mut snap) = lock.lock() {
                                        if let Some(h) = &mut snap.history { h.retain(|s| s.media.key() != key); }
                                        if let Some(sv) = &mut snap.saved { sv.retain(|x| x.key() != key); }
                                    }
                                }
                                gone.write().insert(key);
                                *shell.history_dirty.write() += 1;
                            }
                            busy.set(false);
                            armed.set(false);
                        });
                    },
                    icons::Trash {} span { "Remove" }
                }
                icons::IconButton { label: "Keep", class: "glass small", onclick: move |e: Event<MouseData>| { e.stop_propagation(); armed.set(false) }, icons::Close {} }
            } else {
                icons::IconButton { label: "Remove from Library", class: "glass small forget-btn", onclick: move |e: Event<MouseData>| { e.stop_propagation(); armed.set(true) }, icons::Trash {} }
            }
        }
    }
}
