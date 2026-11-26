//! Veylo's one player on every platform: libmpv driven from Rust (engine.rs), its picture under
//! the WebView through each OS's bridge (video.rs) and Dioxus controls (player_ui.rs), using
//! each platform's hardware decoders.
use crate::backend::Media;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use tokio::sync::mpsc::UnboundedSender;

static CURRENT: std::sync::Mutex<Option<PlayerHandle>> = std::sync::Mutex::new(None);

/// Stops the playback it was returned for.
#[derive(Clone, Debug, Default)]
pub struct PlayerHandle(Arc<AtomicBool>);

impl PlayerHandle {
    pub fn stop(&self) { self.0.store(true, Ordering::Relaxed); }
    pub fn stopped(&self) -> bool { self.0.load(Ordering::Relaxed) }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct PlayRequest {
    pub title: String,
    /// Second line in the player: episode or picture format ("4K HDR10+").
    pub subtitle: String,
    /// Offer a Next episode control.
    pub has_next: bool,
    /// Next episode display label for the "Up Next" countdown card (e.g. "S2 E4 · Title").
    pub next_label: Option<String>,
    /// Next episode `(kind, video_id)` to prefetch near the end of playback so Next starts immediately.
    pub next_video: Option<(String, String)>,
    pub url: String,
    pub start_ms: i64,
    /// Request headers the source requires (behaviorHints.proxyHeaders.request).
    pub headers: Vec<(String, String)>,
    /// What the chosen source declares: size as written, HDR label, quality.
    pub size: Option<String>,
    pub hdr: Option<String>,
    pub quality: Option<String>,
    /// Library title whose progress is recorded (and the episode's video id); None for previews and tests.
    pub record: Option<(Media, Option<String>)>,
    /// An automatic choice with others left: when its audio tracks are tagged and none is in the
    /// Audio-language setting's language, the next source is tried instead of subtitles.
    pub language_check: bool,
    /// Active `stremio-torrent` loopback lease; stop this scoped lease when playback finishes or fails.
    pub is_torrent: bool,
    pub torrent_lease: Option<String>,
}

/// What is actually playing: resolution, picture (from the stream where it can tell) and size.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Facts { pub resolution: String, pub picture: String, pub size: String }

/// The resolution class, by width as much as height: a widescreen film is stored cropped, so a
/// 1080p scope film is 1920x800 and a 4K one 3840x1600, short of the class's usual height.
pub fn resolution_label(width: i64, height: i64) -> String { resolution_class(width, height).into() }

/// `resolution_label` as the names the Play order uses ("4K", "1440p", "1080p", "720p", "SD", "").
pub fn resolution_class(width: i64, height: i64) -> &'static str {
    match () { _ if height >= 2000 || width >= 3600 => "4K", _ if height >= 1400 || width >= 2400 => "1440p",
        _ if height >= 1000 || width >= 1800 => "1080p", _ if height >= 700 || width >= 1200 => "720p", _ if height > 0 => "SD", _ => "" }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    /// The in-app player has a session: the shell shows its controls over the picture.
    Attached(crate::engine::LiveRef),
    /// Video is on screen. `decoder` is the hardware API in use, or None for software.
    Started { decoder: Option<String>, codec: String, facts: Facts },
    /// `next`: the viewer asked for the next episode; `another`: this source failed, try the next one
    /// (`language`: because its audio is not in your language).
    Finished { position_ms: i64, duration_ms: i64, next: bool, another: bool, language: bool },
    Failed(String),
}

/// Stop any active in-app playback session (e.g. when hiding the window to the system tray).
#[cfg(not(target_os = "android"))]
pub fn stop_current() {
    if let Ok(mut current) = CURRENT.lock() {
        if let Some(previous) = current.take() {
            previous.stop();
        }
    }
}

/// Start playback; events arrive on `events` until Finished or Failed.
pub fn play(request: PlayRequest, events: UnboundedSender<PlayerEvent>) -> PlayerHandle {
    let handle = PlayerHandle::default();
    // One player at a time: starting a title stops whatever was playing.
    if let Ok(mut current) = CURRENT.lock() {
        if let Some(previous) = current.replace(handle.clone()) { previous.stop(); }
    }
    if let Err(e) = crate::engine::start(request, events.clone(), handle.clone()) {
        let _ = events.send(PlayerEvent::Failed(format!("{e}")));
    }
    handle
}

#[cfg(test)]
mod label_tests {
    use super::*;
    #[test]
    fn resolution_names_the_class_not_the_exact_size() {
        assert_eq!(resolution_label(3840, 1600), "4K");
        assert_eq!(resolution_label(1920, 1080), "1080p");
        assert_eq!(resolution_label(1920, 788), "1080p", "a 2.39:1 film cropped to its picture");
        assert_eq!(resolution_label(1920, 800), "1080p");
        assert_eq!(resolution_label(1280, 536), "720p");
        assert_eq!(resolution_label(3840, 1608), "4K");
        assert_eq!(resolution_label(1280, 720), "720p");
        assert_eq!(resolution_label(720, 404), "SD");
        assert_eq!(resolution_label(0, 0), "");
    }
}
