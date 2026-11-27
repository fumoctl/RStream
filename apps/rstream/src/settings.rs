//! Settings. Add-ons are listed by name only; their URLs can
//! hold keys and are never shown. Keys are masked while typed and never displayed again.
use crate::{addons, backend, icons};
use dioxus::prelude::*;
use std::collections::HashMap;

async fn read_clipboard_text() -> Option<String> {
    #[cfg(target_os = "android")]
    if let Some(txt) = crate::android::read_clipboard() {
        let trimmed = txt.trim().to_owned();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    let val = document::eval("return await (navigator.clipboard ? navigator.clipboard.readText() : '')").await.ok()?;
    val.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

#[component]
pub fn SettingsPage() -> Element {
    let mut lamp = use_context::<crate::Shell>().lamp_source;
    use_hook(move || lamp.set(String::new()));
    let mut settings = use_signal(backend::load_settings);
    let mut shell = use_context::<crate::Shell>();
    let mut overview = use_resource(move || async move {
        let ov = addons::overview().await;
        shell.addon_updates.set(ov.updates);
        ov
    });
    let mut busy = use_signal(|| None::<String>);
    let mut problem = use_signal(|| None::<String>);
    let mut check_note = use_signal(|| None::<String>);
    let mut tmdb_checking = use_signal(|| false);
    let mut tmdb_problem = use_signal(|| None::<String>);
    let mut custom_url = use_signal(String::new);
    let mut import_open = use_signal(|| false);
    let mut import_text = use_signal(String::new);
    let mut data_note = use_signal(|| None::<String>);
    let mut diag_open = use_signal(|| false);
    let mut diag_text = use_signal(String::new);
    let mut diag_copied = use_signal(|| false);

    let mut run = move |label: String, job: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>>>>| {
        if busy.peek().is_some() { return; }
        busy.set(Some(label));
        problem.set(None);
        check_note.set(None);
        spawn(async move {
            if let Err(e) = job.await { problem.set(Some(format!("{e:#}"))); }
            else { crate::home::invalidate_cache(); }
            busy.set(None);
            overview.restart();
        });
    };

    let mut choose_language = move |key: String| {
        let key = if addons::language(&key).is_some() { key } else { backend::DEFAULT_LANGUAGE.to_owned() };
        settings.set(backend::update_settings(|s| s.stream_language = Some(key)));
    };

    rsx! {
        div { class: "settings",
            h1 { class: "display title", "Settings" }

            section { class: "settings-block",
                div { class: "group-head", span { class: "group-icon", icons::Grid {} } span { class: "group-name", "Charts and posters" } }
                div { class: "setting-row glass",
                    div { class: "setting-copy", span { class: "setting-name", "Source" }
                        span { class: "dim setting-about",
                            if settings().better_posters { "BetterPosters: Trakt and TMDB charts on Home, posters with rating and genre tags, and fuller details. Cinemeta fills in when it can't answer." }
                            else { "Your add-ons' own catalogs, posters and details" } } }
                    div { class: "segmented glass", role: "radiogroup", aria_label: "Charts and posters source",
                        i { class: "slider", aria_hidden: "true" }
                        for (on, name) in [(true, "BetterPosters"), (false, "Add-on")] {
                            button { class: if settings().better_posters == on { "seg wide on" } else { "seg wide" }, role: "radio", aria_checked: "{settings().better_posters == on}",
                                onclick: move |_| {
                                    settings.set(backend::update_settings(|s| s.better_posters = on));
                                    backend::clear_artwork_caches();
                                    crate::home::invalidate_cache();
                                }, "{name}" }
                        }
                    }
                }
            }

            ScreenBlock { settings }

            section { class: "settings-block",
                div { class: "group-head", span { class: "group-icon", icons::Globe {} } span { class: "group-name", "Artwork" } }
                div { class: "setting-row glass",
                    div { class: "setting-copy", span { class: "setting-name", "TMDB" }
                        span { class: "dim setting-about",
                            if tmdb_checking() { "Checking your key…" }
                            else if settings().tmdb_key.is_some() { "Connected: free sources run first; TMDB adds textless backdrops, actor/producer photos, videos and gap-fill stills" }
                            else { "Optional. Free sources (BetterPosters, TVmaze, AniList, Wikidata) always run first; TMDB adds textless backdrops, actor/producer photos, taglines and videos" } } }
                    if settings().tmdb_key.is_some() {
                        button { class: "pill glass", onclick: move |_| {
                            settings.set(backend::update_settings(|s| s.tmdb_key = None));
                            backend::clear_artwork_caches();
                        }, icons::Trash {} span { "Remove key" } }
                    } else {
                        LinkButton { url: addons::TMDB_KEY_PAGE, label: "Get a TMDB key (API Key, not the Read Access Token)" }
                        input { class: "key-field", r#type: "password", placeholder: "TMDB API key", aria_label: "TMDB API key", autocomplete: "off",
                            disabled: tmdb_checking(),
                            onkeydown: move |e| { e.stop_propagation(); },
                            onchange: move |e| {
                                let key = e.value().trim().to_owned();
                                if key.is_empty() || tmdb_checking() { return; }
                                tmdb_checking.set(true);
                                tmdb_problem.set(None);
                                spawn(async move {
                                    match backend::check_tmdb_key(key.clone()).await {
                                        Ok(()) => {
                                            settings.set(backend::update_settings(|s| s.tmdb_key = Some(key)));
                                            backend::clear_artwork_caches();
                                        }
                                        Err(e) => tmdb_problem.set(Some(format!("{e:#}"))),
                                    }
                                    tmdb_checking.set(false);
                                });
                            } }
                    }
                }
                if let Some(p) = tmdb_problem() { p { class: "addon-problem", role: "alert", icons::Alert {} span { "{p}" } } }
            }

            section { class: "settings-block",
                div { class: "group-head", span { class: "group-icon", icons::Magnet {} } span { class: "group-name", "Streaming" } }
                div { class: "setting-row glass",
                    div { class: "setting-copy", span { class: "setting-name", "Audio language" }
                        span { class: "dim setting-about", "Play picks sources in this language first" } }
                    select { class: "select-field", aria_label: "Audio language", disabled: busy().is_some(),
                        value: settings().stream_language.clone().unwrap_or_else(|| backend::DEFAULT_LANGUAGE.into()),
                        onkeydown: move |e| { e.stop_propagation(); },
                        onchange: move |e| choose_language(e.value()),
                        for l in addons::LANGUAGES {
                            option { value: "{l.key}", selected: settings().stream_language.as_deref() == Some(l.key), "{l.label}" }
                        }
                    }
                }
                div { class: "setting-row glass",
                    div { class: "setting-copy", span { class: "setting-name", "Up Next countdown" }
                        span { class: "dim setting-about", "Show a short countdown to the next episode when credits begin or an episode ends" } }
                    div { class: "segmented glass", role: "radiogroup", aria_label: "Up Next countdown",
                        i { class: "slider", aria_hidden: "true" }
                        for (on, name) in [(true, "On"), (false, "Off")] {
                            button { class: if settings().auto_next == on { "seg wide on" } else { "seg wide" }, role: "radio", aria_checked: "{settings().auto_next == on}",
                                onclick: move |_| settings.set(backend::update_settings(|s| s.auto_next = on)), "{name}" }
                        }
                    }
                }
                div { class: "setting-row glass",
                    div { class: "setting-copy",
                        span { class: "setting-name", "Parallel downloads" }
                        span { class: "dim setting-about", "How many episodes or films download concurrently (cloud rclone uploads run on a separate queue so uploads never block the next download)" }
                    }
                    select {
                        class: "select-field",
                        aria_label: "Parallel downloads",
                        value: "{settings().max_parallel_downloads}",
                        onkeydown: move |e| e.stop_propagation(),
                        onchange: move |e| {
                            if let Ok(n) = e.value().parse::<usize>() {
                                settings.set(backend::update_settings(|s| s.max_parallel_downloads = n.clamp(1, 6)));
                            }
                        },
                        for n in 1usize..=6 {
                            option {
                                key: "pdl-{n}",
                                value: "{n}",
                                selected: settings().max_parallel_downloads == n,
                                {if n == 1 { "1 at a time".to_owned() } else { format!("{n} in parallel") }}
                            }
                        }
                    }
                }
                if !cfg!(target_os = "android") {
                    div { class: "setting-row glass",
                        div { class: "setting-copy",
                            span { class: "setting-name", "Minimize to system tray on close" }
                            span { class: "dim setting-about", "Closing the window stops video playback and hides RStream in the system tray so downloads and cloud sync finish in the background" }
                        }
                        div { class: "segmented glass", role: "radiogroup", aria_label: "Minimize to system tray on close",
                            i { class: "slider", aria_hidden: "true" }
                            for (on, name) in [(true, "On"), (false, "Off")] {
                                button {
                                    class: if settings().close_to_tray == on { "seg wide on" } else { "seg wide" },
                                    role: "radio",
                                    aria_checked: "{settings().close_to_tray == on}",
                                    onclick: move |_| {
                                        settings.set(backend::update_settings(|s| s.close_to_tray = on));
                                        crate::set_desktop_close_to_tray(on);
                                    },
                                    "{name}"
                                }
                            }
                        }
                    }
                }
            }

            section { class: "settings-block",
                div { class: "group-head", span { class: "group-icon", icons::Stack {} } span { class: "group-name", "Add-ons" }
                    span { class: "topbar-spacer" }
                    button { class: "pill glass small", disabled: busy().is_some(),
                        onclick: move |_| run("check".into(), Box::pin(async move { check_note.set(Some(addons::check().await.1)); Ok(()) })),
                        icons::Refresh {} span { if busy().as_deref() == Some("check") { "Checking…" } else { "Check for updates" } } }
                }
                if let Some(note) = check_note() { p { class: "check-note", role: "status", icons::Check {} span { "{note}" } } }
                if let Some(p) = problem() { p { class: "addon-problem", role: "alert", icons::Alert {} span { "{p}" } } }
                match overview() {
                    None => rsx! { div { class: "addon-list", div { class: "row skeleton-row" } } },
                    Some(ov) => rsx! {
                        div { class: "market",
                            for (c, row) in ov.curated.clone() {
                                div { class: if row.is_some() { "market-card glass on" } else { "market-card glass" },
                                    div { class: "market-head",
                                        span { class: "market-name display", "{c.name()}" }
                                        if let Some(r) = &row { span { class: "dim tnum market-version", "v{r.version}" } }
                                    }
                                    p { class: "market-about", "{c.about()}" }
                                    p { class: "dim market-about", "{c.detail()}" }
                                    div { class: "market-actions",
                                        if busy().as_deref() == Some(c.name()) {
                                            span { class: "pill static glass", "Setting up…" }
                                        } else if let Some(r) = row.clone() {
                                            if let Some(v) = r.update.clone() {
                                                { let r = r.clone(); rsx! { button { class: "pill accent", disabled: busy().is_some(), onclick: move |_| { let r = r.clone(); run(c.name().into(), Box::pin(async move { addons::update(&r).await })) }, icons::Refresh {} span { "Update to {v}" } } } }
                                            } else { span { class: "market-status", icons::Check {} span { "Installed" } } }
                                            button { class: "pill glass small", disabled: busy().is_some(), onclick: move |_| { let r = r.clone(); run(c.name().into(), Box::pin(async move { addons::remove(&r).await })) }, icons::Trash {} span { "Remove" } }
                                        } else {
                                            button { class: "pill accent", disabled: busy().is_some(),
                                                onclick: move |_| run(c.name().into(), Box::pin(async move { addons::install(c).await })),
                                                icons::Plus {} span { "Install" } }
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "addon-list",
                            for r in ov.others.clone() {
                                div { class: "addon-row glass",
                                    span { class: "addon-name", "{r.name}" }
                                    if !r.offers.is_empty() { span { class: "dim addon-offers", "{r.offers}" } }
                                    span { class: "topbar-spacer" }
                                    if let Some(v) = r.update.clone() {
                                        { let r = r.clone(); let label = r.name.clone(); rsx! { button { class: "pill accent small", disabled: busy().is_some(), onclick: move |_| { let r = r.clone(); run(label.clone(), Box::pin(async move { addons::update(&r).await })) }, icons::Refresh {} span { "Update to {v}" } } } }
                                    } else if !r.version.is_empty() { span { class: "dim tnum addon-offers", "v{r.version}" } }
                                    if r.removable() {
                                        { let r = r.clone(); let label = r.name.clone(); rsx! { button { class: "pill glass small", disabled: busy().is_some(), onclick: move |_| { let r = r.clone(); run(label.clone(), Box::pin(async move { addons::remove(&r).await })) }, icons::Trash {} span { "Remove" } } } }
                                    }
                                }
                            }
                        }
                        div { class: "setting-row glass", style: "margin-top: 12px",
                            div { class: "setting-copy", span { class: "setting-name", "Custom Stremio add-on" }
                                span { class: "dim setting-about", "Install any Stremio-compatible manifest URL (community catalogs, subtitle providers, or self-hosted add-ons). Links stay masked and are never logged." } }
                            input { class: "key-field", r#type: "password", placeholder: "https://…/manifest.json", aria_label: "Custom Stremio add-on URL", autocomplete: "off",
                                value: "{custom_url}", disabled: busy().is_some(),
                                oninput: move |e| custom_url.set(e.value()),
                                onkeydown: move |e| { e.stop_propagation(); },
                            }
                            button { class: "pill accent small", disabled: busy().is_some() || custom_url().trim().is_empty(),
                                onclick: move |_| {
                                    let url = custom_url.peek().trim().to_owned();
                                    custom_url.set(String::new());
                                    run("custom".into(), Box::pin(async move {
                                        let name = addons::install_custom(url).await?;
                                        check_note.set(Some(format!("Installed {name}.")));
                                        Ok(())
                                    }));
                                },
                                icons::Plus {} span { "Install" }
                            }
                        }
                    },
                }
            }

            TorrentVpnBlock { settings }

            RcloneBlock { settings }

            PlayerConfigBlock {}

            section { class: "settings-block",
                div { class: "group-head", span { class: "group-icon", icons::History {} } span { class: "group-name", "Library data & diagnostics" } }
                div { class: "setting-row glass",
                    div { class: "setting-copy", span { class: "setting-name", "Import or export watch history" }
                        span { class: "dim setting-about", "Bring in a Trakt watched JSON, IMDb CSV, or RStream backup. Exports include no keys or add-on links." } }
                    div { class: "account-actions",
                        button { class: "pill glass small", onclick: move |_| import_open.set(!import_open()), icons::Upload {} span { if import_open() { "Hide import" } else { "Import" } } }
                        button { class: "pill glass small", onclick: move |_| {
                            data_note.set(None);
                            spawn(async move {
                                match backend::export_watch_history().await {
                                    Ok(json_text) => {
                                        let js = format!("navigator.clipboard?.writeText({}).catch(()=>{{}})", serde_json::to_string(&json_text).unwrap_or_default());
                                        let _ = document::eval(&js);
                                        data_note.set(Some("Watch history exported to rstream-history-export.json and copied to clipboard.".into()));
                                    }
                                    Err(e) => data_note.set(Some(format!("Export failed: {e}"))),
                                }
                            });
                        }, icons::Download {} span { "Export JSON" } }
                    }
                }
                if import_open() {
                    div { class: "account-form glass",
                        textarea { class: "import-area", rows: "5", placeholder: "Paste Trakt watched JSON, RStream export JSON, or IMDb CSV…",
                            value: "{import_text}",
                            oninput: move |e| import_text.set(e.value()),
                            onkeydown: move |e| e.stop_propagation(),
                        }
                        div { class: "account-actions",
                            button { class: "pill accent small", disabled: import_text().trim().is_empty(),
                                onclick: move |_| {
                                    let raw = import_text.peek().clone();
                                    data_note.set(None);
                                    spawn(async move {
                                        match backend::import_watch_history(&raw).await {
                                            Ok(n) => {
                                                import_text.set(String::new());
                                                import_open.set(false);
                                                data_note.set(Some(format!("Imported {n} title(s) into your Library.")));
                                            }
                                            Err(e) => data_note.set(Some(format!("{e}"))),
                                        }
                                    });
                                },
                                icons::Check {} span { "Import titles" }
                            }
                        }
                    }
                }
                if let Some(note) = data_note() { p { class: "check-note", role: "status", icons::Check {} span { "{note}" } } }
                div { class: "setting-row glass",
                    div { class: "setting-copy", span { class: "setting-name", "Diagnostic log" }
                        span { class: "dim setting-about", "Recent playback and system events with URLs, API keys, and tokens automatically redacted." } }
                    div { class: "account-actions",
                        button { class: "pill glass small", onclick: move |_| {
                            let report = backend::diagnostic_report();
                            diag_text.set(report.clone());
                            let js = format!("navigator.clipboard?.writeText({}).catch(()=>{{}})", serde_json::to_string(&report).unwrap_or_default());
                            let _ = document::eval(&js);
                            diag_copied.set(true);
                        }, icons::Copy {} span { if diag_copied() { "Copied" } else { "Copy log" } } }
                        button { class: "pill glass small", onclick: move |_| {
                            if !diag_open() { diag_text.set(backend::diagnostic_report()); }
                            diag_open.set(!diag_open());
                        }, span { if diag_open() { "Hide" } else { "View" } } }
                    }
                }
                if diag_open() {
                    pre { class: "diag-log glass", "{diag_text}" }
                }
            }
        }
    }
}

#[component]
fn LinkButton(url: &'static str, label: &'static str) -> Element {
    rsx! {
        button { class: "link-btn", title: "{url}", onclick: move |_| crate::open_external(url),
            icons::Link {} span { "{label}" } }
    }
}

#[component]
fn ScreenBlock(settings: Signal<backend::Settings>) -> Element {
    let mut screen = use_signal(crate::display::screen);
    let (title, about) = crate::display::summary(screen(), settings().hdr_output);
    rsx! {
        section { class: "settings-block",
            div { class: "group-head", span { class: "group-icon", icons::Tv {} } span { class: "group-name", "Screen" } }
            div { class: "setting-row glass",
                div { class: "setting-copy", span { class: "setting-name", "This screen: {title}" }
                    span { class: "dim setting-about", "{about}" } }
                button { class: "pill glass", onclick: move |_| screen.set(crate::display::screen()), icons::Refresh {} span { "Check again" } }
            }
            if cfg!(target_os = "windows") {
                div { class: "setting-row glass",
                    div { class: "setting-copy", span { class: "setting-name", "HDR output" }
                        span { class: "dim setting-about", "Automatic sends HDR videos to the screen in HDR when Windows HDR is on. Always SDR tone-maps them, for screens whose HDR looks washed out. Applies from the next video." } }
                    div { class: "segmented glass", role: "radiogroup", aria_label: "HDR output",
                        i { class: "slider", aria_hidden: "true" }
                        for (on, name) in [(true, "Automatic"), (false, "Always SDR")] {
                            button { class: if settings().hdr_output == on { "seg wide on" } else { "seg wide" }, role: "radio", aria_checked: "{settings().hdr_output == on}",
                                onclick: move |_| settings.set(backend::update_settings(|s| s.hdr_output = on)), "{name}" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn TorrentVpnBlock(mut settings: Signal<backend::Settings>) -> Element {
    let mut interfaces = use_signal(backend::torrent_interfaces);
    let mut vpn_state = use_signal(backend::android_vpn_state);
    // Ephemeral RAM-only leak probe state — never persisted to disk or settings!
    let mut leak_magnet = use_signal(String::new);
    let mut probe_status = use_signal(backend::leak_probe_status);
    let mut probe_err = use_signal(|| None::<String>);

    use_drop(move || {
        backend::stop_leak_probe();
    });

    use_future(move || async move {
        loop {
            backend::sleep(500).await;
            let now = backend::leak_probe_status();
            if *probe_status.peek() != now {
                probe_status.set(now);
            }
        }
    });

    let vpn_summary = if cfg!(target_os = "android") {
        let (active, iface) = vpn_state();
        if active {
            format!("Android VPN active ({})", iface.as_deref().unwrap_or("tunnel"))
        } else {
            "No active Android VPN detected".into()
        }
    } else {
        let up_vpns: Vec<String> = interfaces.read().iter().filter(|i| i.is_up && i.is_vpn_like).map(|i| i.name.clone()).collect();
        if up_vpns.is_empty() {
            "No active VPN tunnel interface detected (tun/wg/tap/ppp)".into()
        } else {
            format!("Active VPN interface(s): {}", up_vpns.join(", "))
        }
    };

    let saved_iface = settings().torrent_bind_interface.clone();
    let offline_saved = saved_iface
        .as_ref()
        .filter(|s| !interfaces.read().iter().any(|i| &i.name == *s))
        .cloned();
    let p_status = probe_status();

    rsx! {
        section { class: "settings-block",
            div { class: "group-head",
                span { class: "group-icon", icons::Lock {} }
                span { class: "group-name", "Torrent & VPN killswitch" }
                span { class: "topbar-spacer" }
                button { class: "pill glass small",
                    onclick: move |_| {
                        interfaces.set(backend::torrent_interfaces());
                        vpn_state.set(backend::android_vpn_state());
                    },
                    icons::Refresh {} span { "Refresh interfaces" }
                }
            }
            div { class: "setting-row glass",
                div { class: "setting-copy",
                    span { class: "setting-name", "VPN killswitch" }
                    span { class: "dim setting-about", "Block all BitTorrent P2P streaming and downloads unless a VPN tunnel (or selected interface) is up. {vpn_summary}." }
                }
                div { class: "segmented glass", role: "radiogroup", aria_label: "Torrent VPN killswitch",
                    i { class: "slider", aria_hidden: "true" }
                    for (on, name) in [(true, "On"), (false, "Off")] {
                        button {
                            class: if settings().torrent_killswitch == on { "seg wide on" } else { "seg wide" },
                            role: "radio", aria_checked: "{settings().torrent_killswitch == on}",
                            onclick: move |_| settings.set(backend::update_settings(|s| s.torrent_killswitch = on)),
                            "{name}"
                        }
                    }
                }
            }
            div { class: "setting-row glass",
                div { class: "setting-copy",
                    span { class: "setting-name", "Bind network interface" }
                    span { class: "dim setting-about", "Pin BitTorrent sockets to a specific interface (or leave automatic to pick any active VPN tunnel)." }
                }
                select { class: "select-field", aria_label: "Bind torrent interface",
                    value: saved_iface.clone().unwrap_or_default(),
                    onkeydown: move |e| e.stop_propagation(),
                    onchange: move |e| {
                        let v = e.value().trim().to_owned();
                        settings.set(backend::update_settings(|s| s.torrent_bind_interface = (!v.is_empty()).then_some(v)));
                    },
                    option { value: "", selected: saved_iface.is_none(), "Automatic" }
                    if let Some(saved) = offline_saved {
                        option { key: "{saved}", value: "{saved}", selected: true, "{saved} (offline)" }
                    }
                    for iface in interfaces.read().iter() {
                        option {
                            key: "{iface.name}",
                            value: "{iface.name}",
                            selected: saved_iface.as_deref() == Some(iface.name.as_str()),
                            {
                                let tags = [
                                    iface.is_vpn_like.then_some("VPN"),
                                    Some(if iface.is_up { "up" } else { "down" }),
                                ].into_iter().flatten().collect::<Vec<_>>().join(", ");
                                format!("{} ({tags})", iface.name)
                            }
                        }
                    }
                }
            }

            div { class: "leak-card glass",
                div { class: "leak-head",
                    span { class: "leak-title", "Torrent Leak Test (ipleak.net) — Ephemeral RAM-Only" }
                    span { class: "leak-badge", "RAM-Only · Zero Disk Writes" }
                    span { class: "topbar-spacer" }
                    button { class: "pill glass small",
                        onclick: move |_| crate::open_external("https://ipleak.net"),
                        icons::External {} span { "Open ipleak.net" }
                    }
                }
                p { class: "dim leak-desc",
                    "Paste the temporary magnet URI from ipleak.net Torrent Address detection to verify tracker announces leave strictly through your bound VPN tunnel. Never persisted to disk or settings; auto-expires after 60s."
                }
                div { class: "leak-input-wrap",
                    input {
                        class: "leak-input",
                        r#type: "text",
                        placeholder: "ipleak.net Magnet URI",
                        aria_label: "ipleak.net Magnet URI",
                        autocomplete: "off",
                        spellcheck: "false",
                        value: "{leak_magnet}",
                        oninput: move |e| leak_magnet.set(e.value()),
                        onkeydown: move |e| e.stop_propagation(),
                    }
                    button {
                        class: "bare leak-paste",
                        title: "Paste from clipboard",
                        aria_label: "Paste magnet URI from clipboard",
                        onclick: move |_| {
                            spawn(async move {
                                if let Some(txt) = read_clipboard_text().await {
                                    leak_magnet.set(txt);
                                }
                            });
                        },
                        icons::Clipboard {}
                    }
                }
                div { class: "leak-actions",
                    button {
                        class: "pill accent small leak-start",
                        disabled: leak_magnet().trim().is_empty(),
                        onclick: move |_| {
                            probe_err.set(None);
                            let uri = leak_magnet.peek().trim().to_owned();
                            match backend::start_leak_probe(&uri) {
                                Ok(()) => probe_status.set(backend::leak_probe_status()),
                                Err(e) => probe_err.set(Some(format!("{e:#}"))),
                            }
                        },
                        icons::Radar {} span { "Start Leak Probe" }
                    }
                    button {
                        class: "pill glass small",
                        onclick: move |_| {
                            backend::stop_leak_probe();
                            leak_magnet.set(String::new());
                            probe_err.set(None);
                            probe_status.set(backend::leak_probe_status());
                        },
                        icons::StopCircle {} span { "Stop & Clear" }
                    }
                }
                if let Some(e) = probe_err() {
                    p { class: "addon-problem", role: "alert", icons::Alert {} span { "{e}" } }
                } else if p_status.active || !p_status.summary.is_empty() || p_status.error.is_some() {
                    div { class: if p_status.error.is_some() { "addon-problem" } else { "check-note tnum" }, role: "status",
                        if p_status.error.is_some() { icons::Alert {} } else { icons::Check {} }
                        span {
                            {
                                let iface = p_status.bound_interface.as_deref().unwrap_or("default route");
                                let base = if p_status.active {
                                    format!(
                                        "{} · Bound: {iface} · Trackers OK: {}/{} · {}s remaining",
                                        p_status.summary, p_status.trackers_ok, p_status.trackers_total, p_status.remaining_secs
                                    )
                                } else {
                                    format!(
                                        "{} · Bound: {iface} · Trackers OK: {}/{}",
                                        p_status.summary, p_status.trackers_ok, p_status.trackers_total
                                    )
                                };
                                match &p_status.error {
                                    Some(err) => format!("{base} — {err}"),
                                    None => base,
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RcloneTab {
    Guided,
    AllProviders,
    ImportConf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct GuidedProvider {
    prefix: &'static str,
    title: &'static str,
    badge: &'static str,
    default_name: &'static str,
    about: &'static str,
    is_oauth: bool,
}

const GUIDED_PROVIDERS: [GuidedProvider; 8] = [
    GuidedProvider {
        prefix: "drive",
        title: "Google Drive",
        badge: "OAuth 2.0",
        default_name: "gdrive",
        about: "Browser OAuth connect with optional custom Client ID & Secret for dedicated API quota.",
        is_oauth: true,
    },
    GuidedProvider {
        prefix: "onedrive",
        title: "Microsoft OneDrive",
        badge: "OAuth 2.0",
        default_name: "onedrive",
        about: "Personal or SharePoint OneDrive storage connected via browser sign-in.",
        is_oauth: true,
    },
    GuidedProvider {
        prefix: "dropbox",
        title: "Dropbox",
        badge: "OAuth 2.0",
        default_name: "dropbox",
        about: "Fast cloud streaming and snapshot sync with official Dropbox OAuth.",
        is_oauth: true,
    },
    GuidedProvider {
        prefix: "protondrive",
        title: "Proton Drive",
        badge: "Zero-Knowledge",
        default_name: "proton",
        about: "End-to-end encrypted Proton storage using account credentials & 2FA.",
        is_oauth: false,
    },
    GuidedProvider {
        prefix: "s3",
        title: "S3 / Cloudflare R2",
        badge: "S3 Object Store",
        default_name: "r2",
        about: "Zero-egress Cloudflare R2, AWS S3, Wasabi, Backblaze S3, or MinIO buckets.",
        is_oauth: false,
    },
    GuidedProvider {
        prefix: "webdav",
        title: "WebDAV",
        badge: "HTTPS WebDAV",
        default_name: "webdav",
        about: "Nextcloud, ownCloud, Koofr, Hetzner Storage Box, or any HTTPS WebDAV server.",
        is_oauth: false,
    },
    GuidedProvider {
        prefix: "sftp",
        title: "SFTP / SSH",
        badge: "SSH",
        default_name: "nas",
        about: "Connect to any Linux or NAS server over SSH with password or key file.",
        is_oauth: false,
    },
    GuidedProvider {
        prefix: "local",
        title: "Local Folder",
        badge: "Local",
        default_name: "vault",
        about: "Mounted external drive, NAS share, or local folder on this device.",
        is_oauth: false,
    },
];

#[component]
fn SecretInput(
    placeholder: String,
    value: String,
    on_change: EventHandler<String>,
    #[props(default)] allow_paste: bool,
) -> Element {
    let mut revealed = use_signal(|| false);
    rsx! {
        div { class: "rclone-secret-wrap",
            input {
                class: "rclone-input",
                r#type: if revealed() { "text" } else { "password" },
                placeholder: "{placeholder}",
                aria_label: "{placeholder}",
                autocomplete: "off",
                spellcheck: "false",
                value: "{value}",
                oninput: move |e| on_change.call(e.value()),
                onkeydown: move |e| e.stop_propagation(),
            }
            if allow_paste {
                button {
                    class: "bare rclone-icon-btn",
                    r#type: "button",
                    title: "Paste from clipboard",
                    aria_label: "Paste {placeholder} from clipboard",
                    onclick: move |_| {
                        let cb = on_change;
                        spawn(async move {
                            if let Some(txt) = read_clipboard_text().await {
                                cb.call(txt);
                            }
                        });
                    },
                    icons::Clipboard {}
                }
            }
            button {
                class: "bare rclone-icon-btn",
                r#type: "button",
                title: if revealed() { "Hide secret" } else { "Show secret" },
                aria_label: if revealed() { "Hide {placeholder}" } else { "Show {placeholder}" },
                onclick: move |_| revealed.set(!revealed()),
                if revealed() { icons::EyeOff {} } else { icons::Eye {} }
            }
        }
    }
}

#[component]
fn RcloneBlock(mut settings: Signal<backend::Settings>) -> Element {
    let mut remotes = use_resource(move || async move { backend::list_rclone_remotes().await });
    let providers = use_resource(move || async move { backend::list_rclone_providers().await });
    let mut tab = use_signal(|| RcloneTab::Guided);
    let mut setup_mode = use_signal(|| rstream_rclone::RemoteSetupMode::CreateNew);
    let mut guided_idx = use_signal(|| 0usize);
    let mut remote_name = use_signal(|| GUIDED_PROVIDERS[0].default_name.to_owned());
    let mut name_touched = use_signal(|| false);
    let mut params = use_signal(HashMap::<String, String>::new);
    let mut headless_open = use_signal(|| false);

    // Tier 2: All Rclone Providers state
    let mut provider_search = use_signal(String::new);
    let mut selected_prefix = use_signal(|| "b2".to_owned());
    let mut show_advanced = use_signal(|| false);

    // Crypt wrapper state
    let crypt_enabled = use_signal(|| false);
    let mut crypt_pw = use_signal(String::new);
    let mut crypt_salt = use_signal(String::new);
    let crypt_folder = use_signal(|| "RStream".to_owned());

    // Tier 3: Import rclone.conf state
    let mut import_raw = use_signal(String::new);

    let mut saving = use_signal(|| false);
    let mut syncing_profile = use_signal(|| false);
    let mut profile_draft = use_signal(|| settings.peek().sync_profile.clone());
    let mut oauth_waiting = use_signal(|| false);
    let mut note = use_signal(|| None::<String>);
    let mut err = use_signal(|| None::<String>);

    let list = remotes.read().as_ref().and_then(|r| r.as_ref().ok()).cloned().unwrap_or_default();
    let load_err = remotes.read().as_ref().and_then(|r| r.as_ref().err()).map(|e| format!("{e:#}"));
    let all_providers = providers.read().as_ref().and_then(|r| r.as_ref().ok()).cloned().unwrap_or_default();

    let saved_target = settings().default_save_target.clone();
    let missing_target = saved_target
        .as_ref()
        .filter(|t| !list.iter().any(|r| &r.name == *t))
        .cloned();
    let saved_sync_remote = settings().sync_remote.clone();
    let missing_sync_remote = saved_sync_remote
        .as_ref()
        .filter(|t| !list.iter().any(|r| &r.name == *t))
        .cloned();

    let guided = GUIDED_PROVIDERS[guided_idx().min(GUIDED_PROVIDERS.len() - 1)];
    let is_connect = setup_mode() == rstream_rclone::RemoteSetupMode::ConnectExisting;

    let set_param = move |key: &str, val: String| {
        let mut p = params;
        p.with_mut(|m| {
            if val.is_empty() { m.remove(key); } else { m.insert(key.to_owned(), val); }
        });
    };
    let get_param = |key: &str| -> String {
        params.read().get(key).cloned().unwrap_or_default()
    };

    let active_provider_prefix = match tab() {
        RcloneTab::Guided => guided.prefix.to_owned(),
        RcloneTab::AllProviders => selected_prefix(),
        RcloneTab::ImportConf => String::new(),
    };
    let active_is_oauth = match tab() {
        RcloneTab::Guided => guided.is_oauth,
        RcloneTab::AllProviders => all_providers.iter().find(|p| p.prefix == selected_prefix()).is_some_and(|p| p.is_oauth),
        RcloneTab::ImportConf => false,
    };
    let can_wrap_crypt = active_provider_prefix != "crypt";

    let submit_disabled = saving()
        || remote_name().trim().is_empty()
        || active_provider_prefix.trim().is_empty()
        || (can_wrap_crypt && crypt_enabled() && crypt_pw().is_empty());

    let mut submit_wizard = move || {
        if submit_disabled { return; }
        let name = remote_name.peek().trim().to_owned();
        let provider = active_provider_prefix.clone();
        let mode = *setup_mode.peek();
        let raw_map = params.peek().clone();
        let mut json_params = serde_json::Map::new();
        for (k, v) in &raw_map {
            if !v.trim().is_empty() {
                json_params.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
        }
        let is_interactive_oauth = active_is_oauth && !json_params.contains_key("token");
        if is_interactive_oauth {
            json_params.insert("authorize_oauth".into(), serde_json::Value::Bool(true));
        }
        let crypt = if can_wrap_crypt && *crypt_enabled.peek() {
            let salt = crypt_salt.peek().trim().to_owned();
            Some(rstream_rclone::CryptOptions {
                password: crypt_pw.peek().clone(),
                salt: (!salt.is_empty()).then_some(salt),
                folder: crypt_folder.peek().trim().to_owned(),
            })
        } else {
            None
        };
        let req = rstream_rclone::RemoteSetupRequest {
            name: name.clone(),
            provider,
            parameters: json_params,
            mode,
            crypt,
        };
        saving.set(true);
        oauth_waiting.set(is_interactive_oauth);
        note.set(None);
        err.set(None);
        spawn(async move {
            match backend::setup_rclone_remote(req).await {
                Ok(created_name) => {
                    crypt_pw.set(String::new());
                    crypt_salt.set(String::new());
                    params.with_mut(|m| {
                        for secret_k in ["password", "pass", "secret_access_key", "client_secret", "token", "2fa"] {
                            m.remove(secret_k);
                        }
                    });
                    remotes.restart();
                    note.set(Some(format!(
                        "{} remote `{created_name}` ready.",
                        if mode == rstream_rclone::RemoteSetupMode::ConnectExisting { "Connected" } else { "Created" }
                    )));
                }
                Err(e) => err.set(Some(format!("{e:#}"))),
            }
            oauth_waiting.set(false);
            saving.set(false);
        });
    };

    rsx! {
        section { class: "settings-block",
            div { class: "group-head",
                span { class: "group-icon", icons::Upload {} }
                span { class: "group-name", "Rclone & encrypted remotes" }
            }
            div { class: "setting-row glass",
                div { class: "setting-copy",
                    span { class: "setting-name", "Default save destination" }
                    span { class: "dim setting-about", "Initial destination pre-selected in the Download dialog (local device storage or a configured rclone remote)." }
                }
                select { class: "select-field", aria_label: "Default save destination",
                    value: saved_target.clone().unwrap_or_default(),
                    onkeydown: move |e| e.stop_propagation(),
                    onchange: move |e| {
                        let v = e.value().trim().to_owned();
                        settings.set(backend::update_settings(|s| s.default_save_target = (!v.is_empty()).then_some(v)));
                    },
                    option { value: "", selected: saved_target.is_none(), "Local device" }
                    if let Some(saved) = missing_target {
                        option { key: "def-{saved}", value: "{saved}", selected: true, "{saved}" }
                    }
                    for r in &list {
                        option {
                            key: "def-{r.name}",
                            value: "{r.name}",
                            selected: saved_target.as_deref() == Some(r.name.as_str()),
                            "{r.name} ({r.kind})"
                        }
                    }
                }
            }
            div { class: "setting-row glass",
                div { class: "setting-copy",
                    span { class: "setting-name", "Library, watch history & progress sync" }
                    span { class: "dim setting-about", "Sync your Library, Continue Watching timestamps, watched episodes, bingeGroup locks, and track preferences across devices via RStream/.profiles/ (end-to-end encrypted on crypt remotes)." }
                }
                select {
                    class: "select-field",
                    aria_label: "Library and progress sync remote",
                    value: saved_sync_remote.clone().unwrap_or_default(),
                    onkeydown: move |e| e.stop_propagation(),
                    onchange: move |e| {
                        let v = e.value().trim().to_owned();
                        settings.set(backend::update_settings(|s| s.sync_remote = (!v.is_empty()).then_some(v)));
                    },
                    option { value: "", selected: saved_sync_remote.is_none(), "Off (this device only)" }
                    if let Some(saved) = missing_sync_remote {
                        option { key: "sync-{saved}", value: "{saved}", selected: true, "{saved}" }
                    }
                    for r in &list {
                        option {
                            key: "sync-{r.name}",
                            value: "{r.name}",
                            selected: saved_sync_remote.as_deref() == Some(r.name.as_str()),
                            "{r.name} ({r.kind})"
                        }
                    }
                }
            }
            if saved_sync_remote.is_some() {
                div { class: "setting-row glass",
                    div { class: "setting-copy",
                        span { class: "setting-name", "Sync profile" }
                        span { class: "dim setting-about", "Devices using the same profile name share watch history and progress; different profile names share cloud downloads without mixing watch history." }
                    }
                    div { class: "account-actions",
                        input {
                            class: "key-field",
                            style: "max-width: 160px;",
                            r#type: "text",
                            placeholder: "default",
                            aria_label: "Sync profile name",
                            value: "{profile_draft}",
                            oninput: move |e| profile_draft.set(e.value()),
                            onkeydown: move |e| e.stop_propagation(),
                            onchange: move |e| {
                                let clean = backend::sanitize_profile_name(&e.value());
                                profile_draft.set(clean.clone());
                                settings.set(backend::update_settings(|s| s.sync_profile = clean));
                            },
                        }
                        button {
                            class: "pill accent small",
                            disabled: syncing_profile(),
                            onclick: move |_| {
                                let clean = backend::sanitize_profile_name(&profile_draft.peek());
                                profile_draft.set(clean.clone());
                                if settings.peek().sync_profile != clean {
                                    settings.set(backend::update_settings(|s| s.sync_profile = clean));
                                }
                                syncing_profile.set(true);
                                note.set(None);
                                err.set(None);
                                spawn(async move {
                                    match backend::sync_profile_now().await {
                                        Ok(changed) => {
                                            crate::home::invalidate_cache();
                                            note.set(Some(if changed > 0 {
                                                format!("Profile synced ({changed} remote update(s) applied).")
                                            } else {
                                                "Profile synced (up to date).".into()
                                            }));
                                        }
                                        Err(e) => err.set(Some(format!("Sync failed: {e:#}"))),
                                    }
                                    syncing_profile.set(false);
                                });
                            },
                            icons::Refresh {}
                            span { if syncing_profile() { "Syncing…" } else { "Sync now" } }
                        }
                    }
                }
            }
            if !list.is_empty() {
                div { class: "addon-list",
                    for r in &list {
                        {
                            let rname = r.name.clone();
                            rsx! {
                                div { key: "rc-{r.name}", class: "addon-row glass",
                                    span { class: "addon-name", "{r.name}" }
                                    span { class: "dim addon-offers", "{r.kind}" }
                                    span { class: "topbar-spacer" }
                                    button { class: "pill glass small", aria_label: "Delete remote {r.name}",
                                        onclick: move |_| {
                                            let n = rname.clone();
                                            let value = rname.clone();
                                            note.set(None); err.set(None);
                                            spawn(async move {
                                                match backend::on_runtime(async move {
                                                    tokio::task::spawn_blocking(move || rstream_rclone::delete_remote(&n)).await?
                                                }).await {
                                                    Ok(Ok(())) => {
                                                        backend::purge_disconnected_remote(&value);
                                                        if settings.peek().default_save_target.as_deref() == Some(value.as_str()) {
                                                            settings.set(backend::update_settings(|s| s.default_save_target = None));
                                                        }
                                                        if settings.peek().sync_remote.as_deref() == Some(value.as_str()) {
                                                            settings.set(backend::update_settings(|s| s.sync_remote = None));
                                                        }
                                                        remotes.restart();
                                                        note.set(Some(format!("Removed remote {value}.")));
                                                    }
                                                    Ok(Err(e)) | Err(e) => err.set(Some(format!("{e:#}"))),
                                                }
                                            });
                                        },
                                        icons::Trash {} span { "Delete" }
                                    }
                                }
                            }
                        }
                    }
                }
            } else if let Some(e) = load_err {
                p { class: "dim", "Rclone unavailable: {e}" }
            }

            if let Some(n) = note() { p { class: "check-note", role: "status", icons::Check {} span { "{n}" } } }
            if let Some(e) = err() { p { class: "addon-problem", role: "alert", icons::Alert {} span { "{e}" } } }

            // Interactive Rclone Remote Wizard
            div { class: "rclone-tabs glass", role: "tablist", aria_label: "Rclone setup tiers",
                button {
                    class: if tab() == RcloneTab::Guided { "rclone-tab on" } else { "rclone-tab" },
                    role: "tab", aria_selected: "{tab() == RcloneTab::Guided}",
                    onclick: move |_| tab.set(RcloneTab::Guided),
                    "Guided Provider Cards"
                }
                button {
                    class: if tab() == RcloneTab::AllProviders { "rclone-tab on" } else { "rclone-tab" },
                    role: "tab", aria_selected: "{tab() == RcloneTab::AllProviders}",
                    onclick: move |_| tab.set(RcloneTab::AllProviders),
                    "All Rclone Providers"
                }
                button {
                    class: if tab() == RcloneTab::ImportConf { "rclone-tab on" } else { "rclone-tab" },
                    role: "tab", aria_selected: "{tab() == RcloneTab::ImportConf}",
                    onclick: move |_| tab.set(RcloneTab::ImportConf),
                    "Import rclone.conf"
                }
            }

            if tab() != RcloneTab::ImportConf {
                div { class: "rclone-modes", role: "radiogroup", aria_label: "Remote setup mode",
                    button {
                        class: if !is_connect { "rclone-mode-btn on" } else { "rclone-mode-btn" },
                        role: "radio", aria_checked: "{!is_connect}",
                        onclick: move |_| setup_mode.set(rstream_rclone::RemoteSetupMode::CreateNew),
                        icons::Plus {} span { "Create New Remote" }
                    }
                    button {
                        class: if is_connect { "rclone-mode-btn on" } else { "rclone-mode-btn" },
                        role: "radio", aria_checked: "{is_connect}",
                        onclick: move |_| setup_mode.set(rstream_rclone::RemoteSetupMode::ConnectExisting),
                        icons::Users {} span { "Connect Existing Remote" }
                    }
                }
            }

            match tab() {
                RcloneTab::Guided => rsx! {
                    div { class: "rclone-wizard",
                        div { class: "rclone-cards", role: "radiogroup", aria_label: "Cloud providers",
                            for (idx, p) in GUIDED_PROVIDERS.iter().enumerate() {
                                button {
                                    key: "{p.prefix}",
                                    class: if guided_idx() == idx { "rclone-pcard on" } else { "rclone-pcard" },
                                    role: "radio",
                                    aria_checked: "{guided_idx() == idx}",
                                    onclick: move |_| {
                                        guided_idx.set(idx);
                                        params.set(HashMap::new());
                                        if !*name_touched.peek() || remote_name.peek().trim().is_empty() {
                                            remote_name.set(p.default_name.to_owned());
                                        }
                                    },
                                    div { class: "rclone-pcard-head",
                                        span { class: "rclone-pcard-title", "{p.title}" }
                                        span { class: "rclone-pcard-badge", "{p.badge}" }
                                    }
                                    p { class: "rclone-pcard-about", "{p.about}" }
                                }
                            }
                        }

                        div { class: "rclone-form-card glass",
                            h3 { class: "rclone-form-title",
                                if is_connect { "Connect Existing {guided.title}" } else { "Create New {guided.title}" }
                            }
                            if is_connect {
                                p { class: "rclone-connect-hint",
                                    "Used RStream on another PC or phone? Pick the same cloud provider, enter the exact same Remote Name, and sign in. Your cloud library and encrypted archives connect right away."
                                }
                            }
                            label { class: "rclone-label", "Remote Name" }
                            input {
                                class: "rclone-input",
                                placeholder: "{guided.default_name}",
                                aria_label: "Remote Name",
                                value: "{remote_name}",
                                oninput: move |e| {
                                    name_touched.set(true);
                                    remote_name.set(e.value());
                                },
                                onkeydown: move |e| e.stop_propagation(),
                            }

                            match guided.prefix {
                                "drive" | "onedrive" | "dropbox" => rsx! {
                                    div { class: "rclone-subcard",
                                        span { class: "rclone-subcard-title", "Optional: Custom {guided.title} OAuth Client ID & Secret" }
                                        p { class: "dim rclone-subcard-desc",
                                            "Supplying your own OAuth credentials avoids shared-quota rate limits and API throttling during large library scans and 4K streaming. Leave blank to use standard rclone OAuth."
                                        }
                                        label { class: "rclone-label", "Custom Client ID (optional)" }
                                        input {
                                            class: "rclone-input",
                                            placeholder: if guided.prefix == "drive" { "1234567890-abcdef.apps.googleusercontent.com" } else { "Client ID" },
                                            value: "{get_param(\"client_id\")}",
                                            oninput: move |e| set_param("client_id", e.value()),
                                            onkeydown: move |e| e.stop_propagation(),
                                        }
                                        label { class: "rclone-label", "Custom Client Secret (optional)" }
                                        SecretInput {
                                            placeholder: if guided.prefix == "drive" { "GOCSPX-..." } else { "Client Secret" },
                                            value: get_param("client_secret"),
                                            on_change: move |v| set_param("client_secret", v),
                                        }
                                        button {
                                            class: "link-btn",
                                            r#type: "button",
                                            style: "min-height: 32px; padding: 0; align-self: flex-start;",
                                            onclick: move |_| headless_open.set(!headless_open()),
                                            span { if headless_open() { "Hide headless token JSON" } else { "Headless / pre-authorized token JSON (optional)" } }
                                        }
                                        if headless_open() {
                                            SecretInput {
                                                placeholder: "OAuth token JSON ({{\"access_token\":\"…\"}})",
                                                value: get_param("token"),
                                                allow_paste: true,
                                                on_change: move |v| set_param("token", v),
                                            }
                                        }
                                    }
                                },
                                "protondrive" => rsx! {
                                    label { class: "rclone-label", "Proton Username / Email" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "you@proton.me",
                                        value: "{get_param(\"username\")}",
                                        oninput: move |e| set_param("username", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                    label { class: "rclone-label", "Proton Password" }
                                    SecretInput {
                                        placeholder: "Account password",
                                        value: get_param("password"),
                                        on_change: move |v| set_param("password", v),
                                    }
                                    label { class: "rclone-label", "2FA Code (optional)" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "6-digit TOTP code",
                                        value: "{get_param(\"2fa\")}",
                                        oninput: move |e| set_param("2fa", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                },
                                "s3" => rsx! {
                                    label { class: "rclone-label", "S3 Provider" }
                                    select {
                                        class: "select-field",
                                        style: "width: 100%; border-radius: 12px;",
                                        value: if get_param("provider").is_empty() { "Cloudflare".to_string() } else { get_param("provider") },
                                        onkeydown: move |e| e.stop_propagation(),
                                        onchange: move |e| set_param("provider", e.value()),
                                        for pv in ["Cloudflare", "AWS", "Wasabi", "Backblaze", "Minio", "DigitalOcean", "Other"] {
                                            option {
                                                value: "{pv}",
                                                selected: (if get_param("provider").is_empty() { "Cloudflare".to_string() } else { get_param("provider") }) == pv,
                                                "{pv}"
                                            }
                                        }
                                    }
                                    label { class: "rclone-label", "Endpoint URL" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "https://<accountid>.r2.cloudflarestorage.com",
                                        value: "{get_param(\"endpoint\")}",
                                        oninput: move |e| set_param("endpoint", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                    label { class: "rclone-label", "Access Key ID" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "Access Key ID",
                                        value: "{get_param(\"access_key_id\")}",
                                        oninput: move |e| set_param("access_key_id", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                    label { class: "rclone-label", "Secret Access Key" }
                                    SecretInput {
                                        placeholder: "Secret Access Key",
                                        value: get_param("secret_access_key"),
                                        allow_paste: true,
                                        on_change: move |v| set_param("secret_access_key", v),
                                    }
                                    label { class: "rclone-label", "Region (optional)" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "auto / us-east-1",
                                        value: "{get_param(\"region\")}",
                                        oninput: move |e| set_param("region", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                },
                                "webdav" => rsx! {
                                    label { class: "rclone-label", "WebDAV HTTPS URL" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "https://cloud.example.com/remote.php/dav/files/user/",
                                        value: "{get_param(\"url\")}",
                                        oninput: move |e| set_param("url", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                    label { class: "rclone-label", "Vendor" }
                                    select {
                                        class: "select-field",
                                        style: "width: 100%; border-radius: 12px;",
                                        value: if get_param("vendor").is_empty() { "nextcloud".to_string() } else { get_param("vendor") },
                                        onkeydown: move |e| e.stop_propagation(),
                                        onchange: move |e| set_param("vendor", e.value()),
                                        for vd in ["nextcloud", "owncloud", "sharepoint", "other"] {
                                            option {
                                                value: "{vd}",
                                                selected: (if get_param("vendor").is_empty() { "nextcloud".to_string() } else { get_param("vendor") }) == vd,
                                                "{vd}"
                                            }
                                        }
                                    }
                                    label { class: "rclone-label", "Username" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "Username",
                                        value: "{get_param(\"user\")}",
                                        oninput: move |e| set_param("user", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                    label { class: "rclone-label", "Password / App Token" }
                                    SecretInput {
                                        placeholder: "Password or app password",
                                        value: get_param("pass"),
                                        allow_paste: true,
                                        on_change: move |v| set_param("pass", v),
                                    }
                                },
                                "sftp" => rsx! {
                                    label { class: "rclone-label", "SSH Host" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "nas.local or 192.168.1.10",
                                        value: "{get_param(\"host\")}",
                                        oninput: move |e| set_param("host", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                    label { class: "rclone-label", "Port" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "22",
                                        value: "{get_param(\"port\")}",
                                        oninput: move |e| set_param("port", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                    label { class: "rclone-label", "SSH User" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "root / media",
                                        value: "{get_param(\"user\")}",
                                        oninput: move |e| set_param("user", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                    label { class: "rclone-label", "Password (optional if using key)" }
                                    SecretInput {
                                        placeholder: "SSH password",
                                        value: get_param("pass"),
                                        on_change: move |v| set_param("pass", v),
                                    }
                                    label { class: "rclone-label", "Private Key File Path (optional)" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "~/.ssh/id_ed25519",
                                        value: "{get_param(\"key_file\")}",
                                        oninput: move |e| set_param("key_file", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                },
                                "local" => rsx! {
                                    label { class: "rclone-label", "Local Folder Path (Root)" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "/mnt/storage/RStream",
                                        value: "{get_param(\"Root\")}",
                                        oninput: move |e| set_param("Root", e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                },
                                _ => rsx! {},
                            }

                            CryptSection {
                                is_connect,
                                enabled: crypt_enabled,
                                password: crypt_pw,
                                salt: crypt_salt,
                                folder: crypt_folder,
                            }

                            button {
                                class: "rclone-submit",
                                disabled: submit_disabled,
                                onclick: move |_| submit_wizard(),
                                if oauth_waiting() {
                                    "Waiting for browser sign-in…"
                                } else if saving() {
                                    "Configuring Remote…"
                                } else if is_connect {
                                    "Connect Existing Remote"
                                } else if crypt_enabled() {
                                    "Create Encrypted Remote"
                                } else {
                                    "Create New Remote"
                                }
                            }
                            if oauth_waiting() {
                                button {
                                    class: "pill glass small",
                                    r#type: "button",
                                    style: "align-self: center;",
                                    onclick: move |_| backend::cancel_rclone_oauth(),
                                    span { "Cancel browser sign-in" }
                                }
                            }
                        }
                    }
                },
                RcloneTab::AllProviders => {
                    let q = provider_search().trim().to_ascii_lowercase();
                    let filtered: Vec<rstream_rclone::ProviderInfo> = all_providers
                        .iter()
                        .filter(|p| {
                            q.is_empty()
                                || p.name.to_ascii_lowercase().contains(&q)
                                || p.prefix.to_ascii_lowercase().contains(&q)
                                || p.description.to_ascii_lowercase().contains(&q)
                        })
                        .cloned()
                        .collect();
                    let chosen = all_providers
                        .iter()
                        .find(|p| p.prefix == selected_prefix())
                        .or_else(|| filtered.first())
                        .cloned();
                    rsx! {
                        div { class: "rclone-form-card glass", style: "margin-top: 12px;",
                            h3 { class: "rclone-form-title",
                                if is_connect { "Connect Existing Rclone Backend" } else { "Configure Any Rclone Backend" }
                            }
                            div { class: "rclone-two-inputs",
                                div { class: "rclone-col",
                                    label { class: "rclone-label", "Search Providers ({filtered.len()})" }
                                    input {
                                        class: "rclone-input",
                                        placeholder: "Filter 70+ providers (b2, mega, pcloud, box, smb…)",
                                        value: "{provider_search}",
                                        oninput: move |e| provider_search.set(e.value()),
                                        onkeydown: move |e| e.stop_propagation(),
                                    }
                                }
                                div { class: "rclone-col",
                                    label { class: "rclone-label", "Backend Provider" }
                                    select {
                                        class: "select-field",
                                        style: "width: 100%; border-radius: 12px;",
                                        value: "{selected_prefix}",
                                        onkeydown: move |e| e.stop_propagation(),
                                        onchange: move |e| {
                                            let pfx = e.value();
                                            selected_prefix.set(pfx.clone());
                                            params.set(HashMap::new());
                                            if !*name_touched.peek() || remote_name.peek().trim().is_empty() {
                                                remote_name.set(pfx);
                                            }
                                        },
                                        for p in &filtered {
                                            option {
                                                key: "prov-{p.prefix}",
                                                value: "{p.prefix}",
                                                selected: selected_prefix() == p.prefix,
                                                "{p.prefix} — {p.description}"
                                            }
                                        }
                                    }
                                }
                            }

                            label { class: "rclone-label", "Remote Name" }
                            input {
                                class: "rclone-input",
                                placeholder: "Remote Name",
                                value: "{remote_name}",
                                oninput: move |e| {
                                    name_touched.set(true);
                                    remote_name.set(e.value());
                                },
                                onkeydown: move |e| e.stop_propagation(),
                            }

                            if let Some(prov) = chosen {
                                {
                                    let basic_opts: Vec<_> = prov.options.iter().filter(|o| !o.advanced).cloned().collect();
                                    let adv_opts: Vec<_> = prov.options.iter().filter(|o| o.advanced).cloned().collect();
                                    rsx! {
                                        if prov.is_oauth {
                                            p { class: "rclone-connect-hint",
                                                "OAuth provider detected: clicking Submit launches browser sign-in automatically unless you paste a pre-authorized token below."
                                            }
                                        }
                                        for opt in basic_opts {
                                            ProviderOptionField {
                                                key: "opt-{prov.prefix}-{opt.name}",
                                                opt: opt.clone(),
                                                value: get_param(&opt.name),
                                                on_change: move |(k, v): (String, String)| set_param(&k, v),
                                            }
                                        }
                                        if !adv_opts.is_empty() {
                                            button {
                                                class: "pill glass small",
                                                r#type: "button",
                                                style: "align-self: flex-start;",
                                                onclick: move |_| show_advanced.set(!show_advanced()),
                                                span { if show_advanced() { "Hide {adv_opts.len()} advanced options" } else { "Show {adv_opts.len()} advanced options" } }
                                            }
                                            if show_advanced() {
                                                for opt in adv_opts {
                                                    ProviderOptionField {
                                                        key: "adv-{prov.prefix}-{opt.name}",
                                                        opt: opt.clone(),
                                                        value: get_param(&opt.name),
                                                        on_change: move |(k, v): (String, String)| set_param(&k, v),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            if can_wrap_crypt {
                                CryptSection {
                                    is_connect,
                                    enabled: crypt_enabled,
                                    password: crypt_pw,
                                    salt: crypt_salt,
                                    folder: crypt_folder,
                                }
                            }

                            button {
                                class: "rclone-submit",
                                disabled: submit_disabled,
                                onclick: move |_| submit_wizard(),
                                if oauth_waiting() {
                                    "Waiting for browser sign-in…"
                                } else if saving() {
                                    "Configuring Remote…"
                                } else if is_connect {
                                    "Connect Existing Remote"
                                } else {
                                    "Create New Remote"
                                }
                            }
                            if oauth_waiting() {
                                button {
                                    class: "pill glass small",
                                    r#type: "button",
                                    style: "align-self: center;",
                                    onclick: move |_| backend::cancel_rclone_oauth(),
                                    span { "Cancel browser sign-in" }
                                }
                            }
                        }
                    }
                },
                RcloneTab::ImportConf => rsx! {
                    div { class: "rclone-form-card glass", style: "margin-top: 12px;",
                        h3 { class: "rclone-form-title", "Import rclone.conf" }
                        p { class: "dim setting-about", "Paste standard rclone.conf INI sections. Saved privately with owner-only 0600 permissions." }
                        textarea {
                            class: "import-area",
                            rows: "6",
                            placeholder: "Paste rclone.conf INI contents…",
                            value: "{import_raw}",
                            oninput: move |e| import_raw.set(e.value()),
                            onkeydown: move |e| e.stop_propagation(),
                        }
                        div { class: "account-actions",
                            button {
                                class: "pill accent small",
                                disabled: import_raw().trim().is_empty() || saving(),
                                onclick: move |_| {
                                    let raw = import_raw.peek().clone();
                                    saving.set(true);
                                    note.set(None); err.set(None);
                                    spawn(async move {
                                        let res = backend::on_runtime(async move {
                                            tokio::task::spawn_blocking(move || rstream_rclone::import_config(&raw)).await?
                                        }).await;
                                        match res {
                                            Ok(Ok(())) => {
                                                import_raw.set(String::new());
                                                remotes.restart();
                                                note.set(Some("Imported rclone.conf safely (0600).".into()));
                                            }
                                            Ok(Err(e)) | Err(e) => err.set(Some(format!("{e:#}"))),
                                        }
                                        saving.set(false);
                                    });
                                },
                                icons::Check {} span { "Save rclone.conf" }
                            }
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn ProviderOptionField(
    opt: rstream_rclone::ProviderOption,
    value: String,
    on_change: EventHandler<(String, String)>,
) -> Element {
    let first_help = opt.help.lines().next().unwrap_or("").trim().to_owned();
    let name_for_cb = opt.name.clone();
    let placeholder = if !opt.default_value.is_empty() {
        format!("Default: {}", opt.default_value)
    } else if let Some(ex) = opt.examples.first() {
        format!("e.g. {ex}")
    } else {
        opt.name.clone()
    };
    rsx! {
        div { class: "rclone-opt-field",
            label { class: "rclone-label",
                "{opt.name}"
                if opt.required { span { class: "rclone-req", " *required" } }
            }
            if !first_help.is_empty() {
                span { class: "dim rclone-opt-help", "{first_help}" }
            }
            if opt.is_password {
                SecretInput {
                    placeholder,
                    value,
                    allow_paste: true,
                    on_change: move |v| on_change.call((name_for_cb.clone(), v)),
                }
            } else {
                input {
                    class: "rclone-input",
                    placeholder: "{placeholder}",
                    value: "{value}",
                    oninput: move |e| on_change.call((name_for_cb.clone(), e.value())),
                    onkeydown: move |e| e.stop_propagation(),
                }
            }
        }
    }
}

#[component]
fn CryptSection(
    is_connect: bool,
    mut enabled: Signal<bool>,
    mut password: Signal<String>,
    mut salt: Signal<String>,
    mut folder: Signal<String>,
) -> Element {
    let on = enabled();
    rsx! {
        div { class: if on { "rclone-crypt-card on" } else { "rclone-crypt-card" },
            div { class: "rclone-crypt-head",
                span { class: "rclone-crypt-lock", icons::Lock {} }
                div { class: "rclone-crypt-copy",
                    span { class: "rclone-crypt-title",
                        if is_connect {
                            "Encrypted remote (crypt — enter Recovery Password & Salt)"
                        } else {
                            "Encrypt remote (crypt)"
                        }
                    }
                    span { class: "dim rclone-crypt-desc",
                        if is_connect {
                            "Enable if this existing remote uses rclone crypt encryption. Enter the exact recovery password and optional salt from your Recovery Kit."
                        } else {
                            "Automatically wraps this remote in zero-knowledge rclone crypt encryption (obscuring passwords cleanly with 0600 config permissions)."
                        }
                    }
                }
                button {
                    class: if on { "rclone-switch on" } else { "rclone-switch" },
                    r#type: "button",
                    role: "switch",
                    aria_checked: "{on}",
                    aria_label: "Encrypt remote with rclone crypt",
                    onclick: move |_| enabled.set(!on),
                    span { class: "rclone-switch-knob" }
                }
            }
            if on {
                div { class: "rclone-crypt-fields",
                    SecretInput {
                        placeholder: if is_connect { "Crypt Recovery Password" } else { "Encryption Password" },
                        value: password(),
                        allow_paste: true,
                        on_change: move |v| password.set(v),
                    }
                    SecretInput {
                        placeholder: if is_connect { "Crypt Salt (password2)" } else { "Optional Salt (password2)" },
                        value: salt(),
                        allow_paste: true,
                        on_change: move |v| salt.set(v),
                    }
                    if is_connect {
                        label { class: "rclone-label", "Encrypted Folder (leave RStream or clear for root)" }
                        input {
                            class: "rclone-input",
                            placeholder: "RStream (or blank for root)",
                            value: "{folder}",
                            oninput: move |e| folder.set(e.value()),
                            onkeydown: move |e| e.stop_propagation(),
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn PlayerConfigBlock() -> Element {
    let mpv_dir = use_hook(backend::mpv_config_dir);
    let init_cfg = use_hook(|| {
        let dir = rstream_mpv_config::MpvConfigDir::open_or_seed(mpv_dir.clone()).ok();
        let mpv_txt = dir.as_ref().and_then(|d| d.read_mpv_conf().ok()).unwrap_or_default();
        let inp_txt = dir.as_ref().and_then(|d| d.read_input_conf().ok()).unwrap_or_default();
        let shaders = dir.as_ref().map(|d| d.list_shaders()).unwrap_or_default();
        (mpv_txt, inp_txt, shaders)
    });
    let mut mpv_text = use_signal(|| init_cfg.0.clone());
    let mut input_text = use_signal(|| init_cfg.1.clone());
    let mut shaders = use_signal(|| init_cfg.2.clone());
    let mut edit_open = use_signal(|| false);
    let mut note = use_signal(|| None::<String>);
    let mut err = use_signal(|| None::<String>);
    rsx! {
        section { class: "settings-block",
            div { class: "group-head",
                span { class: "group-icon", icons::Film {} }
                span { class: "group-name", "Player config & GLSL shaders" }
                span { class: "topbar-spacer" }
                button { class: "pill glass small",
                    onclick: move |_| {
                        if let Ok(d) = rstream_mpv_config::MpvConfigDir::open_or_seed(mpv_dir.clone()) {
                            shaders.set(d.list_shaders());
                        }
                        edit_open.set(!edit_open());
                    },
                    icons::Pen {} span { if edit_open() { "Hide configs" } else { "Edit mpv.conf & input.conf" } }
                }
            }
            div { class: "setting-row glass",
                div { class: "setting-copy",
                    span { class: "setting-name", "Drop-in folder & shaders" }
                    span { class: "dim setting-about", {format!("Folder: {} · {} shader(s) found in shaders/.", mpv_dir.display(), shaders.read().len())} }
                }
            }
            if !shaders.read().is_empty() {
                div { class: "addon-list",
                    for sh in shaders.read().iter() {
                        div { key: "sh-{sh.name}", class: "addon-row glass",
                            span { class: "addon-name", "{sh.name}" }
                            if let Some(d) = &sh.description { span { class: "dim addon-offers", "{d}" } }
                        }
                    }
                }
            }
            if edit_open() {
                div { class: "account-form glass",
                    span { class: "setting-name", "mpv.conf" }
                    textarea { class: "import-area", rows: "6", value: "{mpv_text}",
                        oninput: move |e| mpv_text.set(e.value()), onkeydown: move |e| e.stop_propagation() }
                    span { class: "setting-name", "input.conf" }
                    textarea { class: "import-area", rows: "6", value: "{input_text}",
                        oninput: move |e| input_text.set(e.value()), onkeydown: move |e| e.stop_propagation() }
                    div { class: "account-actions",
                        button { class: "pill accent small",
                            onclick: {
                                let dir_path = mpv_dir.clone();
                                move |_| {
                                    note.set(None); err.set(None);
                                    match rstream_mpv_config::MpvConfigDir::open_or_seed(dir_path.clone()) {
                                        Ok(d) => {
                                            if let Err(e) = d.write_mpv_conf(&mpv_text.peek()) { err.set(Some(format!("{e}"))); return; }
                                            if let Err(e) = d.write_input_conf(&input_text.peek()) { err.set(Some(format!("{e}"))); return; }
                                            let opts = d.parse_mpv_conf().map(|p| p.options.len()).unwrap_or(0);
                                            let binds = d.parse_input_conf().map(|b| b.len()).unwrap_or(0);
                                            shaders.set(d.list_shaders());
                                            note.set(Some(format!("Saved ({opts} safe mpv option(s), {binds} input.conf command(s)).")));
                                        }
                                        Err(e) => err.set(Some(format!("{e}"))),
                                    }
                                }
                            },
                            icons::Check {} span { "Save configs" }
                        }
                    }
                }
            }
            if let Some(n) = note() { p { class: "check-note", role: "status", icons::Check {} span { "{n}" } } }
            if let Some(e) = err() { p { class: "addon-problem", role: "alert", icons::Alert {} span { "{e}" } } }
        }
    }
}
