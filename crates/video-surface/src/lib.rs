//! Where libmpv's picture goes on the desktop: under the app's web view, which turns transparent
//! while a title plays, so the page's own controls float over the video. One path per platform:
//!
//! - Linux: a GtkGLArea below the WebKit view, put in place when the first title plays: a window
//!   holding a GL area is composited through GL, and where GL can't draw (software GL, some VMs) the
//!   whole page would stay black.
//! - Windows: mpv's own child window inside the app window, kept below the WebView2.
//! - macOS: a Core Animation OpenGL layer below the WKWebView, filled by mpv's render API; EDR (HDR)
//!   on screens with headroom.
//!
//! (Android's picture is a SurfaceView owned by the app's own Kotlin activity, so it stays in the app.)
//!
//! The crate knows nothing of the app's UI toolkit: the app hands it its window ([`Host`]) and its
//! player ([`Player`]), and keeps what belongs to the toolkit (the web view's background, full screen).
//!
//! Paths are chosen at [`attach`], before mpv opens the file, and never changed while a title plays:
//! freeing mpv's renderer mid-playback turns its video off until the player is recreated. A path that
//! fails on this machine is remembered and skipped from then on ([`fallback`]).

use std::sync::Arc;

pub mod fallback;
pub use fallback::Path;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as imp;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod none;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use none as imp;

/// The playing session. The crate keeps an `Arc` of it while the picture is up, so mpv's core
/// outlives its renderer (mpv requires that order).
pub trait Player: Send + Sync + 'static {
    fn mpv(&self) -> &libmpv2::Mpv;
}

/// The app window the picture goes under, from the app's UI toolkit.
#[cfg(target_os = "linux")]
pub struct Host {
    /// The top-level window.
    pub window: gtk::Window,
    /// The web view showing the page (the picture goes below it).
    pub web: gtk::Widget,
    /// The box holding the web view, for the GL area path (it puts an overlay in the web view's place).
    pub vbox: Option<gtk::Box>,
}

/// The app window the picture goes under, from the app's UI toolkit.
#[cfg(target_os = "macos")]
pub struct Host {
    /// The `NSWindow*`, valid while the window lives.
    pub ns_window: *mut std::ffi::c_void,
}

/// The app window the picture goes under, from the app's UI toolkit.
#[cfg(target_os = "windows")]
pub struct Host {
    /// The top-level `HWND`.
    pub hwnd: isize,
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub struct Host;

/// Put the player's picture under the window (call on the UI thread, before mpv opens the file).
/// Tries this platform's paths in order, skipping any that failed before on this machine; the one
/// now in use, or None when there is no picture (mpv still plays the sound).
pub fn attach(host: &Host, player: Arc<dyn Player>) -> Option<Path> {
    detach();
    for &path in imp::candidates() {
        if fallback::is_unavailable(path) { continue; }
        if fallback::skipped(path) {
            eprintln!("veylo: picture path {} skipped (VEYLO_DX_SKIP_PICTURE_PATH)", path.name());
            continue;
        }
        match imp::open(path, host, &player) {
            Ok(()) => {
                fallback::set_active(Some(path));
                eprintln!("veylo: picture path: {}", path.name());
                return Some(path);
            }
            Err(e) => {
                fallback::mark_unavailable(path);
                eprintln!("veylo: picture path {} failed: {e}; not trying it again this run", path.name());
            }
        }
    }
    eprintln!("veylo: no picture path worked; playing without a picture");
    None
}

/// Take the picture away (UI thread).
pub fn detach() {
    if let Some(path) = fallback::active() {
        imp::close(path);
        fallback::set_active(None);
    }
}

/// The path showing the picture now.
pub fn active() -> Option<Path> { fallback::active() }

/// The video's format is known or changed (any thread). macOS sends HDR video to an EDR screen as
/// PQ when `hdr_output` (the app's setting) allows it; elsewhere nothing changes here.
pub fn layout(player: &dyn Player, hdr_output: bool) { imp::layout(player, hdr_output) }

#[cfg(target_os = "macos")]
pub use macos::{check, hdr_check, potential_headroom, sent_transfer};
