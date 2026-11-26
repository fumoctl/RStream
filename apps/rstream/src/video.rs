//! Where the picture goes: under the app's WebView, which turns transparent while a title plays, so
//! the Dioxus controls (player_ui.rs) float over the video on every platform.
//! Desktop: crates/video-surface puts it there (a GtkGLArea on Linux, mpv's child window on
//!   Windows, a Core Animation OpenGL layer with EDR on macOS); this file hands it
//!   the window and keeps what belongs to Dioxus (the page's background, full screen).
//! Android: a SurfaceView below the WebView (RStreamVideo.kt), handed to mpv as its window.
use crate::engine::Live;
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};

static ATTACHED_SESSION: AtomicUsize = AtomicUsize::new(0);

fn claim_session(slot: &AtomicUsize, session: usize) {
    slot.store(session, Ordering::SeqCst);
}

fn release_session(slot: &AtomicUsize, session: usize) -> bool {
    slot.compare_exchange(session, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Keep both window and WebView opaque while browsing on desktop.
#[cfg(not(target_os = "android"))]
pub fn init_opaque() { imp::init_opaque() }
/// Put the session's picture on screen (call on the UI thread).
pub fn attach(live: &Arc<Live>, session: usize) {
    claim_session(&ATTACHED_SESSION, session);
    imp::attach(live);
}
/// Take the picture away and give the app its opaque background back, unless a newer session took over.
pub fn detach(session: usize) {
    if release_session(&ATTACHED_SESSION, session) {
        imp::detach();
    }
}
/// The picture's aspect or fit changed (any thread).
pub fn layout(live: &Live) { imp::layout(live) }
/// Hint the content frame rate (e.g. 23.976 Hz) to the display surface where supported (Android 11+).
pub fn set_frame_rate(fps: f64) { imp::set_frame_rate(fps) }
/// Desktop window fullscreen; Android is always full screen while playing.
pub fn set_fullscreen(on: bool) { imp::set_fullscreen(on) }
/// Windows: the app window, for the screen's HDR state (display.rs).
#[cfg(target_os = "windows")]
pub fn hwnd() -> Option<isize> {
    use dioxus::desktop::tao::platform::windows::WindowExtWindows;
    Some(dioxus::desktop::window().window.hwnd() as isize)
}
/// macOS: the most EDR headroom the window's screen can reach (1.0: SDR only; 0: not read yet).
#[cfg(target_os = "macos")]
pub fn potential_headroom() -> f64 { veylo_video_surface::potential_headroom(|| Some(imp::host())) }
/// macOS: the self-tests of the picture's pipeline, and the transfer it goes out with ("pq" in HDR).
#[cfg(target_os = "macos")]
pub use veylo_video_surface::{check, hdr_check, sent_transfer};

#[cfg(not(target_os = "android"))]
impl veylo_video_surface::Player for Live {
    fn mpv(&self) -> &libmpv2::Mpv { &self.mpv }
}

#[cfg(target_os = "android")]
mod imp {
    use super::*;
    use crate::engine::Fit;
    pub fn attach(live: &Arc<Live>) {
        let _ = crate::android::video_show();
        let _ = crate::android::video_media_meta(&live.request.title, &live.request.subtitle);
        crate::android::attach_surface(live);
    }
    pub fn detach() { let _ = crate::android::video_hide(); }
    pub fn layout(live: &Live) {
        let s = live.state();
        let fit = match s.fit { Fit::Fit => 0, Fit::Fill => 1, Fit::Stretch => 2 };
        let _ = crate::android::video_layout(s.aspect, fit);
    }
    pub fn set_frame_rate(fps: f64) { let _ = crate::android::video_frame_rate(fps); }
    pub fn set_fullscreen(_on: bool) {}
}

#[cfg(not(target_os = "android"))]
mod imp {
    use super::*;
    use veylo_video_surface::Host;

    /// The window the picture goes under, from Dioxus (UI thread).
    pub fn host() -> Host {
        let ctx = dioxus::desktop::window();
        #[cfg(target_os = "linux")]
        {
            use dioxus::desktop::{tao::platform::unix::WindowExtUnix, wry::WebViewExtUnix};
            use gtk::prelude::*;
            let web: gtk::Widget = ctx.webview.webview().upcast();
            Host { window: ctx.window.gtk_window().clone().upcast(), web, vbox: ctx.window.default_vbox().cloned() }
        }
        #[cfg(target_os = "macos")]
        {
            use dioxus::desktop::tao::platform::macos::WindowExtMacOS;
            Host { ns_window: ctx.window.ns_window() }
        }
        #[cfg(target_os = "windows")]
        {
            use dioxus::desktop::tao::platform::windows::WindowExtWindows;
            Host { hwnd: ctx.window.hwnd() as isize }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        { let _ = ctx; Host }
    }

    /// The page turns see-through over the picture where the picture is a separate layer under the
    /// WebView's own background (macOS: the window is transparent and the page paints its own black).
    const CLEAR_PAGE: bool = cfg!(any(target_os = "linux", target_os = "windows"));

    fn set_surface_bg(rgba: (u8, u8, u8, u8)) {
        if !CLEAR_PAGE { return; }
        let w = dioxus::desktop::window();
        #[cfg(target_os = "linux")]
        {
            let _ = rgba;
            // Keep WebKitWebView's ARGB8888 surface mode constant over the opaque `#000000`
            // GTK vbox/overlay and GtkGLArea: HTML/CSS paints `#000` while browsing and turns
            // `transparent` while `.app.playing` is mounted.
            let _ = w.webview.set_background_color((0, 0, 0, 0));
            use dioxus::desktop::wry::WebViewExtUnix;
            use gtk::prelude::*;
            let webview = w.webview.webview();
            webview.set_app_paintable(true);
            webview.queue_draw();
        }
        #[cfg(not(target_os = "linux"))]
        {
            w.window.set_background_color(Some(rgba));
            let _ = w.webview.set_background_color(rgba);
        }
    }

    #[cfg(target_os = "linux")]
    fn disable_webkit_partial_damage<W: gtk::glib::object::IsA<gtk::Widget>>(webview: &W) {
        use gtk::glib::translate::ToGlibPtr;
        use std::ffi::{c_char, c_int, c_void, CStr};
        // WebKitGTK 2.52–2.54's DMA-BUF `SwapChainDamageTracker` clears transparent GBM FBOs to
        // `(0,0,0,0)` and paints only the frame's partial damage rect (e.g. `.scrubber`), erasing
        // the rest of the HUD when `GtkGLArea` draws underneath. Turning off partial-damage
        // compositing/propagation keeps full 120fps GPU DMA-BUF rendering while compositing the
        // complete UI layer tree into every swapchain buffer.
        unsafe {
            let sym = |name: &CStr| libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
            let (p_get_settings, p_dev_features, p_exp_features, p_all_features, p_len, p_get, p_ident, p_set, p_unref) = (
                sym(c"webkit_web_view_get_settings"),
                sym(c"webkit_settings_get_development_features"),
                sym(c"webkit_settings_get_experimental_features"),
                sym(c"webkit_settings_get_all_features"),
                sym(c"webkit_feature_list_get_length"),
                sym(c"webkit_feature_list_get"),
                sym(c"webkit_feature_get_identifier"),
                sym(c"webkit_settings_set_feature_enabled"),
                sym(c"webkit_feature_list_unref"),
            );
            if [p_get_settings, p_len, p_get, p_ident, p_set, p_unref]
                .iter()
                .any(|p| p.is_null())
            {
                return;
            }
            let get_settings: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(p_get_settings);
            let list_len: unsafe extern "C" fn(*mut c_void) -> usize = std::mem::transmute(p_len);
            let list_get: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void = std::mem::transmute(p_get);
            let feat_ident: unsafe extern "C" fn(*mut c_void) -> *const c_char = std::mem::transmute(p_ident);
            let set_enabled: unsafe extern "C" fn(*mut c_void, *mut c_void, c_int) = std::mem::transmute(p_set);
            let list_unref: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(p_unref);

            let widget_ptr: *mut gtk::ffi::GtkWidget = webview.as_ref().to_glib_none().0;
            let settings = get_settings(widget_ptr.cast());
            if settings.is_null() {
                return;
            }
            for getter_ptr in [p_dev_features, p_exp_features, p_all_features] {
                if getter_ptr.is_null() {
                    continue;
                }
                let get_list: unsafe extern "C" fn() -> *mut c_void = std::mem::transmute(getter_ptr);
                let list = get_list();
                if list.is_null() {
                    continue;
                }
                let n = list_len(list);
                for i in 0..n {
                    let feat = list_get(list, i);
                    if feat.is_null() {
                        continue;
                    }
                    let id_ptr = feat_ident(feat);
                    if id_ptr.is_null() {
                        continue;
                    }
                    if let Ok(id) = CStr::from_ptr(id_ptr).to_str() {
                        if matches!(
                            id,
                            "PropagateDamagingInformation"
                                | "UseDamagingInformationForCompositing"
                                | "UnifyDamagedRegions"
                        ) {
                            set_enabled(settings, feat, 0);
                        }
                    }
                }
                list_unref(list);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn queue_linux_redraws(delays: &[u64]) {
        use dioxus::desktop::wry::WebViewExtUnix;
        use gtk::prelude::*;
        let webview = dioxus::desktop::window().webview.webview();
        for &ms in delays {
            let w = webview.clone();
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
                if let Some(p) = w.parent() {
                    p.queue_draw();
                } else {
                    w.queue_draw();
                }
            });
        }
    }

    pub fn init_opaque() {
        set_surface_bg((0, 0, 0, 255));
        #[cfg(target_os = "linux")]
        {
            use dioxus::desktop::tao::platform::unix::WindowExtUnix;
            use dioxus::desktop::wry::WebViewExtUnix;
            use gtk::prelude::*;
            static BLACK_BG_CSS: std::sync::Once = std::sync::Once::new();
            BLACK_BG_CSS.call_once(|| {
                let provider = gtk::CssProvider::new();
                let _ = provider.load_from_data(b".rstream-black-bg { background-color: #000000; background-image: none; }");
                if let Some(screen) = gtk::gdk::Screen::default() {
                    gtk::StyleContext::add_provider_for_screen(&screen, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
                }
            });
            let win = dioxus::desktop::window();
            if let Some(vbox) = win.window.default_vbox() {
                vbox.style_context().add_class("rstream-black-bg");
            }
            let prefer_dark = crate::preferred_desktop_theme() != Some(dioxus::desktop::tao::window::Theme::Light);
            if let Some(s) = gtk::Settings::default() {
                s.set_gtk_application_prefer_dark_theme(prefer_dark);
            }
            let webview = win.webview.webview();
            disable_webkit_partial_damage(&webview);
            queue_linux_redraws(&[150, 350, 700]);
        }
    }

    pub fn attach(live: &Arc<Live>) {
        let path = veylo_video_surface::attach(&host(), live.clone());
        if path.is_some() { set_surface_bg((0, 0, 0, 0)); }
        #[cfg(target_os = "linux")]
        queue_linux_redraws(&[150, 450]);
        // Without a picture mpv still plays the sound.
        live.mark_ready();
    }
    pub fn detach() {
        veylo_video_surface::detach();
        set_surface_bg((0, 0, 0, 255));
        #[cfg(target_os = "linux")]
        queue_linux_redraws(&[100, 350]);
        set_fullscreen(false);
    }
    pub fn layout(live: &Live) { veylo_video_surface::layout(live, crate::backend::hdr_output()); }
    pub fn set_frame_rate(_fps: f64) {}
    pub fn set_fullscreen(on: bool) {
        let w = dioxus::desktop::window();
        w.window.set_fullscreen(on.then_some(dioxus::desktop::tao::window::Fullscreen::Borderless(None)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_session_drop_does_not_detach_newer_mounted_session() {
        let slot = AtomicUsize::new(0);
        claim_session(&slot, 101);
        // Dioxus 0.7 mounts new keyed PlayerScreen (202) before dropping old PlayerScreen (101).
        claim_session(&slot, 202);
        assert!(!release_session(&slot, 101));
        assert_eq!(slot.load(Ordering::SeqCst), 202);
        assert!(release_session(&slot, 202));
        assert_eq!(slot.load(Ordering::SeqCst), 0);
    }
}
