//! RStream in Dioxus: the shell (top bar, keys, player status), its screens and the in-app player, on the Rust core.
// Windows release builds are a GUI app: no console window behind RStream.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
mod addons;
#[cfg(target_os = "android")]
mod android;
mod backend;
mod components;
mod engine;
mod home;
mod library;
mod icons;
mod player;
mod player_rules;
mod player_ui;
mod search;
#[cfg(not(target_os = "android"))]
mod selftest;
mod settings;
mod shortcuts;
mod theatre;
mod title;
mod tracks;
mod video;
mod display;

use backend::Media;
use theatre::Theatre;
use base64::Engine;
use dioxus::prelude::*;

/// The app's styles, in cascade order (src/styles).
const STYLE: &str = concat!(include_str!("styles/base.css"), include_str!("styles/screens.css"), include_str!("styles/sizes.css"),
    include_str!("styles/addons.css"), include_str!("styles/player.css"), include_str!("styles/pages.css"), include_str!("styles/motion.css"),
    include_str!("styles/tv.css"));
const ARCHIVO: &[u8] = include_bytes!("../assets/fonts/archivo.ttf");

/// Whether Veylo runs on a television (Android TV and TVs that say so): Back never leaves the app,
/// shelves are shorter, and tv.js drives focus from the remote.
pub fn is_tv() -> bool {
    #[cfg(target_os = "android")]
    { android::is_tv() }
    #[cfg(not(target_os = "android"))]
    { false }
}

/// Destinations, in the Android app's order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tab { Home, Library, Settings }

impl Tab {
    const ALL: [Tab; 3] = [Tab::Home, Tab::Library, Tab::Settings];
    fn title(self) -> &'static str {
        match self { Tab::Home => "Home", Tab::Library => "Library", Tab::Settings => "Settings" }
    }
}

/// What the status chip shows while the platform player works.
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerStatus {
    /// Checking sources before Play.
    Finding(String),
    /// Connecting to BitTorrent peers / resolving magnet metadata before playback opens.
    Connecting(String),
    Opening(String),
    Playing { title: String, codec: String, hardware: Option<String>, facts: player::Facts },
    Failed(String),
}

/// Shared shell state, provided once at the root.
#[derive(Clone, Copy)]
pub struct Shell {
    pub tab: Signal<Tab>,
    /// What the player is doing, shown as a status line; None when idle.
    pub player_status: Signal<Option<PlayerStatus>>,
    pub search_open: Signal<bool>,
    /// What is typed in Search, kept so every way of closing it (Esc, Back, Close, a result)
    /// remembers the query.
    pub search_draft: Signal<String>,
    /// A title's videos playing in the in-app theatre (trailers, teasers, featurettes).
    pub trailer: Signal<Option<Theatre>>,
    /// The app's shortcuts sheet.
    pub shortcuts_open: Signal<bool>,
    /// A key pressed on the title page (play, like, save, ...).
    pub title_command: Signal<Option<char>>,
    /// A query Search should open with (a name tapped on a title page).
    pub search_query: Signal<Option<String>>,
    pub opened: Signal<Option<Media>>,
    /// Artwork that lends the accent: the opened title, else the current screen.
    pub lamp_source: Signal<String>,
    pub history: Signal<backend::History>,
    /// Bumped after playback or library edits so LastPlayed refreshes.
    pub history_dirty: Signal<u32>,
    /// Installed add-ons with a newer version you have not applied (a dot on Settings).
    pub addon_updates: Signal<usize>,
    /// The in-app player's session while it is on screen.
    pub player: Signal<Option<engine::LiveRef>>,
    /// A key pressed while the player is on screen, for its controls.
    pub player_key: Signal<Option<String>>,
    /// Bumped when the viewer backs out of the player while its source is still being found.
    pub play_ticket: Signal<u64>,
}

impl Shell {
    /// The player is up but nothing plays yet: a source is being found or opened.
    pub fn pending(&self) -> Option<String> {
        if self.player.peek().is_some() { return None; }
        match &*self.player_status.peek() {
            Some(PlayerStatus::Finding(t) | PlayerStatus::Connecting(t) | PlayerStatus::Opening(t)) => Some(t.clone()),
            _ => None,
        }
    }
}

/// Open the player at once while a source is found; the finder starts playback only while
/// `still_wanted` holds for the ticket returned here.
pub fn finding(mut shell: Shell, title: String) -> u64 {
    *shell.play_ticket.write() += 1;
    shell.player_status.set(Some(PlayerStatus::Finding(title)));
    *shell.play_ticket.peek()
}

pub fn still_wanted(shell: Shell, ticket: u64) -> bool { *shell.play_ticket.peek() == ticket }

/// Close Search, remembering what was typed (two characters or more).
pub fn close_search(mut shell: Shell) {
    let draft = shell.search_draft.peek().clone();
    if draft.trim().chars().count() >= 2 { backend::remember_query(&mut shell.history.write(), &draft); }
    if !draft.is_empty() { shell.search_draft.set(String::new()); }
    if *shell.search_open.peek() { shell.search_open.set(false); }
}

/// How long a playback failure stays on screen.
const FAILURE_SHOWN_MS: u64 = 8000;

/// Back out of the player before anything plays: the search in progress is dropped.
pub fn cancel_finding(mut shell: Shell) {
    *shell.play_ticket.write() += 1;
    shell.player_status.set(None);
    backend::stop_torrent();
}

fn main() {
    #[cfg(target_os = "android")]
    { dioxus::launch(App); }
    #[cfg(not(target_os = "android"))]
    launch_desktop();
}

/// Developer command: `rstream --sources <type> <video-id> [episodes in the season]` prints the Play order and which
/// source passes the one-byte check. File names, sizes and labels only; never URLs.
#[cfg(not(target_os = "android"))]
fn print_sources(kind: &str, id: &str, pack: Option<usize>) {
    if let Err(e) = backend::init() { eprintln!("{e:#}"); return; }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let (list, failed) = match backend::sources(kind, id).await { Ok(r) => r, Err(e) => { eprintln!("{e:#}"); return; } };
        println!("{} sources, offline add-ons: {failed:?}", list.len());
        let order = backend::play_order(&list, pack);
        let torrents = backend::torrent_order(&list, pack);
        let shown = if order.is_empty() { &torrents } else { &order };
        for (i, s) in shown.iter().take(8).enumerate() {
            println!("{:>2}. [{} {}] {} | {} | {}", i + 1, s.quality.unwrap_or("?"), s.hdr.unwrap_or("SDR"),
                if s.filename.is_empty() { s.description.lines().next().unwrap_or("") } else { &s.filename }, s.size.as_deref().unwrap_or("size?"), s.addon);
        }
        let mut pick = backend::pick_source(order, pack).await;
        if pick.best.is_none() {
            pick.best = torrents.into_iter().next();
            if pick.best.is_some() { pick.provider_status = None; }
        }
        let (best, provider_status) = (pick.best, pick.provider_status);
        println!("Play would start: {}", best.map(|s| if s.filename.is_empty() { s.description.lines().next().unwrap_or("").to_owned() } else { s.filename }).unwrap_or_else(|| format!("nothing ({provider_status:?})")));
    });
}

/// Developer command: `rstream --addons` runs the start-up add-on setup (replacing
/// website installs, update check) and prints names and versions; never links.
#[cfg(not(target_os = "android"))]
fn print_addons() {
    if let Err(e) = backend::init() { eprintln!("{e:#}"); return; }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let updates = addons::startup().await;
        let ov = addons::overview().await;
        println!("Add-on updates: {updates}");
        for (c, row) in &ov.curated { println!("{}: {}", c.name(), row.as_ref().map(|r| format!("v{} {:?}", r.version, r.update)).unwrap_or_else(|| "not installed".into())); }
        for r in &ov.others { println!("other: {} v{} {:?}", r.name, r.version, r.update); }
    });
}

#[cfg(any(target_os = "linux", test))]
fn theme_from_name(name: &str) -> Option<dioxus::desktop::tao::window::Theme> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("dark") { Some(dioxus::desktop::tao::window::Theme::Dark) }
    else if lower.contains("light") { Some(dioxus::desktop::tao::window::Theme::Light) }
    else { None }
}

#[cfg(any(target_os = "linux", test))]
fn parse_gtk_settings_theme(ini: &str) -> Option<dioxus::desktop::tao::window::Theme> {
    let mut theme_name = None;
    for line in ini.lines().map(str::trim) {
        if line.starts_with('#') || line.starts_with(';') || line.starts_with('[') { continue; }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim().trim_matches('"').trim_matches('\''));
        if k == "gtk-application-prefer-dark-theme" && matches!(v.to_ascii_lowercase().as_str(), "1" | "true") {
            return Some(dioxus::desktop::tao::window::Theme::Dark);
        }
        if k == "gtk-theme-name" { theme_name = theme_from_name(v); }
    }
    theme_name
}

#[cfg(any(target_os = "linux", test))]
fn parse_kdeglobals_theme(ini: &str) -> Option<dioxus::desktop::tao::window::Theme> {
    let mut in_window = false;
    let mut first = None;
    for line in ini.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') { in_window = line == "[Colors:Window]"; continue; }
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim() != "BackgroundNormal" { continue; }
        let mut nums = v.split(',').filter_map(|n| n.trim().parse::<u16>().ok());
        if let (Some(r), Some(g), Some(b)) = (nums.next(), nums.next(), nums.next()) {
            let theme = if (r + g + b) / 3 < 128 { dioxus::desktop::tao::window::Theme::Dark } else { dioxus::desktop::tao::window::Theme::Light };
            if in_window { return Some(theme); }
            first.get_or_insert(theme);
        }
    }
    first
}

#[cfg(not(target_os = "android"))]
pub(crate) fn preferred_desktop_theme() -> Option<dioxus::desktop::tao::window::Theme> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(t) = std::env::var("GTK_THEME") {
            if let Some(theme) = theme_from_name(&t) { return Some(theme); }
        }
        let cfg = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()).map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").filter(|s| !s.is_empty()).map(|h| std::path::PathBuf::from(h).join(".config")));
        if let Some(cfg) = cfg {
            if let Ok(ini) = std::fs::read_to_string(cfg.join("gtk-3.0/settings.ini")) {
                if let Some(theme) = parse_gtk_settings_theme(&ini) { return Some(theme); }
            }
            if let Ok(ini) = std::fs::read_to_string(cfg.join("kdeglobals")) {
                if let Some(theme) = parse_kdeglobals_theme(&ini) { return Some(theme); }
            }
        }
    }
    Some(dioxus::desktop::tao::window::Theme::Dark)
}

#[cfg(target_os = "linux")]
fn select_connected_drm_render_node() -> Option<String> {
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((card, _)) = name.split_once('-') else { continue };
        if !card.strip_prefix("card").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) { continue; }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else { continue };
        if status.trim() != "connected" { continue; }
        let Ok(drm_nodes) = std::fs::read_dir(format!("/sys/class/drm/{card}/device/drm")) else { continue };
        if let Some(render) = drm_nodes.flatten().find_map(|n| {
            let s = n.file_name().to_string_lossy().into_owned();
            s.starts_with("renderD").then_some(s)
        }) {
            return Some(format!("/dev/dri/{render}"));
        }
    }
    None
}

#[cfg(not(target_os = "android"))]
fn launch_desktop() {
    let args: Vec<String> = std::env::args().collect();
    // Windows GUI builds have no console of their own: the developer commands print to the terminal that started them.
    #[cfg(windows)]
    if args.len() > 1 {
        extern "system" { fn AttachConsole(process: u32) -> i32; }
        const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS); }
    }
    if args.get(1).map(String::as_str) == Some("--sources") && args.len() >= 4 { print_sources(&args[2], &args[3], args.get(4).and_then(|n| n.parse().ok())); return; }
    if args.get(1).map(String::as_str) == Some("--addons") { print_addons(); return; }
    if args.get(1).map(String::as_str) == Some("--self-test") { std::process::exit(selftest::run(&args[2..])); }
    if args.get(1).map(String::as_str) == Some("--check-picture") && args.len() >= 3 {
        let report = args.iter().position(|a| a == "--report").and_then(|i| args.get(i + 1));
        std::process::exit(selftest::check_picture(&args[2], report.map(String::as_str)));
    }
    use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
    // WebKitGTK's DMA-BUF renderer repaints cleanly when a GPU render node exists and WebKit's
    // sandbox allows loading NixOS's `/run/opengl-driver/lib` EGL/GBM; fall back to SHM only when
    // no GPU render node is present (e.g. pure software Xvfb CI).
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_none()
            && (std::path::Path::new("/nix/store").exists() || std::env::var_os("APPIMAGE").is_some())
        {
            std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
        }
        let render_count = std::fs::read_dir("/dev/dri")
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
            .count();
        if render_count == 0 && std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        } else if render_count >= 2 && std::env::var_os("WEBKIT_WEB_RENDER_DEVICE_FILE").is_none() {
            if let Some(dev) = select_connected_drm_render_node() {
                std::env::set_var("WEBKIT_WEB_RENDER_DEVICE_FILE", &dev);
                if std::env::var_os("WEBKIT_DMABUF_RENDERER_DRM_DEVICE").is_none() {
                    std::env::set_var("WEBKIT_DMABUF_RENDERER_DRM_DEVICE", &dev);
                }
            }
        }
        if std::env::var_os("GTK_DATA_PREFIX").is_none() {
            let user = std::env::var("USER").unwrap_or_default();
            let candidates = [
                (!user.is_empty()).then(|| format!("/etc/profiles/per-user/{user}")),
                Some("/run/current-system/sw".into()),
            ];
            if let Some(prefix) = candidates.into_iter().flatten().find(|p| std::path::Path::new(&format!("{p}/share/themes")).is_dir()) {
                std::env::set_var("GTK_DATA_PREFIX", prefix);
            }
        }
    }
    let window = WindowBuilder::new()
        .with_title("RStream")
        .with_theme(preferred_desktop_theme())
        .with_inner_size(LogicalSize::new(1200.0, 768.0))
        .with_min_inner_size(LogicalSize::new(360.0, 560.0));
    #[cfg(target_os = "linux")]
    let window = {
        use dioxus::desktop::tao::platform::unix::WindowBuilderExtUnix;
        window.with_transparent(false).with_rgba_visual(true)
    };
    #[cfg(not(target_os = "linux"))]
    let window = window
        // macOS: a transparent window is what stops the WKWebView drawing a background of its own.
        .with_transparent(cfg!(any(target_os = "windows", target_os = "macos")));
    // Desktop title bar, taskbar, and switcher icon (Windows exe icon is also embedded via windows/rstream.rc).
    let window = window.with_window_icon(window_icon());
    let bg = if cfg!(target_os = "linux") { (0, 0, 0, 0) } else { (0, 0, 0, 255) };
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(window).with_menu(None).with_background_color(bg))
        .launch(App);
}

#[cfg(not(target_os = "android"))]
static TRAY_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Apply the user's "Minimize to system tray on close" setting immediately when a system tray is active.
#[cfg(not(target_os = "android"))]
pub fn set_desktop_close_to_tray(enabled: bool) {
    use dioxus::desktop::WindowCloseBehaviour;
    let tray_ready = TRAY_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed);
    let behaviour = if enabled && tray_ready {
        WindowCloseBehaviour::WindowHides
    } else {
        WindowCloseBehaviour::WindowCloses
    };
    dioxus::desktop::window().set_close_behavior(behaviour);
}

#[cfg(target_os = "android")]
pub fn set_desktop_close_to_tray(_enabled: bool) {}

#[cfg(not(target_os = "android"))]
struct DesktopTrayHandles {
    _tray: dioxus::desktop::trayicon::TrayIcon,
    status_item: dioxus::desktop::trayicon::menu::MenuItem,
    pause_item: dioxus::desktop::trayicon::menu::MenuItem,
}

#[cfg(not(target_os = "android"))]
fn restore_desktop_window() {
    let win = dioxus::desktop::window();
    #[cfg(target_os = "linux")]
    {
        use dioxus::desktop::{tao::platform::unix::WindowExtUnix, wry::WebViewExtUnix};
        use gtk::prelude::*;
        let gtk_win = win.window.gtk_window().clone();
        let webview = win.webview.webview().clone();
        gtk_win.show_all();
        gtk_win.deiconify();
        gtk_win.present();
        webview.queue_draw();
        gtk::glib::idle_add_local_once(move || {
            gtk_win.present();
            webview.queue_draw();
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        win.set_visible(true);
        win.set_minimized(false);
        win.set_focus();
    }
    backend::trigger_profile_sync_now();
}

#[cfg(not(target_os = "android"))]
fn handle_tray_menu_action(shell: Shell, id: &str) {
    match id {
        "tray_open" => restore_desktop_window(),
        "tray_sync" => backend::trigger_profile_sync_now(),
        "tray_pause_all" => backend::pause_all_downloads(),
        "tray_quit" => {
            player::stop_current();
            if shell.pending().is_some() {
                cancel_finding(shell);
            }
            backend::pause_all_downloads();
            backend::flush_writes();
            let win = dioxus::desktop::window();
            win.set_close_behavior(dioxus::desktop::WindowCloseBehaviour::WindowCloses);
            win.close();
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(250));
                std::process::exit(0);
            });
        }
        _ => {}
    }
}

#[cfg(not(target_os = "android"))]
fn init_desktop_tray() -> Option<std::rc::Rc<DesktopTrayHandles>> {
    use dioxus::desktop::trayicon::{
        menu::{Menu, MenuItem, PredefinedMenuItem},
        Icon, TrayIconBuilder,
    };
    let img = image::load_from_memory(include_bytes!("../../../branding/rstream-icon.png"))
        .ok()?
        .thumbnail(64, 64)
        .to_rgba8();
    let (w, h) = img.dimensions();
    let icon = Icon::from_rgba(img.into_raw(), w, h).ok()?;
    let menu = Menu::new();
    let status_item = MenuItem::with_id("tray_status", "RStream — Idle", false, None);
    let open_item = MenuItem::with_id("tray_open", "Open RStream", true, None);
    let sync_item = MenuItem::with_id("tray_sync", "Sync Library & Progress Now", true, None);
    let pause_item = MenuItem::with_id("tray_pause_all", "Pause All Downloads", false, None);
    let quit_item = MenuItem::with_id("tray_quit", "Quit RStream", true, None);
    let _ = menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &open_item,
        &sync_item,
        &pause_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]);
    let tray = TrayIconBuilder::new()
        .with_tooltip("RStream")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_icon(icon)
        .build()
        .ok()?;
    TRAY_AVAILABLE.store(true, std::sync::atomic::Ordering::Relaxed);
    set_desktop_close_to_tray(backend::initial_close_to_tray());
    Some(std::rc::Rc::new(DesktopTrayHandles {
        _tray: tray,
        status_item,
        pause_item,
    }))
}

/// The app icon for desktop windows and taskbars.
#[cfg(not(target_os = "android"))]
fn window_icon() -> Option<dioxus::desktop::tao::window::Icon> {
    let image = image::load_from_memory(include_bytes!("../../../branding/rstream-icon.png")).ok()?.thumbnail(256, 256).to_rgba8();
    let (width, height) = image.dimensions();
    dioxus::desktop::tao::window::Icon::from_rgba(image.into_raw(), width, height).ok()
}

fn font_css() -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(ARCHIVO);
    format!("@font-face{{font-family:'Archivo';src:url(data:font/ttf;base64,{data}) format('truetype');\
        font-weight:100 900;font-stretch:62% 125%;font-display:block}}")
}

/// Developer option: RSTREAM_TAB=home|library|settings picks the first screen.
fn start_tab() -> Tab {
    match std::env::var("RSTREAM_TAB").unwrap_or_default().to_lowercase().as_str() {
        "library" => Tab::Library, "settings" => Tab::Settings, _ => Tab::Home,
    }
}

/// The system bars' heights in CSS px (top, bottom); nothing to reserve on the desktop.
fn system_bar_insets() -> (f32, f32) {
    #[cfg(target_os = "android")]
    { android::system_bar_insets().unwrap_or((24.0, 24.0)) }
    #[cfg(not(target_os = "android"))]
    { (0.0, 0.0) }
}

/// The backend starts once the window exists: on Android the JNI context
/// (files folder, player) is only available from then on.
fn root_signal<T: 'static>(v: T) -> Signal<T> {
    Signal::new_in_scope(v, dioxus::core::ScopeId::ROOT)
}

#[component]
fn App() -> Element {
    let started = use_hook(|| backend::init().map_err(|e| format!("{e:#}")));
    let init_history = started.as_ref().map(|_| backend::load_history()).unwrap_or_default();
    use_context_provider(|| Shell {
        tab: root_signal(start_tab()),
        player_status: root_signal(None),
        search_open: root_signal(std::env::var_os("RSTREAM_SEARCH").is_some()),
        search_draft: root_signal(String::new()),
        search_query: root_signal(None),
        shortcuts_open: root_signal(false),
        trailer: root_signal(None),
        title_command: root_signal(None),
        opened: root_signal(None),
        lamp_source: root_signal(String::new()),
        history: root_signal(init_history),
        history_dirty: root_signal(0),
        addon_updates: root_signal(0),
        player: root_signal(None),
        player_key: root_signal(None),
        play_ticket: root_signal(0),
    });
    #[cfg(not(target_os = "android"))]
    use_hook(crate::video::init_opaque);
    match started {
        Ok(()) => rsx! { Shell_ {} },
        Err(error) => rsx! { style { {STYLE} } div { class: "center-note", div { class: "glass panel", p { "RStream couldn’t start" } p { class: "dim", "{error}" } } } },
    }
}

#[component]
#[allow(non_snake_case)]
fn Shell_() -> Element {
    let fonts = use_hook(font_css);
    let shell = use_context::<Shell>();
    // Developer option: RSTREAM_PLAY_DEMO=1 (desktop) or files/dev-play.json (Android) plays at launch.
    use_hook(move || {
        #[cfg(target_os = "android")]
        if let Some(list) = android::take_dev_autoplay() { spawn(backend::dev_autoplay(list)); }
        if let Some(list) = std::env::var_os("RSTREAM_DEV_AUTOPLAY").and_then(|p| std::fs::read_to_string(p).ok()) { spawn(backend::dev_autoplay(list)); }
        #[cfg(target_os = "android")]
        if let Some(v) = android::take_dev_play() {
            let text = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
            start_playback(shell, player::PlayRequest { title: text("title"), subtitle: "Test player".into(), url: text("url"),
                hdr: Some(text("hdr")).filter(|h| !h.is_empty()), ..Default::default() }, None);
        }
        #[cfg(not(target_os = "android"))]
        if std::env::var("RSTREAM_PLAY_DEMO").as_deref() == Ok("hdr") {
            let clip = std::env::temp_dir().join("rstream-demo-hdr.mp4");
            if std::fs::write(&clip, selftest::HDR_CLIP).is_ok() {
                start_playback(shell, player::PlayRequest { title: "HDR10 test clip".into(), subtitle: "Test player".into(), url: clip.to_string_lossy().into_owned(), ..Default::default() }, None);
            }
            return;
        }
        if std::env::var("RSTREAM_PLAY_DEMO").as_deref() == Ok("bars") {
            let clip = std::env::temp_dir().join("rstream-colour-bars.y4m");
            if std::fs::write(&clip, veylo_picture_check::y4m(180)).is_ok() {
                start_playback(shell, player::PlayRequest { title: "Colour bars".into(), subtitle: "Picture test".into(), url: clip.to_string_lossy().into_owned(), ..Default::default() }, None);
            }
            return;
        }
        if std::env::var_os("RSTREAM_PLAY_DEMO").is_some() {
            start_playback(shell, player::PlayRequest { title: "Sintel trailer".into(), subtitle: "Test player".into(), url: backend::DEMO_STREAM.into(), ..Default::default() }, None);
        }
    });
    // Once per launch: refresh managed add-ons and check for new versions.
    let mut addon_updates = shell.addon_updates;
    use_future(move || async move { addon_updates.set(addons::startup().await); });

    #[cfg(not(target_os = "android"))]
    let tray_handles = use_hook(init_desktop_tray);

    #[cfg(not(target_os = "android"))]
    {
        dioxus::desktop::use_tray_icon_event_handler(move |event| {
            if let dioxus::desktop::trayicon::TrayIconEvent::Click { button: dioxus::desktop::trayicon::MouseButton::Left, .. } = event {
                restore_desktop_window();
            }
        });
        // `dioxus-desktop` 0.7 registers `muda::MenuEvent::set_event_handler` (an `OnceCell`) for
        // `MudaMenuEvent` before `TrayMenuEvent`, so tray menu activations arrive on `use_muda_event_handler`.
        dioxus::desktop::use_muda_event_handler(move |event| {
            handle_tray_menu_action(shell, event.id().0.as_str());
        });
        dioxus::desktop::use_tray_menu_event_handler(move |event| {
            handle_tray_menu_action(shell, event.id.0.as_str());
        });
        let last_focus_sync = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(0i64)));
        dioxus::desktop::use_wry_event_handler(move |event, _| {
            use dioxus::desktop::tao::event::{Event, WindowEvent};
            if let Event::WindowEvent { event, .. } = event {
                match event {
                    WindowEvent::CloseRequested => {
                        player::stop_current();
                        if shell.pending().is_some() {
                            cancel_finding(shell);
                        }
                        if TRAY_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed) && backend::load_settings().close_to_tray {
                            backend::trigger_profile_sync_now();
                        } else {
                            backend::pause_all_downloads();
                            backend::flush_writes();
                        }
                    }
                    WindowEvent::Focused(true) => {
                        let now = backend::now_secs();
                        if now - last_focus_sync.get() >= 60 {
                            last_focus_sync.set(now);
                            backend::trigger_profile_sync_now();
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // Observe remote profile sync completions & update desktop tray status + 5-min playback sync heartbeat.
    {
        #[cfg(not(target_os = "android"))]
        let tray_for_loop = tray_handles.clone();
        use_future(move || {
            #[cfg(not(target_os = "android"))]
            let tray_ref = tray_for_loop.clone();
            async move {
                let mut seen_epoch = backend::profile_sync_epoch();
                let mut ticks: u32 = 0;
                #[cfg(not(target_os = "android"))]
                let mut last_tray_status = String::new();
                #[cfg(not(target_os = "android"))]
                let mut last_pause_enabled = false;
                loop {
                    backend::sleep(1000).await;
                    ticks = ticks.wrapping_add(1);
                    let cur_epoch = backend::profile_sync_epoch();
                    if cur_epoch != seen_epoch {
                        seen_epoch = cur_epoch;
                        let mut hd = shell.history_dirty;
                        *hd.write() += 1;
                        crate::home::invalidate_cache();
                    }
                    // Coarse 5-minute watch-progress sync heartbeat during long movies.
                    if ticks % 300 == 0 && shell.player.peek().is_some() {
                        backend::trigger_profile_sync();
                    }
                    #[cfg(not(target_os = "android"))]
                    if let Some(ref th) = tray_ref {
                        let act = backend::background_activity();
                        let total_jobs = act.running_downloads + act.queued_downloads;
                        let pct = (act.total_bytes > 0)
                            .then(|| ((act.downloaded_bytes.saturating_mul(100)) / act.total_bytes).min(100));
                        let label = if total_jobs > 0 {
                            match (pct, act.primary_title.as_deref()) {
                                (Some(p), Some(t)) => format!("Saving {total_jobs} · {p}% ({t})"),
                                (None, Some(t)) => format!("Saving {total_jobs} ({t})"),
                                (Some(p), None) => format!("Saving {total_jobs} item(s) · {p}%"),
                                (None, None) => format!("Saving {total_jobs} item(s)…"),
                            }
                        } else if act.db_syncing {
                            "Syncing library & database…".into()
                        } else {
                            "RStream — Idle".into()
                        };
                        if label != last_tray_status {
                            th.status_item.set_text(&label);
                            let _ = th._tray.set_tooltip(Some(&label));
                            last_tray_status = label;
                        }
                        let can_pause = total_jobs > 0;
                        if can_pause != last_pause_enabled {
                            th.pause_item.set_enabled(can_pause);
                            last_pause_enabled = can_pause;
                        }
                    }
                }
            }
        });
    }
    let Shell { tab, mut search_open, mut opened, lamp_source, player_status, mut shortcuts_open, mut title_command, .. } = shell;
    let lamp = use_resource(move || async move { backend::accent(lamp_source()).await });
    let lamp_css = lamp().flatten().unwrap_or_else(|| "#f2efea".into());
    // Android draws the WebView under its bars: reserve their real heights everywhere, read again
    // whenever the window changes size (rotation, split screen, a new navigation mode).
    let mut insets = use_signal(system_bar_insets);

    // Android Back closes a page, Search or the videos, like Esc; on a top-level screen it leaves the app.
    // On a TV it never leaves: Back only goes back (to Home at most), and the remote's Home key leaves.
    #[cfg(target_os = "android")]
    use_effect(move || { let _ = android::set_back_enabled(is_tv() || shortcuts_open() || search_open() || opened.read().is_some() || (shell.player)().is_some()
        || (shell.trailer)().is_some() || matches!(player_status(), Some(PlayerStatus::Finding(_) | PlayerStatus::Connecting(_) | PlayerStatus::Opening(_)))); });
    // A failure says why once; it doesn't stay over the app (a new playback replaces it sooner).
    let mut status = shell.player_status;
    use_effect(move || {
        let Some(PlayerStatus::Failed(error)) = player_status() else { return };
        spawn(async move {
            backend::sleep(FAILURE_SHOWN_MS).await;
            if matches!(&*status.peek(), Some(PlayerStatus::Failed(e)) if *e == error) { status.set(None); }
        });
    });
    // The pending overlay can finish asynchronously while its help is open.
    use_effect(move || { if (shell.player)().is_some() && *shortcuts_open.peek() { shortcuts_open.set(false); } });

    // App shortcuts (the ? sheet lists them). Typing in Search or a field never triggers them.
    let on_key = move |e: KeyboardEvent| {
        if shortcuts_open() {
            if matches!(e.key(), Key::Escape | Key::Backspace) || e.key() == Key::Character("?".into()) {
                e.prevent_default(); shortcuts_open.set(false);
            }
            return;
        }
        // The player's controls take every key while it is on screen.
        if shell.player.peek().is_some() {
            if e.key() == Key::Tab { return; }
            e.prevent_default();
            let mut key = shell.player_key;
            let k = e.key().to_string();
            let mut prefix = String::new();
            if e.modifiers().ctrl() { prefix.push_str("Ctrl+"); }
            if e.modifiers().alt() { prefix.push_str("Alt+"); }
            if e.modifiers().meta() { prefix.push_str("Meta+"); }
            if e.modifiers().shift() && k.chars().count() > 1 { prefix.push_str("Shift+"); }
            key.set(Some(format!("{prefix}{k}")));
            return;
        }
        // The player waiting for its source: Esc or Back leaves it, other keys wait.
        if shell.pending().is_some() {
            if e.key() == Key::Tab { return; }
            e.prevent_default();
            if matches!(e.key(), Key::Escape | Key::Backspace) { cancel_finding(shell); }
            else if e.key() == Key::Character("?".into()) { shortcuts_open.set(true); }
            return;
        }
        let ctrl = e.modifiers().ctrl() || e.modifiers().meta();
        let go = |mut tab: Signal<Tab>, mut opened: Signal<Option<Media>>, to: Tab| { close_search(shell); opened.set(None); tab.set(to); };
        match e.key() {
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("k") => { e.prevent_default(); search_open.set(true); }
            Key::Character(c) if ctrl && ["1", "2", "3"].contains(&c.as_str()) => {
                e.prevent_default();
                go(tab, opened, Tab::ALL[c.parse::<usize>().unwrap_or(1) - 1]);
            }
            Key::Escape if shell.trailer.read().is_some() => { let mut t = shell.trailer; t.set(None); }
            // In the theatre, ← and → step through the title's videos.
            Key::ArrowLeft | Key::ArrowRight if shell.trailer.read().is_some() => {
                e.prevent_default();
                let back = e.key() == Key::ArrowLeft;
                let mut t = shell.trailer;
                t.with_mut(|t| if let Some(t) = t { let n = t.clips.len().max(1); t.at = if back { (t.at + n - 1) % n } else { (t.at + 1) % n }; });
            }
            // The rest are YouTube's own keys, so they behave the same with focus inside the video:
            // Space or K play/pause, J and L ten seconds back and on, M mute, F full screen; plus
            // 1–9 to jump to a video, Y to open it on YouTube and ? for the list.
            Key::Character(c) if shell.trailer.read().is_some() && !ctrl && !e.modifiers().alt() => {
                let key = c.to_lowercase();
                let mut t = shell.trailer;
                match key.as_str() {
                    " " | "k" => { e.prevent_default(); theatre::command("toggle"); }
                    "j" => theatre::command("back"),
                    "l" => theatre::command("forward"),
                    "m" => theatre::command("mute"),
                    "f" => theatre::command("full"),
                    "y" => if let Some(id) = t.peek().as_ref().and_then(|t| t.clips.get(t.at)).map(|c| c.youtube.clone()) { open_external(&format!("https://www.youtube.com/watch?v={id}")); },
                    "?" => shortcuts_open.set(!shortcuts_open()),
                    d if d.len() == 1 && ("1"..="9").contains(&d) => {
                        let i = d.parse::<usize>().unwrap_or(1) - 1;
                        t.with_mut(|t| if let Some(t) = t { if i < t.clips.len() { t.at = i; } });
                    }
                    _ => {}
                }
            }
            Key::Escape if search_open() => close_search(shell),
            Key::Escape if opened.read().is_some() => opened.set(None),
            // A TV's Back on another tab goes to Home (it never leaves the app).
            Key::Escape if is_tv() && tab() != Tab::Home => go(tab, opened, Tab::Home),
            _ if search_open() || shell.trailer.read().is_some() || ctrl || e.modifiers().alt() => {}
            Key::Backspace if opened.read().is_some() => opened.set(None),
            Key::Character(c) if c == "?" => shortcuts_open.set(!shortcuts_open()),
            Key::Character(c) if c == "/" => { e.prevent_default(); search_open.set(true); }
            // Title page: P play, L like, D not for me, S save, W watched, T trailer, O sources.
            Key::Character(c) if opened.read().is_some() => title_command.set(c.to_lowercase().chars().next()),
            _ => {}
        }
    };
    // Keys pressed while nothing in the app has focus still reach the app's shortcuts.
    use_hook(|| { document::eval(r#"window.addEventListener('keydown', e => {
        if (e.target === document.body) { const a = document.querySelector('.app'); if (a) { a.focus(); a.dispatchEvent(new KeyboardEvent('keydown', e)); } }
    });"#); });
    // Android TV: the remote moves focus and focus lights the room (tv.js, styles/tv.css).
    use_hook(|| if cfg!(target_os = "android") { document::eval(include_str!("tv.js")); });
    // Scroll shadow, per-tab scroll restoration, title scroll-to-top, pill slider, and mouse glare:
    // combined into one rAF-throttled MutationObserver instead of multiple observers.
    use_hook(|| { document::eval(r#"(() => {
        const savedScroll = Object.create(null);
        const screenOf = p => {
            if (!p) return null;
            for (const c of ['home', 'library', 'settings', 'duel']) {
                if (p.querySelector('.' + c)) return c;
            }
            return null;
        };
        const sync = () => {
            const p = document.querySelector('.page');
            const y = p ? p.scrollTop : 0;
            document.documentElement.classList.toggle('scrolled', y > 4);
            // Smooth GPU hero parallax on TitlePage: drives a single clamped CSS var with zero reflow.
            const tp = p && p.querySelector('.title-page');
            if (tp) tp.style.setProperty('--hero-y', Math.min(320, Math.max(0, y)) + 'px');
            const s = screenOf(p);
            if (s && p) savedScroll[s] = y;
        };
        document.addEventListener('scroll', e => { if (e.target.classList && e.target.classList.contains('page')) sync(); }, true);
        const restoreScroll = () => {
            const p = document.querySelector('.page');
            if (!p) return;
            const title = p.querySelector('.title-page');
            if (title) {
                if (!title.dataset.topDone) { title.dataset.topDone = '1'; p.scrollTop = 0; sync(); }
                return;
            }
            const s = screenOf(p);
            const el = s && p.querySelector('.' + s);
            if (el && !el.dataset.restored) {
                el.dataset.restored = '1';
                if (savedScroll[s]) p.scrollTop = savedScroll[s];
                sync();
            }
        };
        // Selection pills slide: the dock's glass pill and each segmented control's thumb move to the
        // selected item and take its width. Groups whose slider is placed get data-sliding, so the
        // item's own fill steps aside (styles/motion.css); without this script the items keep their fill.
        const slide = () => document.querySelectorAll('.dock, .segmented').forEach(g => {
            const s = g.querySelector('.slider'), on = g.querySelector('.dock-tab.on, .seg.on');
            if (!s) return;
            if (!on) { s.style.opacity = '0'; return; }
            const at = [on.offsetLeft, on.offsetTop, on.offsetWidth, on.offsetHeight];
            if (s.dataset.at === at.join()) return;
            const first = !s.dataset.at; s.dataset.at = at.join();
            if (first) s.style.transition = 'none';
            Object.assign(s.style, {
                transform: `translate3d(${at[0]}px, ${at[1]}px, 0)`,
                width: at[2] + 'px',
                height: at[3] + 'px',
                opacity: '1',
            });
            g.dataset.sliding = '';
            if (first) requestAnimationFrame(() => requestAnimationFrame(() => { s.style.transition = ''; }));
        });
        addEventListener('resize', () => requestAnimationFrame(slide));
        // A new tab slides in from the side you moved toward.
        document.addEventListener('click', e => {
            const t = e.target.closest && e.target.closest('.dock-tab');
            if (!t) return;
            const tabs = [...document.querySelectorAll('.dock-tab')], from = tabs.findIndex(x => x.classList.contains('on'));
            document.documentElement.style.setProperty('--dir', tabs.indexOf(t) < from ? '-1' : '1');
        }, true);
        // Light on glass: mouse-only glare throttled to one rAF per frame with cached rect.
        {
            let cachedArt = null, cachedRect = null, glareRaf = 0, lastX = 0, lastY = 0, pendingArt = null;
            const dropRect = () => { cachedRect = null; };
            addEventListener('scroll', dropRect, true);
            addEventListener('resize', dropRect);
            document.addEventListener('pointermove', e => {
                if (e.pointerType !== 'mouse') return;
                const art = e.target.closest && e.target.closest('.card-art');
                if (!art) return;
                pendingArt = art; lastX = e.clientX; lastY = e.clientY;
                if (glareRaf) return;
                glareRaf = requestAnimationFrame(() => {
                    glareRaf = 0;
                    const a = pendingArt;
                    if (!a || !a.isConnected) return;
                    if (a !== cachedArt || !cachedRect) { cachedArt = a; cachedRect = a.getBoundingClientRect(); }
                    if (!cachedRect.width || !cachedRect.height) return;
                    a.style.setProperty('--mx', ((lastX - cachedRect.left) / cachedRect.width * 100).toFixed(1) + '%');
                    a.style.setProperty('--my', ((lastY - cachedRect.top) / cachedRect.height * 100).toFixed(1) + '%');
                });
            }, { passive: true });
        }
        // Single rAF-throttled observer for scroll shadow, title scroll-reset, and pill sliders.
        let framed = false;
        new MutationObserver(() => {
            if (framed) return;
            framed = true;
            requestAnimationFrame(() => { framed = false; restoreScroll(); sync(); slide(); });
        }).observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ['class'] });
    })();"#); });

    // The ambient light keeps the previous picture under the new one while it fades in, then
    // prunes back to one layer after 1.1s so scrolling never blurs two full-screen images at once.
    let mut lamp_layers = use_signal(Vec::<String>::new);
    use_effect(move || {
        let raw = lamp_source();
        if raw.is_empty() { return; }
        let now = backend::ambient_url(&raw);
        let mut layers = lamp_layers.peek().clone();
        if layers.last() == Some(&now) { return; }
        layers.retain(|s| s != &now);
        layers.push(now.clone());
        if layers.len() > 2 { layers.remove(0); }
        let need_prune = layers.len() > 1;
        lamp_layers.set(layers);
        if need_prune {
            spawn(async move {
                backend::sleep(1100).await;
                if lamp_layers.peek().last() == Some(&now) {
                    lamp_layers.set(vec![now]);
                }
            });
        }
    });

    rsx! {
        style { {fonts} }
        style { {STYLE} }
        div {
            class: format!(
                "app{}{}{}",
                if cfg!(target_os = "android") { " android" } else { "" },
                if cfg!(target_os = "linux") { " linux" } else { "" },
                if (shell.player)().is_some() { " playing" } else { "" }
            ),
            style: "--lamp: {lamp_css}; --top-inset: {insets().0}px; --bottom-inset: {insets().1}px",
            onresize: move |_| if cfg!(target_os = "android") { let now = system_bar_insets(); if *insets.peek() != now { insets.set(now); } },
            tabindex: "-1",
            onkeydown: on_key,
            onmounted: move |e| async move { let _ = e.set_focus(true).await; },
            // The light the glass refracts: the title in front, blurred into the room.
            div { class: "ambient", aria_hidden: "true",
                for src in lamp_layers() { img { key: "{src}", src: "{src}", alt: "" } }
            }
            // Android's status bar sits on solid black on every screen, above every overlay.
            div { class: "status-guard", aria_hidden: "true" }
            TopBar {}
            // `inert` is a boolean attribute: present means on, even as inert="false". Omit it when unused.
            main { class: "page", key: "{opened.read().as_ref().map(|m| m.key()).unwrap_or_default()}", inert: (search_open() || shortcuts_open()).then_some("true"),
                if let Some(media) = opened() { for media in [media] { title::TitlePage { key: "{media.key()}", media } } }
                else { match tab() {
                    Tab::Home => rsx! { home::Home {} },
                    Tab::Settings => rsx! { settings::SettingsPage {} },
                    Tab::Library => rsx! { library::Library {} },
                } }
            }
            if let Some(status) = player_status() { if (shell.player)().is_none() {
                match status {
                    PlayerStatus::Finding(title) => rsx! { player_ui::PendingPlayer { title, step: "Finding a source" } },
                    PlayerStatus::Connecting(title) => rsx! { player_ui::PendingPlayer { title, step: "Connecting to peers" } },
                    PlayerStatus::Opening(title) => rsx! { player_ui::PendingPlayer { title, step: "Opening" } },
                    status => rsx! { StatusChip { status } },
                }
            } }
            if let Some(live) = (shell.player)() { player_ui::PlayerScreen { key: "{std::sync::Arc::as_ptr(&live.0) as usize}", live } }
            if search_open() { search::SearchOverlay {} }
            if shortcuts_open() { shortcuts::AppShortcuts {} }
            if let Some(theatre) = (shell.trailer)() { theatre::TheatreSheet { theatre } }
        }
    }
}

/// Wordmark, a glass dock of destination icons (the selected one shows its
/// name), and the search orb.
#[component]
fn TopBar() -> Element {
    let mut shell = use_context::<Shell>();
    rsx! {
        header { class: "topbar", inert: (shell.shortcuts_open)().then_some("true"),
            if shell.opened.read().is_some() {
                icons::IconButton { label: "Back", class: "glass", onclick: move |_| shell.opened.set(None), icons::ArrowLeft {} }
            } else {
            span { class: "wordmark display", "RStream" }
            nav { class: "dock glass", role: "tablist", aria_label: "Destinations",
                // The pill that slides to the selected tab (positioned in the motion script below).
                i { class: "slider", aria_hidden: "true" }
                for t in Tab::ALL {
                    button {
                        class: if (shell.tab)() == t { "dock-tab on" } else { "dock-tab" },
                        role: "tab",
                        aria_selected: "{(shell.tab)() == t}",
                        aria_label: "{t.title()}",
                        title: "{t.title()}",
                        onclick: move |_| shell.tab.set(t),
                        match t {
                            Tab::Home => rsx! { icons::House {} },
                            Tab::Library => rsx! { icons::Stack {} },
                            Tab::Settings => rsx! { icons::Sliders {} },
                        }
                        if (shell.tab)() == t { span { "{t.title()}" } }
                        if t == Tab::Settings && (shell.addon_updates)() > 0 { i { class: "tab-dot", aria_label: "Add-on updates" } }
                    }
                }
            }
            }
            span { class: "topbar-spacer" }
            LastPlayed {}
            shortcuts::ShortcutButton {}
            icons::IconButton { label: "Search (Ctrl+K)", class: "glass", onclick: move |_| shell.search_open.set(true), icons::Search {} }
        }
    }
}

#[component]
fn StatusChip(status: PlayerStatus) -> Element {
    let mut current = use_context::<Shell>().player_status;
    rsx! {
        div { class: "toast glass", role: "status",
            match status {
                PlayerStatus::Finding(title) => rsx! { span { class: "spinner", aria_hidden: "true" } span { class: "toast-title", "{title}" } span { class: "chip", "Finding a source" } },
                PlayerStatus::Connecting(title) => rsx! { span { class: "spinner", aria_hidden: "true" } span { class: "toast-title", "{title}" } span { class: "chip", "Connecting to peers" } },
                PlayerStatus::Opening(title) => rsx! { span { class: "lamp", icons::Play {} } span { class: "toast-title", "{title}" } },
                PlayerStatus::Playing { title, codec, hardware, facts } => {
                    let lit = hardware.is_some();
                    let decoder = hardware.unwrap_or_else(|| "software".into());
                    let hdr = facts.picture != "SDR" && !facts.picture.is_empty();
                    rsx! {
                    span { class: "lamp", icons::Play {} }
                    span { class: "toast-title", "{title}" }
                    if !facts.resolution.is_empty() { span { class: "chip", "{facts.resolution}" } }
                    if !codec.is_empty() { span { class: "chip", "{codec}" } }
                    if !facts.picture.is_empty() { span { class: if hdr { "chip lit" } else { "chip" }, title: "Picture", "{facts.picture}" } }
                    if !facts.size.is_empty() { span { class: "chip tnum", title: "File size", "{facts.size}" } }
                    span { class: if lit { "chip hw" } else { "chip sw" },
                        title: if lit { "Hardware decoding" } else { "Software decoding" },
                        icons::Cpu {} span { "{decoder}" } }
                }},
                PlayerStatus::Failed(error) => rsx! {
                    span { class: "warn-icon", icons::Alert {} } span { "{error}" }
                    icons::IconButton { label: "Dismiss", class: "small", onclick: move |_| current.set(None), icons::Close {} }
                },
            }
        }
    }
}

/// Tries the next source from where the failed one stopped, told whether it was for the audio language.
pub type TryAnother = Box<dyn Fn(Shell, i64, bool)>;

/// Play through the platform player and narrate what it is doing. `on_next` plays the next episode.
pub fn start_playback(shell: Shell, request: player::PlayRequest, on_next: Option<Box<dyn Fn(Shell)>>) {
    start_playback_with(shell, request, on_next, None)
}

/// `start_playback`, with `on_another` trying the next source when this one failed (told whether
/// it was for the audio language).
pub fn start_playback_with(mut shell: Shell, request: player::PlayRequest, on_next: Option<Box<dyn Fn(Shell)>>, on_another: Option<TryAnother>) {
    let title = request.title.clone();
    let torrent_lease = request.torrent_lease.clone();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    shell.player_status.set(Some(PlayerStatus::Opening(title.clone())));
    player::play(request, tx);
    follow_playback_events(shell, title, torrent_lease, rx, on_next, on_another);
}

fn follow_playback_events(mut shell: Shell, title: String, mut torrent_lease: Option<String>, mut rx: tokio::sync::mpsc::UnboundedReceiver<player::PlayerEvent>,
    on_next: Option<Box<dyn Fn(Shell)>>, on_another: Option<TryAnother>) {
    // Playback belongs to the app, even when its source chooser closes immediately.
    dioxus::core::spawn_forever(async move {
        // This playback's in-app session: only it may take the player off screen.
        let mut mine: Option<engine::LiveRef> = None;
        let mut clear = move |mine: &Option<engine::LiveRef>| if mine.is_some() && *shell.player.peek() == *mine { shell.player.set(None); };
        while let Some(event) = rx.recv().await {
            match event {
                player::PlayerEvent::Started { decoder, codec, facts } =>
                    shell.player_status.set(Some(PlayerStatus::Playing { title: title.clone(), codec, hardware: decoder, facts })),
                player::PlayerEvent::Attached(live) => { mine = Some(live.clone()); shell.player.set(Some(live)); }
                player::PlayerEvent::Failed(error) => {
                    clear(&mine);
                    if let Some(lid) = torrent_lease.take() { backend::stop_torrent_lease_wait(lid).await; }
                    shell.player_status.set(Some(PlayerStatus::Failed(error)));
                    break;
                }
                player::PlayerEvent::Finished { next, another, language, position_ms, .. } => {
                    clear(&mine);
                    if let Some(lid) = torrent_lease.take() { backend::stop_torrent_lease_wait(lid).await; }
                    shell.player_status.set(None);
                    backend::flush_progress().await;
                    *shell.history_dirty.write() += 1;
                    if next { if let Some(play_next) = &on_next { play_next(shell); } }
                    if another { if let Some(try_another) = &on_another { try_another(shell, position_ms, language); } }
                    break;
                }
            }
        }
    });
}

/// Open a link outside Veylo, only ever on an explicit tap (trailers on YouTube).
pub fn open_external(url: &str) {
    if !url.starts_with("https://") { return; }
    #[cfg(target_os = "android")]
    { let _ = android::open_url(url); }
    #[cfg(target_os = "linux")]
    { let _ = std::process::Command::new("xdg-open").arg(url).spawn(); }
    #[cfg(target_os = "macos")]
    { let _ = std::process::Command::new("open").arg(url).spawn(); }
    #[cfg(target_os = "windows")]
    { let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn(); }
}

/// The last thing you played, always top right: a play button ringed by how far you got (its
/// name in the tooltip). One tap carries on at once (the player closes when you leave it).
#[component]
fn LastPlayed() -> Element {
    let shell = use_context::<Shell>();
    let last = use_resource(move || { let _ = (shell.history_dirty)(); async { backend::recent_viewing(4).await.ok().and_then(|l| l.into_iter().next()) } });
    match last() {
        Some(Some(seen)) => rsx! { for m in [seen.media] { LastPlayedButton { key: "{m.key()}", media: m.clone() } } },
        _ => rsx! {},
    }
}

#[component]
fn LastPlayedButton(media: Media) -> Element {
    let shell = use_context::<Shell>();
    // A thin ring around the play button: how far you got. Only a title you are partway
    // through resumes, so only then does the ring show progress.
    const AROUND: f64 = 141.37; // 2π × 22.5
    let resumes = backend::in_progress(&media);
    let done = if resumes { (backend::progress_fraction(&media) * AROUND).clamp(0.0, AROUND) } else { 0.0 };
    // One quiet line under the name: where you are, or what a tap will do.
    let line = if resumes { Some(backend::resume_context(&media)).filter(|c| !c.is_empty() && c != "In progress").unwrap_or_else(|| "Carry on".into()) }
        else if media.kind == "movie" { "Watch again".into() }
        else if media.video_watched { "Next episode".into() }
        else { "Carry on".into() };
    let label = format!("{line}: {}", media.name);
    let m = media.clone();
    let ahead = media.clone();
    use_hook(move || title::prefetch_resume(&ahead));
    rsx! {
        button { class: "last-played glass", title: "{label}", aria_label: "{label}",
            onclick: move |_| title::resume_now(shell, m.clone()),
            span { class: "lp-orb",
                svg { class: "lp-ring", view_box: "0 0 48 48", "aria-hidden": "true",
                    circle { class: "lp-track", cx: "24", cy: "24", r: "22.5" }
                    if done > 0.0 { circle { class: "lp-done", cx: "24", cy: "24", r: "22.5", style: "stroke-dasharray: {done:.1} 200" } }
                }
                icons::Play {}
            }
            span { class: "lp-text",
                span { class: "lp-name", "{media.name}" }
                span { class: "lp-line", "{line}" }
            }
        }
    }
}

#[cfg(test)]
mod playback_lifetime_tests {
    use super::*;
    use std::{cell::{Cell, RefCell}, rc::Rc};

    #[derive(Clone, Default)]
    struct Probe {
        show: Rc<Cell<Option<Signal<bool>>>>,
        status: Rc<Cell<Option<Signal<Option<PlayerStatus>>>>>,
        sender: Rc<RefCell<Option<tokio::sync::mpsc::UnboundedSender<player::PlayerEvent>>>>,
    }

    fn harness(probe: Probe) -> Element {
        let show = use_signal(|| true);
        probe.show.set(Some(show));
        use_context_provider(|| probe.clone());
        use_context_provider(|| {
            let status = Signal::new(None);
            probe.status.set(Some(status));
            Shell {
                tab: Signal::new(Tab::Home), player_status: status,
                search_open: Signal::new(false), search_draft: Signal::new(String::new()), trailer: Signal::new(None), shortcuts_open: Signal::new(false),
                title_command: Signal::new(None), search_query: Signal::new(None), opened: Signal::new(None),
                lamp_source: Signal::new(String::new()),
                history: Signal::new(backend::History::default()), history_dirty: Signal::new(0), addon_updates: Signal::new(0),
                player: Signal::new(None), player_key: Signal::new(None), play_ticket: Signal::new(0),
            }
        });
        rsx! { if show() { TestSourceChooser {} } }
    }

    #[component]
    fn TestSourceChooser() -> Element {
        let shell = use_context::<Shell>();
        let probe = use_context::<Probe>();
        use_hook(move || {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *probe.sender.borrow_mut() = Some(tx);
            follow_playback_events(shell, "Test film".into(), None, rx, None, None);
        });
        rsx! { div {} }
    }

    #[test]
    fn playback_events_survive_closing_the_source_chooser() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let probe = Probe::default();
            let mut dom = VirtualDom::new_with_props(harness, probe.clone());
            dom.rebuild_in_place();
            probe.show.get().unwrap().set(false);
            dom.render_immediate_to_vec();
            // The real event receiver must remain alive after the chooser is unmounted.
            probe.sender.borrow().as_ref().unwrap().send(player::PlayerEvent::Started {
                decoder: None, codec: "HEVC".into(), facts: Default::default(),
            }).expect("closing the chooser must not cancel playback");
            let _ = tokio::time::timeout(std::time::Duration::from_millis(20), dom.wait_for_work()).await;
            assert!(matches!(&*probe.status.get().unwrap().peek(), Some(PlayerStatus::Playing { title, .. }) if title == "Test film"));
        });
    }

    #[test]
    fn async_torrent_launch_survives_closing_the_source_chooser() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            #[derive(Clone, Default)]
            struct TorrentProbe {
                show: Rc<Cell<Option<Signal<bool>>>>,
                status: Rc<Cell<Option<Signal<Option<PlayerStatus>>>>>,
                ready: Rc<RefCell<Option<tokio::sync::oneshot::Sender<String>>>>,
            }

            #[component]
            fn ChooserLaunchingTorrent() -> Element {
                let mut shell = use_context::<Shell>();
                let tp = use_context::<TorrentProbe>();
                use_hook(move || {
                    let ticket = *shell.play_ticket.peek();
                    shell.player_status.set(Some(PlayerStatus::Connecting("Torrent film".into())));
                    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
                    *tp.ready.borrow_mut() = Some(tx);
                    dioxus::core::spawn_forever(async move {
                        if let Ok(url) = rx.await {
                            if still_wanted(shell, ticket) {
                                let mut s = shell;
                                s.player_status.set(Some(PlayerStatus::Opening(url)));
                            }
                        }
                    });
                });
                rsx! { div {} }
            }

            fn torrent_harness(tp: TorrentProbe) -> Element {
                let show = use_signal(|| true);
                tp.show.set(Some(show));
                use_context_provider(|| tp.clone());
                use_context_provider(|| {
                    let status = Signal::new(None);
                    tp.status.set(Some(status));
                    Shell {
                        tab: Signal::new(Tab::Home), player_status: status,
                        search_open: Signal::new(false), search_draft: Signal::new(String::new()), trailer: Signal::new(None), shortcuts_open: Signal::new(false),
                        title_command: Signal::new(None), search_query: Signal::new(None), opened: Signal::new(None),
                        lamp_source: Signal::new(String::new()),
                        history: Signal::new(backend::History::default()), history_dirty: Signal::new(0), addon_updates: Signal::new(0),
                        player: Signal::new(None), player_key: Signal::new(None), play_ticket: Signal::new(0),
                    }
                });
                rsx! { if show() { ChooserLaunchingTorrent {} } }
            }

            let tp = TorrentProbe::default();
            let mut dom = VirtualDom::new_with_props(torrent_harness, tp.clone());
            dom.rebuild_in_place();
            assert!(matches!(&*tp.status.get().unwrap().peek(), Some(PlayerStatus::Connecting(t)) if t == "Torrent film"));
            // Unmount the source chooser immediately, just like `SourceSheet`'s `on_close.call(())`.
            tp.show.get().unwrap().set(false);
            dom.render_immediate_to_vec();
            tp.ready.borrow_mut().take().unwrap().send("http://127.0.0.1:45678/stream/ok".into()).expect("chooser unmount must not drop torrent task");
            let _ = tokio::time::timeout(std::time::Duration::from_millis(20), dom.wait_for_work()).await;
            assert!(matches!(&*tp.status.get().unwrap().peek(), Some(PlayerStatus::Opening(u)) if u.contains("/stream/ok")));
        });
    }
}

#[cfg(all(test, not(target_os = "android")))]
mod theme_tests {
    use super::*;
    use dioxus::desktop::tao::window::Theme;

    #[test]
    fn parses_gtk_and_kde_theme_configs() {
        assert_eq!(parse_gtk_settings_theme("[Settings]\ngtk-theme-name=adw-gtk3-dark\n"), Some(Theme::Dark));
        assert_eq!(parse_gtk_settings_theme("[Settings]\ngtk-theme-name=Adwaita-light\n"), Some(Theme::Light));
        assert_eq!(parse_kdeglobals_theme("[Colors:Window]\nBackgroundNormal=36,40,59\n"), Some(Theme::Dark));
    }
}
