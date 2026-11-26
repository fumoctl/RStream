//! Veylo's one playback engine: libmpv, driven from Rust, on every platform. The picture is
//! drawn under the app's WebView (video.rs) and the controls are Dioxus (player_ui.rs), so the
//! player looks and behaves the same everywhere; only the way frames reach the screen differs:
//!
//! - Linux/desktop: mpv's render API into a GL area under a transparent WebView. Decoding uses
//!   the GPU (`hwdec=auto-safe`: VAAPI, NVDEC, Vulkan, VideoToolbox, D3D11VA), and HDR is
//!   tone-mapped by mpv with per-scene peak detection (GTK3 can't hand HDR to the compositor).
//! - Windows: `vo=gpu-next` on Direct3D 11 in mpv's own child window under the WebView2,
//!   with the GPU decoding (`hwdec=auto-safe`: D3D11VA, NVDEC). With Windows HDR on, mpv asks for
//!   an HDR10 swapchain (`target-colorspace-hint`) and HDR reaches the screen; Dolby Vision
//!   profile 5 is reshaped by libplacebo and shown as HDR10. Otherwise it is tone-mapped to SDR.
//! - Android, direct (default): `vo=mediacodec_embed`. MediaCodec decodes straight into a
//!   SurfaceView, so HDR10, HLG and HDR10+ (its dynamic metadata stays in the bitstream) reach an
//!   HDR panel untouched, at the lowest power. Subtitles are drawn by the Dioxus layer.
//! - Android, rendered: `vo=gpu-next`. mpv draws the picture (ASS and image subtitles, tone
//!   mapping); chosen by the viewer, or automatically for image subtitles.
//!
//! URLs and headers are never logged.
use crate::backend;
use crate::player::{Facts, PlayRequest, PlayerEvent, PlayerHandle};
use libmpv2::{events::{Event, PropertyData}, mpv_end_file_reason, Format, Mpv};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc::UnboundedSender, watch};

/// How the picture fills the screen.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
#[allow(clippy::enum_variant_names)] // "Fit" is what the menu says
pub enum Fit { #[default] Fit, Fill, Stretch }

impl Fit {
    pub const ALL: [Fit; 3] = [Fit::Fit, Fit::Fill, Fit::Stretch];
    pub fn name(self) -> &'static str { match self { Fit::Fit => "Fit", Fit::Fill => "Fill", Fit::Stretch => "Stretch" } }
    pub fn about(self) -> &'static str {
        match self { Fit::Fit => "The whole picture", Fit::Fill => "Fill the screen, crop the edges", Fit::Stretch => "Fill the screen, stretch the picture" }
    }
}

/// One audio or subtitle track as the viewer sees it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Track { pub id: i64, pub audio: bool, pub label: String, pub selected: bool, pub image: bool, pub styled: bool,
    /// The track's language tag as the file has it ("ita", "en"), empty when untagged.
    pub lang: String,
    /// Forced subtitles: signs and foreign lines only, not the dialogue.
    pub forced: bool }

/// A part of the video the viewer may want to pass over, from the file's chapter names.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SegmentKind { Intro, Recap, Credits }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment { pub kind: SegmentKind, pub start: f64, pub end: f64 }

/// A named chapter in the file (from mpv's `chapter-list`), for seekbar ticks and the chapter picker.
#[derive(Clone, Debug, PartialEq)]
pub struct Chapter { pub title: String, pub start: f64 }

/// What the screen can show, as the platform declares it (Android's display, Windows' DXGI
/// output the window is on); Linux output is SDR. `peak_nits`: the panel's peak, 0 if unknown.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Screen { pub hdr10: bool, pub hlg: bool, pub hdr10_plus: bool, pub dolby_vision: bool, pub peak_nits: u32 }

/// Everything the controls draw. Positions are quantised so the UI repaints a few times a second.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct State {
    pub loaded: bool,
    pub position: f64,
    pub duration: f64,
    /// Seconds buffered ahead of `position`.
    pub buffered: f64,
    pub paused: bool,
    pub buffering: bool,
    pub ended: bool,
    pub hwdec: String,
    pub codec: String,
    pub width: i64,
    pub height: i64,
    /// Display aspect (width / height), 0 until known.
    pub aspect: f64,
    pub transfer: String,
    pub target_transfer: String,
    pub tracks: Vec<Track>,
    pub speed: f64,
    pub volume: f64,
    pub muted: bool,
    /// The current text subtitle, for the Dioxus layer in direct mode.
    pub subtitle: String,
    pub error: Option<String>,
    /// Android: MediaCodec writes straight to the display.
    pub direct: bool,
    pub fit: Fit,
    /// Dolby Vision profile of the video track (0: none). Profile 5 has no HDR10 layer.
    pub dv_profile: i64,
    /// Frames the player had to drop (decoder or display too slow).
    pub dropped: i64,
    /// The playing file's size in bytes as mpv reads it (0 until known).
    pub file_size: i64,
    /// Intro, recap and credits, where the file's chapters name them.
    pub segments: Vec<Segment>,
    /// The same from the online databases (IntroDB, AniSkip), for what the chapters don't name.
    pub online_segments: Vec<Segment>,
    /// All chapters from the container, sorted by start time.
    pub chapters: Vec<Chapter>,
    /// Subtitle timing offset in seconds (positive delays subtitles).
    pub sub_delay: f64,
    /// Audio timing offset in seconds (positive delays audio).
    pub audio_delay: f64,
    /// Subtitle scale (0.6..=2.0).
    pub sub_scale: f64,
    /// Subtitle vertical position percent (70..=100).
    pub sub_pos: i32,
    /// Subtle dark pill behind subtitles.
    pub sub_bg: bool,
    /// Container frame rate (0 until known), used for Android TV/tablet refresh-rate matching.
    pub fps: f64,
    /// Active GLSL shader paths currently loaded in mpv (`glsl-shaders`).
    pub active_shaders: Vec<String>,
}

impl State {
    /// What can be skipped: the file's own chapters first, the databases for the kinds they miss.
    pub fn skips(&self) -> Vec<Segment> {
        let mut all = self.segments.clone();
        all.extend(self.online_segments.iter().filter(|g| !self.segments.iter().any(|c| c.kind == g.kind)));
        all
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Finish { Close, Next, Another, OtherLanguage }

/// A playing session, shared by the engine thread, the video surface and the controls.
pub struct Live {
    pub mpv: Mpv,
    pub request: PlayRequest,
    /// The screen when this session started; None where it couldn't be read (Linux).
    pub screen: Option<Screen>,
    state: watch::Sender<State>,
    finish: Mutex<Option<Finish>>,
    ready: (Mutex<bool>, Condvar),
}

/// A session handle for Dioxus props and signals (equal when it is the same session).
#[derive(Clone)]
pub struct LiveRef(pub Arc<Live>);
impl PartialEq for LiveRef { fn eq(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) } }
impl std::fmt::Debug for LiveRef { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "Live({})", self.0.request.title) } }

/// The session that owns the screen, for the platform surface callbacks.
static ACTIVE: Mutex<Option<Arc<Live>>> = Mutex::new(None);

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn active() -> Option<Arc<Live>> { ACTIVE.lock().ok().and_then(|a| a.clone()) }

impl Live {
    pub fn subscribe(&self) -> watch::Receiver<State> { self.state.subscribe() }
    pub fn state(&self) -> State { self.state.borrow().clone() }
    fn update(&self, f: impl FnOnce(&mut State) -> bool) { self.state.send_if_modified(f); }

    /// The video surface is in place: mpv may open the file.
    pub fn mark_ready(&self) {
        let (lock, cv) = &self.ready;
        if let Ok(mut r) = lock.lock() { *r = true; cv.notify_all(); }
    }
    fn wait_ready(&self, timeout: Duration, stop: &PlayerHandle) -> bool {
        let (lock, cv) = &self.ready;
        let deadline = Instant::now() + timeout;
        let Ok(mut guard) = lock.lock() else { return false };
        while !*guard {
            if stop.stopped() || self.finish.lock().is_ok_and(|f| f.is_some()) { return false; }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { return false };
            let step = remaining.min(Duration::from_millis(200));
            let Ok((g, _)) = cv.wait_timeout(guard, step) else { return false };
            guard = g;
        }
        true
    }

    // ------------------------------------------------------------ commands from the controls
    pub fn toggle_pause(&self) {
        let s = self.state();
        if s.ended { let _ = self.mpv.command("seek", &["0", "absolute"]); }
        let _ = self.mpv.set_property("pause", !s.paused);
    }
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn set_paused(&self, paused: bool) { let _ = self.mpv.set_property("pause", paused); }
    pub fn seek_by(&self, seconds: f64) { let _ = self.mpv.command("seek", &[&format!("{seconds}"), "relative+exact"]); }
    pub fn seek_to(&self, seconds: f64) { let _ = self.mpv.command("seek", &[&format!("{:.3}", seconds.max(0.0)), "absolute"]); }
    pub fn set_speed(&self, speed: f64) { let _ = self.mpv.set_property("speed", speed); }
    pub fn set_volume(&self, volume: f64) { let _ = self.mpv.set_property("volume", volume.clamp(0.0, 130.0)); let _ = self.mpv.set_property("mute", false); }
    pub fn toggle_mute(&self) { let _ = self.mpv.set_property("mute", !self.state().muted); }
    pub fn set_sub_delay(&self, seconds: f64) {
        let v = (seconds * 100.0).round() / 100.0;
        let _ = self.mpv.set_property("sub-delay", v);
        self.update(|s| { let c = (s.sub_delay - v).abs() > 0.001; s.sub_delay = v; c });
    }
    pub fn set_audio_delay(&self, seconds: f64) {
        let v = (seconds * 100.0).round() / 100.0;
        let _ = self.mpv.set_property("audio-delay", v);
        self.update(|s| { let c = (s.audio_delay - v).abs() > 0.001; s.audio_delay = v; c });
    }
    pub fn set_sub_scale(&self, scale: f64) {
        let v = ((scale * 20.0).round() / 20.0).clamp(0.6, 2.0);
        let _ = self.mpv.set_property("sub-scale", v);
        self.update(|s| { let c = (s.sub_scale - v).abs() > 0.001; s.sub_scale = v; c });
        backend::update_settings(|cfg| cfg.sub_scale = v);
    }
    pub fn set_sub_pos(&self, pos: i32) {
        let v = pos.clamp(70, 100);
        let _ = self.mpv.set_property("sub-pos", v as i64);
        self.update(|s| { let c = s.sub_pos != v; s.sub_pos = v; c });
        backend::update_settings(|cfg| cfg.sub_pos = v);
    }
    pub fn set_sub_bg(&self, on: bool) {
        let _ = self.mpv.set_property("sub-back-color", if on { "#A8000000" } else { "#00000000" });
        self.update(|s| { let c = s.sub_bg != on; s.sub_bg = on; c });
        backend::update_settings(|cfg| cfg.sub_bg = on);
    }
    pub fn select(&self, audio: bool, id: Option<i64>) {
        let value = id.map(|i| i.to_string()).unwrap_or_else(|| "no".into());
        let _ = self.mpv.set_property(if audio { "aid" } else { "sid" }, value.as_str());
        if !audio { self.fit_subtitles(id); }
    }
    /// Add a subtitle file (a local path or a link) as a track and turn it on; returns its track id.
    /// Blocks while mpv reads the file, so call it off the UI thread.
    pub fn add_subtitle(&self, file: &str, title: &str, lang: &str) -> anyhow::Result<i64> {
        self.mpv.command("sub-add", &[file, "select", title, lang]).map_err(|e| anyhow::anyhow!("{e}"))?;
        refresh_tracks(self);
        let id = self.mpv.get_property::<i64>("sid").map_err(|e| anyhow::anyhow!("{e}"))?;
        self.fit_subtitles(Some(id));
        Ok(id)
    }
    /// The next audio track, or the next subtitle track then off; returns what is on now.
    pub fn cycle_track(&self, audio: bool) -> String {
        let s = self.state();
        let list: Vec<&Track> = s.tracks.iter().filter(|t| t.audio == audio).collect();
        let at = list.iter().position(|t| t.selected);
        // Subtitles cycle through "off" after the last track; audio wraps around.
        let next = match at { Some(i) if i + 1 < list.len() => Some(list[i + 1]), Some(_) if !audio => None, _ => list.first().copied() };
        self.select(audio, next.map(|t| t.id));
        next.map(|t| t.label.clone()).unwrap_or_else(|| "Off".into())
    }
    pub fn seek_percent(&self, percent: f64) {
        let d = self.state().duration;
        if d > 0.0 { self.seek_to(d * percent / 100.0); }
    }
    /// Subtitles mpv must draw itself leave the direct path: image subtitles (PGS, DVD) always,
    /// styled ASS when the picture is SDR (on HDR the HDR wins, and the text is drawn plainly).
    fn fit_subtitles(&self, id: Option<i64>) {
        let s = self.state();
        if !s.direct { return; }
        let hdr = matches!(s.transfer.as_str(), "pq" | "hlg") || s.dv_profile > 0;
        if s.tracks.iter().any(|t| !t.audio && Some(t.id) == id && (t.image || (t.styled && !hdr))) { self.set_direct(false); }
    }
    pub fn set_fit(&self, fit: Fit) {
        self.update(|s| { let changed = s.fit != fit; s.fit = fit; changed });
        crate::video::layout(self);
        if cfg!(not(target_os = "android")) {
            let _ = self.mpv.set_property("keepaspect", fit != Fit::Stretch);
            let _ = self.mpv.set_property("panscan", if fit == Fit::Fill { 1.0 } else { 0.0 });
        }
    }
    /// Android: switch between MediaCodec straight to the display and mpv's renderer.
    pub fn set_direct(&self, direct: bool) {
        if !cfg!(target_os = "android") || self.state().direct == direct { return; }
        self.update(|s| { s.direct = direct; true });
        let _ = self.mpv.set_property("hwdec", if direct { "mediacodec" } else { "mediacodec-copy" });
        let _ = self.mpv.set_property("vo", if direct { "mediacodec_embed" } else { "gpu-next" });
    }
    /// Dolby Vision profile 5 without a Dolby Vision panel: the RPU that rebuilds its colours only
    /// survives FFmpeg's own decoder, so mpv decodes on the CPU and reshapes on the GPU.
    pub fn render_dolby_vision(&self) {
        self.update(|s| { s.direct = false; true });
        let _ = self.mpv.set_property("hwdec", "no");
        let _ = self.mpv.set_property("vo", if cfg!(any(target_os = "android", target_os = "windows")) { "gpu-next" } else { "libmpv" });
    }
    pub fn set_online_segments(&self, found: Vec<Segment>) {
        self.update(|s| { let c = s.online_segments != found; s.online_segments = found; c });
    }
    /// Reload the source where it stopped (after a network error).
    pub fn retry(&self) {
        let at = self.state().position;
        self.update(|s| { s.error = None; true });
        let _ = self.mpv.set_property("start", format!("+{at}").as_str());
        let _ = self.mpv.command("loadfile", &[self.request.url.as_str(), "replace"]);
    }
    pub fn finish(&self, how: Finish) {
        if let Ok(mut f) = self.finish.lock() { if f.is_none() { *f = Some(how); } }
        self.ready.1.notify_all();
        let _ = self.mpv.command("quit", &[]);
    }

    /// Execute a raw `input.conf` command string via `mpv_command_string`.
    pub fn run_mpv_command(&self, command: &str) -> anyhow::Result<()> {
        let cmd = command.trim();
        anyhow::ensure!(!cmd.is_empty(), "empty mpv command");
        let c_cmd = std::ffi::CString::new(cmd).map_err(|_| anyhow::anyhow!("mpv command contains NUL byte"))?;
        let rc = unsafe { libmpv2_sys::mpv_command_string(self.mpv.ctx.as_ptr(), c_cmd.as_ptr()) };
        if rc < 0 {
            anyhow::bail!("mpv command failed (error {rc})");
        }
        self.sync_shaders_from_mpv();
        Ok(())
    }

    /// Toggle a GLSL shader file on/off; forces `direct = false` (`vo=gpu-next`) on Android when shaders become active.
    pub fn toggle_shader(&self, path: &std::path::Path) -> anyhow::Result<bool> {
        let target = path.to_string_lossy().into_owned();
        let mut shaders = self.state().active_shaders;
        let enabled = if let Some(pos) = shaders.iter().position(|p| p == &target) {
            shaders.remove(pos);
            false
        } else {
            shaders.push(target);
            true
        };
        if !shaders.is_empty() {
            self.set_direct(false);
        }
        let sep = if cfg!(windows) { ";" } else { ":" };
        let joined = shaders.join(sep);
        self.mpv.set_property("glsl-shaders", joined.as_str()).map_err(|e| anyhow::anyhow!("glsl-shaders: {e}"))?;
        self.update(|s| { s.active_shaders = shaders; true });
        Ok(enabled)
    }

    fn sync_shaders_from_mpv(&self) {
        let raw = self.mpv.get_property::<String>("glsl-shaders").unwrap_or_default();
        let shaders = split_shader_list(&raw);
        if !shaders.is_empty() {
            self.set_direct(false);
        }
        self.update(|s| {
            let changed = s.active_shaders != shaders;
            s.active_shaders = shaders;
            changed
        });
    }
}

fn split_shader_list(raw: &str) -> Vec<String> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    raw.split(sep).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

pub(crate) fn ensure_c_numeric_locale() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        libc::setlocale(libc::LC_NUMERIC, c"C".as_ptr());
    });
}

/// Start a session: mpv is created here; the file opens once the video surface is ready.
pub fn start(req: PlayRequest, events: UnboundedSender<PlayerEvent>, stop: PlayerHandle) -> anyhow::Result<()> {
    let screen = crate::display::screen();
    let cfg = backend::load_settings();
    let parsed = rstream_mpv_config::MpvConfigDir::open_or_seed(backend::mpv_config_dir())
        .and_then(|dir| dir.parse_mpv_conf())
        .unwrap_or_default();
    let direct = cfg!(target_os = "android") && !parsed.has_shaders;
    let mpv = create(&req, direct, &cfg, &parsed)?;
    let active_shaders = split_shader_list(&mpv.get_property::<String>("glsl-shaders").unwrap_or_default());
    let direct = direct && active_shaders.is_empty();
    let (state, _) = watch::channel(State {
        speed: 1.0, volume: 100.0, direct,
        sub_scale: cfg.sub_scale, sub_pos: cfg.sub_pos, sub_bg: cfg.sub_bg,
        active_shaders,
        ..Default::default()
    });
    let live = Arc::new(Live { mpv, request: req, screen, state, finish: Mutex::new(None), ready: (Mutex::new(false), Condvar::new()) });
    if let Ok(mut a) = ACTIVE.lock() { *a = Some(live.clone()); }
    let _ = events.send(PlayerEvent::Attached(LiveRef(live.clone())));
    std::thread::Builder::new().name("rstream-engine".into()).spawn(move || run(live, events, stop))?;
    Ok(())
}


fn create(req: &PlayRequest, direct: bool, cfg: &backend::Settings, parsed: &rstream_mpv_config::ParsedMpvConf) -> anyhow::Result<Mpv> {
    #[cfg(target_os = "windows")]
    let hdr_output = backend::hdr_output();
    // libmpv refuses to start unless numbers parse the C way; GTK/WebKit set the user's locale.
    ensure_c_numeric_locale();
    #[cfg(target_os = "android")]
    let ca = crate::android::prepare_mpv()?;
    let _ = direct;
    Mpv::with_initializer(|o| {
        // Nothing personal is read: the same behaviour on every machine.
        o.set_property("config", false)?;
        o.set_property("terminal", false)?;
        o.set_property("idle", true)?;
        o.set_property("keep-open", true)?;
        o.set_property("osc", false)?;
        o.set_property("input-default-bindings", false)?;
        o.set_property("input-vo-keyboard", false)?;
        o.set_property("ytdl", false)?;
        o.set_property("hr-seek", "yes")?;
        o.set_property("cache", "yes")?;
        o.set_property("demuxer-max-back-bytes", "64MiB")?;
        // Smooth lookahead and resilient HTTP streaming over Wi-Fi / CDN resets.
        // Loopback BitTorrent HTTP streams wait on initial pieces + MKV EOF cues from peers.
        let (readahead_secs, net_timeout) = if req.is_torrent { (20i64, 60.0f64) } else { (60i64, 20.0f64) };
        let _ = o.set_property("demuxer-readahead-secs", readahead_secs);
        let _ = o.set_property("network-timeout", net_timeout);
        let _ = o.set_property("stream-lavf-o", "reconnect=1,reconnect_streamed=1,reconnect_on_network_error=1,reconnect_on_http_error=5xx,reconnect_delay_max=5");
        // HDR on an SDR target: per-scene peak detection (what HDR10+ metadata is for) and the default curve.
        o.set_property("hdr-compute-peak", "yes")?;
        // Your language's audio first, when the file has it (a MULTI or dual-audio release);
        // mpv falls back to the file's default track otherwise.
        if let Some(key) = backend::stream_language() {
            let codes = backend::track_codes(&key);
            if !codes.is_empty() { o.set_property("alang", codes.join(",").as_str())?; }
        }
        // Subtitles are the app's decision once the tracks are known (player_ui's fallback):
        // off when your language is in the audio (or "Any language"), else the file's full
        // subtitles in your language, or a download. Starting with none avoids a flash of a
        // container-default subtitle before that runs.
        o.set_property("sid", "no")?;
        o.set_property("sub-font", "sans-serif")?;
        o.set_property("sub-color", "#F2EFEA")?;
        o.set_property("sub-border-color", "#DD000000")?;
        o.set_property("sub-scale", cfg.sub_scale)?;
        let _ = o.set_property("sub-pos", cfg.sub_pos as i64);
        if cfg.sub_bg { let _ = o.set_property("sub-back-color", "#A8000000"); }
        if req.start_ms > 0 { o.set_property("start", format!("+{}", req.start_ms as f64 / 1000.0).as_str())?; }
        // Certificates are checked everywhere (mpv's default is not to): Android with mpv-android's CA
        // bundle below, Linux with the system's, Windows through Schannel and the Windows store.
        o.set_property("tls-verify", "yes")?;
        // Safe user options and shaders from `<data_dir>/mpv/mpv.conf` (never overriding platform vo/hwdec/tls).
        for (k, v) in &parsed.options {
            if !k.starts_with("glsl-shader") {
                let _ = o.set_property(k.as_str(), v.as_str());
            }
        }
        // Request headers are added once mpv runs (add_headers): one list entry each.
        // Windows: mpv draws into its own child window of the app window (video.rs) with Direct3D 11.
        #[cfg(target_os = "windows")]
        {
            o.set_property("vo", "gpu-next")?;
            o.set_property("gpu-api", "d3d11")?;
            o.set_property("force-window", "no")?;
            o.set_property("cursor-autohide", "no")?;
            // Clicks and the cursor pass through to the WebView above it (newer mpv; older ones ignore it).
            let _ = o.set_property("cursor-passthrough", true);
            o.set_property("hwdec", "auto-safe")?;
            o.set_property("demuxer-max-bytes", "256MiB")?;
            // HDR to an HDR screen: mpv asks Windows for an HDR10 swapchain when the video and
            // the screen are HDR (Windows "Use HDR" on), and tone-maps to SDR otherwise.
            // "Always SDR" says no explicitly, whatever this mpv build's default. The panel's peak is
            // applied once mpv really sends HDR10 (`follow_target`), never to SDR output.
            let _ = o.set_property("target-colorspace-hint", if hdr_output { "yes" } else { "no" });
        }
        // Linux and macOS: mpv draws through its render API into the app's own surface (video.rs).
        #[cfg(not(any(target_os = "android", target_os = "windows")))]
        {
            o.set_property("vo", "libmpv")?;
            o.set_property("hwdec", "auto-safe")?;
            o.set_property("demuxer-max-bytes", "256MiB")?;
        }
        #[cfg(target_os = "android")]
        {
            o.set_property("vo", if direct { "mediacodec_embed" } else { "gpu-next" })?;
            o.set_property("gpu-context", "android")?;
            o.set_property("opengl-es", "yes")?;
            o.set_property("hwdec", if direct { "mediacodec" } else { "mediacodec-copy" })?;
            o.set_property("hwdec-codecs", "all")?;
            o.set_property("force-window", "no")?;
            o.set_property("ao", "audiotrack,opensles")?;
            o.set_property("audio-set-media-role", "yes")?;
            o.set_property("tls-ca-file", ca.as_str())?;
            o.set_property("demuxer-max-bytes", "96MiB")?;
            // The SurfaceView is sized to the picture (video.rs); mpv fills it exactly.
            o.set_property("keepaspect", "no")?;
        }
        Ok(())
    }).map_err(|e| anyhow::anyhow!("mpv couldn't start: {e}")).and_then(|mpv| {
        apply_shader_options(&mpv, &parsed.options);
        add_headers(&mpv, &req.headers)?;
        Ok(mpv)
    })
}

fn apply_shader_options(mpv: &Mpv, options: &[(String, String)]) {
    for (k, v) in options {
        match k.as_str() {
            "glsl-shaders" | "glsl-shaders-set" => { let _ = mpv.set_property("glsl-shaders", v.as_str()); }
            "glsl-shader" | "glsl-shaders-append" | "glsl-shaders-post" if !v.is_empty() => {
                let _ = mpv.command("change-list", &["glsl-shaders", "append", v.as_str()]);
            }
            "glsl-shaders-pre" if !v.is_empty() => {
                let _ = mpv.command("change-list", &["glsl-shaders", "pre", v.as_str()]);
            }
            "glsl-shaders-remove" if !v.is_empty() => {
                let _ = mpv.command("change-list", &["glsl-shaders", "remove", v.as_str()]);
            }
            "glsl-shaders-clr" => { let _ = mpv.set_property("glsl-shaders", ""); }
            _ => {}
        }
    }
}

/// The source's request headers, one `http-header-fields` entry each through `change-list
/// append`: setting the list as a whole splits on commas, which header values (cookies) hold.
/// A header that can't be passed is left out (as before), never the whole stream.
fn add_headers(mpv: &Mpv, headers: &[(String, String)]) -> anyhow::Result<()> {
    for (k, v) in headers {
        if k.is_empty() || k.contains([':', '\r', '\n', '\0']) || v.contains(['\r', '\n', '\0']) { continue; }
        if mpv.command("change-list", &["http-header-fields", "append", &format!("{k}: {v}")]).is_err() {
            // The name only: header values can be credentials.
            eprintln!("rstream: a request header ({k}) couldn't be passed to the player");
        }
    }
    Ok(())
}

const OBSERVED: [(&str, Format); 21] = [
    ("chapter-list", Format::String),
    ("frame-drop-count", Format::Int64),
    ("time-pos", Format::Double), ("duration", Format::Double), ("demuxer-cache-time", Format::Double),
    ("pause", Format::Flag), ("paused-for-cache", Format::Flag), ("seeking", Format::Flag), ("eof-reached", Format::Flag),
    ("speed", Format::Double), ("volume", Format::Double), ("mute", Format::Flag), ("sub-text", Format::String),
    ("sub-delay", Format::Double), ("audio-delay", Format::Double),
    ("hwdec-current", Format::String), ("video-format", Format::String), ("track-list", Format::String),
    ("aid", Format::String), ("sid", Format::String), ("core-idle", Format::Flag),
];

fn run(live: Arc<Live>, events: UnboundedSender<PlayerEvent>, stop: PlayerHandle) {
    let mpv = &live.mpv;
    let _ = mpv.disable_deprecated_events();
    for (i, (name, format)) in OBSERVED.iter().enumerate() { let _ = mpv.observe_property(name, *format, i as u64 + 1); }
    if !live.wait_ready(Duration::from_secs(10), &stop) {
        if stop.stopped() || live.finish.lock().is_ok_and(|f| f.is_some()) {
            if let Ok(mut a) = ACTIVE.lock() { if a.as_ref().is_some_and(|a| Arc::ptr_eq(a, &live)) { *a = None; } }
            let how = live.finish.lock().ok().and_then(|f| *f).unwrap_or(Finish::Close);
            let _ = events.send(PlayerEvent::Finished {
                position_ms: 0, duration_ms: 0,
                next: how == Finish::Next, another: matches!(how, Finish::Another | Finish::OtherLanguage),
                language: how == Finish::OtherLanguage,
            });
            return;
        }
        eprintln!("rstream: video surface not ready, opening anyway");
    }
    if let Err(e) = mpv.command("loadfile", &[live.request.url.as_str(), "replace"]) {
        backend::dev_log(&format!("playback_error: mpv loadfile failed ({e}) quality={:?} hdr={:?} direct={}", live.request.quality, live.request.hdr, live.state().direct));
        live.update(|s| { s.error = Some(format!("This source couldn't be opened ({e}).")); true });
    }
    let (mut position, mut duration, mut cache_end) = (0.0f64, 0.0f64, 0.0f64);
    let (mut paused_for_cache, mut seeking, mut paused, mut dropped) = (false, false, false, 0i64);
    let (mut stall_count, mut perf_reported) = (0u32, false);
    // The chapters, kept to place the last one's end once the duration is known.
    let mut chapters = String::new();
    let mark = |chapters: &str, duration: f64| {
        let found = segments(chapters, duration);
        let parsed = parse_chapters(chapters);
        live.update(|s| {
            let c = s.segments != found || s.chapters != parsed;
            s.segments = found;
            s.chapters = parsed;
            c
        });
    };
    let (mut announced, mut quitting, mut played) = (false, false, false);
    let mut checkpoint = Instant::now();
    let mut target_check = Instant::now();
    // Time actually played since the last report (seeks don't count), so "watched" means watched.
    let (mut last_tick, mut measured, mut speed) = (None::<f64>, 0.0f64, 1.0f64);
    let mut last_recorded_pos = -1.0f64;
    // The title's metadata goes with this session's first report and its last one.
    let mut sent_meta = false;
    let record = |position: f64, duration: f64, measured: &mut f64, sent_meta: &mut bool, last_recorded_pos: &mut f64, last: bool| if let Some((media, video)) = &live.request.record {
        if position > 0.0 && duration > 0.0 && (last || *measured > 0.0 || (position - *last_recorded_pos).abs() >= 1.0) {
            backend::record_progress(media.clone(), video.clone(), (position * 1000.0) as i64, (duration * 1000.0) as i64,
                (*measured * 1000.0) as i64, !*sent_meta || last);
            *measured = 0.0;
            *sent_meta = true;
            *last_recorded_pos = position;
        }
    };
    loop {
        if stop.stopped() && !quitting { quitting = true; live.finish(Finish::Close); }
        if checkpoint.elapsed() > Duration::from_secs(15) { checkpoint = Instant::now(); record(position, duration, &mut measured, &mut sent_meta, &mut last_recorded_pos, false); }
        // What mpv sends to the screen is known once it renders, and changes when Windows HDR is
        // switched or the window moves to another screen: follow it, and say so.
        if target_check.elapsed() > Duration::from_secs(2) {
            target_check = Instant::now();
            if follow_target(&live) && announced {
                let s = live.state();
                let _ = events.send(PlayerEvent::Started { decoder: hardware(&s), codec: codec_name(&s.codec), facts: facts(&live, &s) });
            }
        }
        let Some(event) = mpv.wait_event(0.25) else { continue };
        match event {
            Ok(Event::FileLoaded) => {
                refresh_tracks(&live);
                let size = mpv.get_property::<i64>("file-size").unwrap_or(0).max(0);
                live.update(|s| { s.loaded = true; s.error = None; s.file_size = size; true });
                skip_commentary(&live);
                // Profile 5 carries no HDR10 layer: without a Dolby Vision panel only mpv's renderer
                // (libplacebo's reshaping) shows its colours right.
                let s = live.state();
                if s.direct && s.dv_profile == 5 && !live.screen.unwrap_or_default().dolby_vision { live.render_dolby_vision(); }
            }
            Ok(Event::VideoReconfig) => {
                let p = |n: &str| mpv.get_property::<i64>(n).unwrap_or(0);
                let (w, h, dw, dh) = (p("video-params/w"), p("video-params/h"), p("video-params/dw"), p("video-params/dh"));
                let transfer: String = mpv.get_property("video-params/gamma").unwrap_or_default();
                let target: String = mpv.get_property("video-target-params/gamma").unwrap_or_default();
                let codec: String = mpv.get_property("video-format").unwrap_or_default();
                let hwdec: String = mpv.get_property("hwdec-current").unwrap_or_default();
                let fps = mpv.get_property::<f64>("container-fps").unwrap_or(0.0);
                live.update(|s| {
                    s.width = w; s.height = h; s.aspect = if dw > 0 && dh > 0 { dw as f64 / dh as f64 } else { 0.0 };
                    s.transfer = transfer; s.target_transfer = target; s.codec = codec; s.hwdec = hwdec; s.fps = fps; true
                });
                crate::video::layout(&live);
                if fps > 10.0 && fps < 240.0 { crate::video::set_frame_rate(fps); }
                // A subtitle chosen by default gets the same treatment as one picked by hand.
                let sid = live.state().tracks.iter().find(|t| !t.audio && t.selected).map(|t| t.id);
                if sid.is_some() { live.fit_subtitles(sid); }
                let s = live.state();
                #[cfg(target_os = "android")]
                crate::android::log(&format!("video {}x{} {} transfer={} hwdec={} direct={}", s.width, s.height, s.codec, s.transfer, s.hwdec, s.direct));
                if !announced && !s.codec.is_empty() {
                    announced = true;
                    let _ = events.send(PlayerEvent::Started { decoder: hardware(&s), codec: codec_name(&s.codec), facts: facts(&live, &s) });
                }
            }
            Ok(Event::PropertyChange { name, change, .. }) => match (name, change) {
                ("time-pos", PropertyData::Double(v)) => {
                    if let Some(prev) = last_tick { measured += played_between(prev, v, speed); }
                    #[cfg(target_os = "android")]
                    if last_tick.is_none() { crate::android::log(&format!("playing from {v:.1}s")); }
                    last_tick = Some(v);
                    position = v;
                    if v > 1.0 { played = true; }
                    live.update(|s| {
                        let moved = (s.position - v).abs() >= 0.25 || s.dropped != dropped;
                        if moved { s.position = v; s.buffered = (cache_end - v).max(0.0); s.dropped = dropped; }
                        moved
                    });
                }
                ("duration", PropertyData::Double(v)) => { duration = v; live.update(|s| { s.duration = v; true }); mark(&chapters, duration); }
                ("chapter-list", PropertyData::Str(j)) => { chapters = j.to_owned(); mark(&chapters, duration); }
                ("demuxer-cache-time", PropertyData::Double(v)) => {
                    cache_end = v;
                    if paused || paused_for_cache || seeking {
                        let buf = (cache_end - position).max(0.0);
                        live.update(|s| { let c = (s.buffered - buf).abs() >= 1.0; if c { s.buffered = buf; } c });
                    }
                }
                ("pause", PropertyData::Flag(v)) => {
                    paused = v;
                    live.update(|s| { s.paused = v; true });
                    #[cfg(target_os = "android")]
                    let _ = crate::android::video_media_state(!v, (position * 1000.0) as i64, speed as f32);
                }
                ("paused-for-cache", PropertyData::Flag(v)) => {
                    if v && played && !paused_for_cache { stall_count += 1; }
                    paused_for_cache = v;
                    #[cfg(target_os = "android")]
                    crate::android::log(&format!("{} at {position:.1}s", if v { "buffering" } else { "buffered" }));
                    let buf = paused_for_cache || seeking;
                    live.update(|s| { let c = s.buffering != buf; s.buffering = buf; c });
                    if !perf_reported && stall_count >= 3 {
                        perf_reported = true;
                        let s = live.state();
                        backend::dev_log(&format!("performance: repeated buffering stalls ({stall_count}x) codec={} {}x{} fps={:.2} hwdec={} direct={} dropped={dropped}",
                            s.codec, s.width, s.height, s.fps, s.hwdec, s.direct));
                    }
                }
                ("seeking", PropertyData::Flag(v)) => {
                    seeking = v;
                    let buf = paused_for_cache || seeking;
                    live.update(|s| { let c = s.buffering != buf; s.buffering = buf; c })
                }
                ("eof-reached", PropertyData::Flag(true)) if played => {
                    live.update(|s| { s.ended = true; true });
                    // If there is no next episode (or auto-next is disabled), close cleanly right away.
                    // With a next episode and auto-next on, the UI runs the short "Up Next" countdown.
                    if !live.request.has_next {
                        live.finish(Finish::Close);
                    } else if !backend::load_settings().auto_next {
                        live.finish(Finish::Next);
                    }
                }
                ("eof-reached", PropertyData::Flag(false)) => live.update(|s| { let c = s.ended; s.ended = false; c }),
                // Synced into State on time-pos ticks so a struggling decoder never causes a UI re-render storm.
                ("frame-drop-count", PropertyData::Int64(v)) => {
                    dropped = v;
                    if !perf_reported && played && dropped >= 30 {
                        perf_reported = true;
                        let s = live.state();
                        backend::dev_log(&format!("performance: high frame drops ({dropped}) codec={} {}x{} fps={:.2} hwdec={} direct={} transfer={}",
                            s.codec, s.width, s.height, s.fps, s.hwdec, s.direct, s.transfer));
                    }
                }
                ("speed", PropertyData::Double(v)) => {
                    speed = v;
                    live.update(|s| { s.speed = v; true });
                    #[cfg(target_os = "android")]
                    let _ = crate::android::video_media_state(!paused, (position * 1000.0) as i64, speed as f32);
                }
                ("volume", PropertyData::Double(v)) => live.update(|s| { s.volume = v; true }),
                ("mute", PropertyData::Flag(v)) => live.update(|s| { s.muted = v; true }),
                ("sub-delay", PropertyData::Double(v)) => live.update(|s| { let c = (s.sub_delay - v).abs() > 0.001; s.sub_delay = v; c }),
                ("audio-delay", PropertyData::Double(v)) => live.update(|s| { let c = (s.audio_delay - v).abs() > 0.001; s.audio_delay = v; c }),
                ("sub-text", PropertyData::Str(t)) => { let t = t.to_owned(); live.update(|s| { let c = s.subtitle != t; s.subtitle = t; c }) }
                ("sub-text", PropertyData::None) => live.update(|s| { let c = !s.subtitle.is_empty(); s.subtitle.clear(); c }),
                ("hwdec-current", PropertyData::Str(t)) => { let t = t.to_owned(); live.update(|s| { s.hwdec = t; true }) }
                ("track-list", PropertyData::Str(j)) => refresh_tracks_from_json(&live, j),
                ("track-list" | "aid" | "sid", _) => refresh_tracks(&live),
                _ => {}
            },
            Ok(Event::EndFile(reason)) if reason == mpv_end_file_reason::Error && !quitting => {
                #[cfg(target_os = "android")]
                crate::android::log("end-file: error");
                let s = live.state();
                backend::dev_log(&format!("playback_error: {} pos={position:.1}s codec={} {}x{} hwdec={} direct={} transfer={} dropped={dropped}",
                    if played { "stopped mid-stream" } else { "failed to start" }, s.codec, s.width, s.height, s.hwdec, s.direct, s.transfer));
                live.update(|s| { s.error = Some(if played { "The stream stopped." } else { "This source couldn't be played." }.into()); true });
            }
            Ok(Event::Shutdown) => break,
            _ => {}
        }
        if live.finish.lock().map(|f| f.is_some()).unwrap_or(false) { quitting = true; }
    }
    record(position, duration, &mut measured, &mut sent_meta, &mut last_recorded_pos, true);
    if let Ok(mut a) = ACTIVE.lock() { if a.as_ref().is_some_and(|a| Arc::ptr_eq(a, &live)) { *a = None; } }
    let how = live.finish.lock().ok().and_then(|f| *f).unwrap_or(Finish::Close);
    let _ = events.send(PlayerEvent::Finished {
        position_ms: (position * 1000.0) as i64, duration_ms: (duration * 1000.0) as i64,
        next: how == Finish::Next, another: matches!(how, Finish::Another | Finish::OtherLanguage),
        language: how == Finish::OtherLanguage,
    });
}

fn refresh_tracks(live: &Live) {
    let json: String = live.mpv.get_property("track-list").unwrap_or_default();
    refresh_tracks_from_json(live, &json);
}

fn refresh_tracks_from_json(live: &Live, json: &str) {
    let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    let tracks = parse_tracks_list(&list);
    let dv = dv_profile_list(&list);
    live.update(|s| { let c = s.tracks != tracks || s.dv_profile != dv; s.tracks = tracks; s.dv_profile = dv; c });
}

/// All chapters from mpv's `chapter-list` (JSON), sorted chronologically with sensible fallback names.
pub fn parse_chapters(json: &str) -> Vec<Chapter> {
    let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    let mut out: Vec<Chapter> = list.iter().enumerate().filter_map(|(i, c)| {
        let start = c["time"].as_f64()?.max(0.0);
        let raw = c["title"].as_str().unwrap_or("").trim();
        let title = if raw.is_empty() { format!("Chapter {}", i + 1) } else { raw.to_owned() };
        Some(Chapter { title, start })
    }).collect();
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    out
}

/// Intro, recap and credits from mpv's `chapter-list` (JSON). A chapter runs to the next one's
/// start (the last one to `duration`). Names are matched as whole words; a chapter too long or
/// too short for what its name claims is a misnamed one and left alone, and credits only count
/// in the second half of the video.
pub fn segments(json: &str, duration: f64) -> Vec<Segment> {
    let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    let mut marks: Vec<(f64, &str)> = list.iter().filter_map(|c| Some((c["time"].as_f64()?, c["title"].as_str().unwrap_or("")))).collect();
    marks.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = Vec::new();
    for (i, (start, title)) in marks.iter().enumerate() {
        let end = marks.get(i + 1).map(|m| m.0).unwrap_or(duration);
        let Some(kind) = chapter_kind(title) else { continue };
        let length = end - start;
        let fits = match kind {
            SegmentKind::Intro => (5.0..=240.0).contains(&length),
            SegmentKind::Recap => (5.0..=300.0).contains(&length),
            SegmentKind::Credits => (5.0..=900.0).contains(&length) && duration > 0.0 && *start >= duration / 2.0,
        };
        if fits { out.push(Segment { kind, start: *start, end }); }
    }
    out
}

fn chapter_kind(title: &str) -> Option<SegmentKind> {
    let t = title.to_lowercase();
    let words: Vec<&str> = t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |w: &str| words.contains(&w);
    // "OP", "OP1", "ED 2": the anime names, only when they are the whole name.
    let short = |p: &str| words.len() <= 2 && words.first().is_some_and(|w| w.strip_prefix(p).is_some_and(|r| r.chars().all(|c| c.is_ascii_digit())))
        && words.get(1).is_none_or(|w| w.chars().all(|c| c.is_ascii_digit()));
    if has("recap") || words.first() == Some(&"previously") { return Some(SegmentKind::Recap); }
    if has("intro") || has("opening") || short("op") || t.contains("title sequence") || t.contains("main title") { return Some(SegmentKind::Intro); }
    if has("credits") || has("outro") || has("ending") || short("ed") || t.contains("end titles") { return Some(SegmentKind::Credits); }
    None
}

/// The segment being played that can be skipped now: not in its last second and a half, where a
/// skip would only cut a cut.
pub fn skip_offer(segments: &[Segment], position: f64) -> Option<Segment> {
    segments.iter().copied().find(|g| position >= g.start && position < g.end - 1.5)
}

/// Seconds of playback between two time-pos reports: a small step forward is watching, a jump
/// (a seek, either way) is not.
pub fn played_between(prev: f64, now: f64, speed: f64) -> f64 {
    let step = now - prev;
    if step > 0.0 && step <= 2.0 * speed.max(1.0) { step } else { 0.0 }
}

/// The selected video track's Dolby Vision profile, 0 without one.
fn dv_profile_list(list: &[serde_json::Value]) -> i64 {
    list.iter().find(|t| t["type"] == "video" && t["selected"] == true)
        .and_then(|t| t["dolby-vision-profile"].as_i64()).unwrap_or(0)
}

#[cfg(test)]
pub fn parse_tracks(json: &str) -> Vec<Track> {
    let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    parse_tracks_list(&list)
}

fn parse_tracks_list(list: &[serde_json::Value]) -> Vec<Track> {
    list.iter().filter_map(|t| {
        let audio = match t["type"].as_str()? { "audio" => true, "sub" => false, _ => return None };
        let id = t["id"].as_i64()?;
        let title = t["title"].as_str().unwrap_or("").trim();
        let language = t["lang"].as_str().map(language_name).unwrap_or_default();
        let codec = t["codec"].as_str().unwrap_or("");
        let channels = t["demux-channel-count"].as_i64().unwrap_or(0);
        let mut parts: Vec<String> = Vec::new();
        if !title.is_empty() { parts.push(title.to_owned()); }
        if !language.is_empty() && !title.to_lowercase().contains(&language.to_lowercase()) { parts.push(language); }
        if audio && channels > 0 { parts.push(match channels { 1 => "Mono".into(), 2 => "Stereo".into(), 6 => "5.1".into(), 8 => "7.1".into(), n => format!("{n} ch") }); }
        if let Some(c) = pretty_codec(codec) { parts.push(c); }
        if t["external"].as_bool() == Some(true) { parts.push("External".into()); }
        let image = t["image"].as_bool().unwrap_or(matches!(codec, "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle"));
        let styled = matches!(codec, "ass" | "ssa");
        Some(Track { id, audio, label: if parts.is_empty() { format!("Track {id}") } else { parts.join(", ") }, selected: t["selected"].as_bool() == Some(true), image, styled,
            // "und" (undetermined), "mul" (several), "zxx" (no speech) say nothing about the language.
            lang: t["lang"].as_str().map(str::trim).filter(|l| !["und", "mul", "zxx", "mis"].contains(&l.to_lowercase().as_str())).unwrap_or_default().to_owned(),
            forced: t["forced"].as_bool() == Some(true) || title.to_lowercase().contains("forced") })
    }).collect()
}

fn pretty_codec(c: &str) -> Option<String> {
    Some(match c {
        "truehd" => "Dolby TrueHD", "eac3" => "Dolby Digital Plus", "ac3" => "Dolby Digital", "dts" => "DTS", "aac" => "AAC",
        "opus" => "Opus", "flac" => "FLAC", "subrip" => "SRT", "ass" => "ASS", "hdmv_pgs_subtitle" => "PGS", "webvtt" => "WebVTT",
        "dvd_subtitle" => "VobSub", "mov_text" => "Text", "" => return None, other => return Some(other.to_uppercase()),
    }.into())
}

/// A language code (ISO 639-1 or -2, as mpv and the subtitle add-ons write them) in English.
pub fn language_name(code: &str) -> String {
    let c = code.to_lowercase();
    match c.as_str() {
        "en" | "eng" => "English", "it" | "ita" => "Italian", "es" | "spa" => "Spanish", "fr" | "fre" | "fra" => "French",
        "de" | "ger" | "deu" => "German", "pt" | "por" => "Portuguese", "ja" | "jpn" => "Japanese", "ko" | "kor" => "Korean",
        "zh" | "chi" | "zho" => "Chinese", "ru" | "rus" => "Russian", "nl" | "dut" | "nld" => "Dutch", "pl" | "pol" => "Polish",
        "sv" | "swe" => "Swedish", "da" | "dan" => "Danish", "no" | "nor" | "nob" | "nb" => "Norwegian", "fi" | "fin" => "Finnish",
        "tr" | "tur" => "Turkish", "ar" | "ara" => "Arabic", "hi" | "hin" => "Hindi", "el" | "gre" | "ell" => "Greek",
        "he" | "heb" => "Hebrew", "cs" | "cze" | "ces" => "Czech", "hu" | "hun" => "Hungarian", "ro" | "rum" | "ron" => "Romanian",
        "pb" | "pob" | "pt-br" => "Portuguese (Brazil)", "uk" | "ukr" => "Ukrainian", "bg" | "bul" => "Bulgarian", "hr" | "hrv" => "Croatian",
        "sr" | "srp" | "scc" => "Serbian", "sl" | "slv" => "Slovenian", "sk" | "slo" | "slk" => "Slovak", "et" | "est" => "Estonian",
        "lv" | "lav" => "Latvian", "lt" | "lit" => "Lithuanian", "fa" | "per" | "fas" => "Persian", "id" | "ind" => "Indonesian",
        "ms" | "may" | "msa" => "Malay", "th" | "tha" => "Thai", "vi" | "vie" => "Vietnamese", "ca" | "cat" => "Catalan",
        "eu" | "baq" | "eus" => "Basque", "gl" | "glg" => "Galician", "is" | "ice" | "isl" => "Icelandic", "mk" | "mac" | "mkd" => "Macedonian",
        "sq" | "alb" | "sqi" => "Albanian", "bs" | "bos" => "Bosnian", "bn" | "ben" => "Bengali", "ta" | "tam" => "Tamil",
        "te" | "tel" => "Telugu", "ml" | "mal" => "Malayalam", "ur" | "urd" => "Urdu", "tl" | "tgl" | "fil" => "Filipino",
        "zt" | "zht" => "Chinese (traditional)", "ze" => "Chinese (bilingual)", "ea" | "spl" | "spn" => "Spanish (Latin America)",
        "ka" | "geo" | "kat" => "Georgian", "hy" | "arm" | "hye" => "Armenian", "km" | "khm" => "Khmer", "my" | "bur" | "mya" => "Burmese",
        "af" | "afr" => "Afrikaans", "sw" | "swa" => "Swahili", "kk" | "kaz" => "Kazakh", "mn" | "mon" => "Mongolian", "ne" | "nep" => "Nepali",
        "si" | "sin" => "Sinhala", "eo" | "epo" => "Esperanto", "ga" | "gle" => "Irish", "cy" | "wel" | "cym" => "Welsh", "la" | "lat" => "Latin",
        "und" | "" => "", _ => return code.to_uppercase(),
    }.into()
}

/// The hardware API decoding the picture, or None in software.
pub fn hardware(s: &State) -> Option<String> {
    Some(s.hwdec.clone()).filter(|h| !h.is_empty() && h != "no").map(|h| match h.as_str() {
        "mediacodec" | "mediacodec-copy" => "MediaCodec".into(), "vaapi" | "vaapi-copy" => "VAAPI".into(), "nvdec" | "nvdec-copy" | "cuda" => "NVDEC".into(),
        "videotoolbox" | "videotoolbox-copy" => "VideoToolbox".into(), "d3d11va" | "d3d11va-copy" => "D3D11VA".into(), "vulkan" => "Vulkan".into(), other => other.to_owned(),
    })
}

pub fn codec_name(codec: &str) -> String {
    match codec { "hevc" => "HEVC".into(), "h264" => "H.264".into(), "av1" => "AV1".into(), "vp9" => "VP9".into(), "" => String::new(), c => c.to_uppercase() }
}

/// What the screen really shows. The stream's transfer decides HDR; the source label only adds
/// what a stream can't state (the "+" of HDR10+, Dolby Vision). `target`: the transfer mpv
/// renders to (`video-target-params/gamma`), "pq" when it sends HDR10 to the screen itself.
/// Unknown stays empty.
pub fn picture(transfer: &str, label: Option<&str>, dv_profile: i64, screen: Screen, direct: bool, target: &str) -> String {
    let label = label.unwrap_or("");
    let plus = label.contains("10+");
    let dv = dv_profile > 0 || label.to_lowercase().contains("dolby") || label.contains("DV");
    // Profile 5 is PQ underneath even where the stream's colour tags are blank.
    let transfer = if dv_profile == 5 && !transfer.is_empty() { "pq" } else { transfer };
    // Rendered by mpv into an HDR10 swapchain (Windows HDR): what reaches the panel is HDR10.
    if !direct && target == "pq" && matches!(transfer, "pq" | "hlg") {
        return if dv { "HDR10 (Dolby Vision source)".into() } else if plus { "HDR10 (HDR10+ source)".into() }
            else if transfer == "hlg" { "HDR10 (HLG source)".into() } else { "HDR10".into() };
    }
    match transfer {
        "pq" => {
            if direct && screen.hdr10 {
                // The panel only takes what it declares; otherwise it shows the static HDR10 layer.
                if dv && !screen.dolby_vision { "HDR10 (Dolby Vision source)".into() }
                else if dv { "Dolby Vision".into() }
                else if plus && !screen.hdr10_plus { "HDR10 (HDR10+ source)".into() }
                else if plus { "HDR10+".into() } else { "HDR10".into() }
            } else {
                let name = if dv { "Dolby Vision" } else if plus { "HDR10+" } else { "HDR10" };
                if direct { format!("{name}, tone-mapped by the system") } else { format!("{name}, tone-mapped to SDR") }
            }
        }
        "hlg" => if direct && screen.hlg { "HLG".into() } else if direct { "HLG, tone-mapped by the system".into() } else { "HLG, tone-mapped to SDR".into() },
        "" => String::new(),
        _ => "SDR".into(),
    }
}

/// `alang` picks a track by language before the file's default, so it can land on a commentary or
/// audio-description track: then take the first other track in that language, else the default.
pub fn extra_audio(t: &Track) -> bool {
    let l = t.label.to_lowercase();
    t.audio && (l.contains("commentary") || l.contains("description") || l.contains("descriptive"))
}

fn skip_commentary(live: &Live) {
    let s = live.state();
    let Some(on) = s.tracks.iter().find(|t| t.audio && t.selected) else { return };
    if !extra_audio(on) { return; }
    let main = s.tracks.iter().find(|t| t.audio && !extra_audio(t) && t.lang == on.lang && !on.lang.is_empty())
        .or_else(|| s.tracks.iter().find(|t| t.audio && !extra_audio(t)));
    if let Some(t) = main { live.select(true, Some(t.id)); }
}

/// Read the transfer mpv renders to; true when it changed. On Windows, HDR10 output is mapped to
/// the panel's peak (read again, the window may have moved), and SDR output goes back to mpv's own.
fn follow_target(live: &Live) -> bool {
    let target: String = live.mpv.get_property("video-target-params/gamma").unwrap_or_default();
    // macOS: mpv's render API doesn't report it; the video layer knows what it asked for.
    #[cfg(target_os = "macos")]
    let target = if target.is_empty() && live.state().loaded { crate::video::sent_transfer().to_owned() } else { target };
    if target.is_empty() || live.state().target_transfer == target { return false; }
    #[cfg(target_os = "windows")]
    {
        let peak = crate::display::screen().filter(|s| s.hdr10).map(|s| s.peak_nits).filter(|p| (300..=10_000).contains(p));
        match peak {
            Some(p) if target == "pq" => { let _ = live.mpv.set_property("target-peak", p as i64); }
            _ => { let _ = live.mpv.set_property("target-peak", "auto"); }
        }
    }
    live.update(|s| { s.target_transfer = target; true });
    true
}

pub fn facts(live: &Live, s: &State) -> Facts {
    Facts {
        resolution: Some(crate::player::resolution_label(s.width, s.height)).filter(|r| !r.is_empty()).or(live.request.quality.clone()).unwrap_or_default(),
        picture: picture(&s.transfer, live.request.hdr.as_deref(), s.dv_profile, live.screen.unwrap_or_default(), s.direct, &s.target_transfer),
        // The file mpv opened (an episode's own size, not its season pack's), else what the source declared.
        size: if s.file_size > 0 { backend::file_size_label(s.file_size as u64) } else { live.request.size.clone().unwrap_or_default() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the real libmpv: every language setting is an `alang` list mpv takes as is.
    #[test]
    fn audio_language_lists_reach_mpv() {
        ensure_c_numeric_locale();
        for l in crate::addons::LANGUAGES {
            let codes = backend::track_codes(l.key).join(",");
            assert!(!codes.is_empty(), "{}", l.key);
            let mpv = Mpv::with_initializer(|o| { o.set_property("config", false)?; o.set_property("vo", "null")?; o.set_property("alang", codes.as_str())?; Ok(()) }).unwrap();
            let back: String = mpv.get_property("alang").unwrap();
            assert_eq!(back, codes);
        }
    }

    /// Against the real libmpv: a header with a comma stays one entry.
    #[test]
    fn headers_with_commas_reach_mpv_whole() {
        ensure_c_numeric_locale();
        let mpv = Mpv::with_initializer(|o| { o.set_property("config", false)?; o.set_property("vo", "null")?; o.set_property("ao", "null")?; Ok(()) }).unwrap();
        add_headers(&mpv, &[("Cookie".into(), "a=1, b=2".into()), ("X-Token".into(), "t".into()), ("Bad:Name".into(), "x".into()), ("Nul".into(), "a\0b".into())]).unwrap();
        let list: String = mpv.get_property("http-header-fields").unwrap();
        assert!(list.contains("Cookie: a=1, b=2") && list.contains("X-Token: t") && !list.contains("Bad:Name"), "{list}");
        // mpv prints a list joined by commas; removing the cookie as one entry proves it is one.
        mpv.command("change-list", &["http-header-fields", "remove", "Cookie: a=1, b=2"]).unwrap();
        let rest: String = mpv.get_property("http-header-fields").unwrap();
        assert_eq!(rest, "X-Token: t");
    }
    #[test]
    fn chapters_name_the_intro_recap_and_credits() {
        let json = r#"[{"title":"Previously on","time":0},{"title":"Prologue","time":40},{"title":"Opening","time":200},
            {"title":"Part A","time":290},{"title":"Ending Credits","time":1300},{"title":"Preview","time":1390}]"#;
        let got = segments(json, 1420.0);
        assert_eq!(got, vec![
            Segment { kind: SegmentKind::Recap, start: 0.0, end: 40.0 },
            Segment { kind: SegmentKind::Intro, start: 200.0, end: 290.0 },
            Segment { kind: SegmentKind::Credits, start: 1300.0, end: 1390.0 },
        ]);
        // Opening credits are the intro; anime short names count only alone.
        assert_eq!(chapter_kind("Opening Credits"), Some(SegmentKind::Intro));
        assert_eq!(chapter_kind("OP2"), Some(SegmentKind::Intro));
        assert_eq!(chapter_kind("ED 1"), Some(SegmentKind::Credits));
        assert_eq!(chapter_kind("Operation Overlord"), None);
        assert_eq!(chapter_kind("Edward"), None);
        assert_eq!(chapter_kind("Introduction"), None);
        assert_eq!(chapter_kind("Chapter 3"), None);
        // A whole-act "Intro", credits early on, an unordered list: misnamed or sorted out.
        assert!(segments(r#"[{"title":"Intro","time":0},{"title":"Act 2","time":900}]"#, 2000.0).is_empty());
        assert!(segments(r#"[{"title":"Credits","time":10},{"title":"Story","time":60}]"#, 2000.0).is_empty());
        assert_eq!(segments(r#"[{"title":"Story","time":30},{"title":"Intro","time":0}]"#, 600.0),
            vec![Segment { kind: SegmentKind::Intro, start: 0.0, end: 30.0 }]);
        assert!(segments("", 100.0).is_empty());
        let ch = parse_chapters(r#"[{"title":"Act 2","time":300},{"title":"","time":0}]"#);
        assert_eq!(ch, vec![
            Chapter { title: "Chapter 2".into(), start: 0.0 },
            Chapter { title: "Act 2".into(), start: 300.0 },
        ]);
    }
    #[test]
    fn chapters_come_before_the_databases() {
        let chapter = Segment { kind: SegmentKind::Intro, start: 60.0, end: 150.0 };
        let online = vec![Segment { kind: SegmentKind::Intro, start: 58.0, end: 149.0 }, Segment { kind: SegmentKind::Credits, start: 1300.0, end: 1400.0 }];
        let s = State { segments: vec![chapter], online_segments: online.clone(), ..Default::default() };
        assert_eq!(s.skips(), vec![chapter, online[1]]);
    }
    #[test]
    fn a_skip_is_offered_inside_the_segment_only() {
        let g = [Segment { kind: SegmentKind::Intro, start: 60.0, end: 150.0 }];
        assert_eq!(skip_offer(&g, 59.0), None);
        assert_eq!(skip_offer(&g, 60.0), Some(g[0]));
        assert_eq!(skip_offer(&g, 148.0), Some(g[0]));
        assert_eq!(skip_offer(&g, 149.0), None);
    }
    #[test]
    fn only_small_forward_steps_count_as_watching() {
        assert_eq!(super::played_between(10.0, 10.5, 1.0), 0.5);
        assert_eq!(super::played_between(10.0, 20.0, 1.0), 0.0); // a +10 s seek
        assert_eq!(super::played_between(20.0, 10.0, 1.0), 0.0); // back
        assert_eq!(super::played_between(10.0, 13.0, 2.0), 3.0); // fast playback
        assert_eq!(super::played_between(10.0, 10.0, 1.0), 0.0);
    }
    #[test]
    fn hdr_is_named_by_what_reaches_the_panel() {
        let hdr = Screen { hdr10: true, hlg: true, hdr10_plus: true, dolby_vision: false, peak_nits: 0 };
        assert_eq!(picture("pq", Some("HDR10+"), 0, hdr, true, ""), "HDR10+");
        assert_eq!(picture("pq", Some("HDR10+"), 0, Screen { hdr10: true, ..Default::default() }, true, ""), "HDR10 (HDR10+ source)");
        assert_eq!(picture("pq", Some("HDR10+"), 0, hdr, false, ""), "HDR10+, tone-mapped to SDR");
        assert_eq!(picture("pq", None, 0, Screen::default(), true, ""), "HDR10, tone-mapped by the system");
        assert_eq!(picture("hlg", None, 0, hdr, true, ""), "HLG");
        assert_eq!(picture("bt.1886", Some("HDR10"), 0, hdr, true, ""), "SDR");
        assert_eq!(picture("", None, 0, hdr, true, ""), "");
        // Dolby Vision is named only when the panel takes it.
        assert_eq!(picture("pq", Some("Dolby Vision"), 8, hdr, true, ""), "HDR10 (Dolby Vision source)");
        assert_eq!(picture("pq", None, 8, Screen { dolby_vision: true, ..hdr }, true, ""), "Dolby Vision");
        assert_eq!(picture("bt.1886", None, 5, hdr, false, ""), "Dolby Vision, tone-mapped to SDR");
        // Windows with HDR on: mpv renders into an HDR10 swapchain (target "pq").
        assert_eq!(picture("bt.1886", None, 5, Screen::default(), false, "pq"), "HDR10 (Dolby Vision source)");
        assert_eq!(picture("pq", Some("HDR10+"), 0, Screen::default(), false, "pq"), "HDR10 (HDR10+ source)");
        assert_eq!(picture("hlg", None, 0, Screen::default(), false, "pq"), "HDR10 (HLG source)");
        assert_eq!(picture("pq", None, 0, Screen::default(), false, "bt.1886"), "HDR10, tone-mapped to SDR");
        assert_eq!(picture("bt.1886", None, 0, Screen::default(), false, "pq"), "SDR");
    }
    #[test]
    fn tracks_read_like_a_menu() {
        let json = r#"[{"id":1,"type":"video","codec":"hevc","selected":true,"dolby-vision-profile":8},
            {"id":1,"type":"audio","lang":"eng","codec":"eac3","demux-channel-count":6,"selected":true},
            {"id":2,"type":"audio","lang":"ita","title":"Italiano","codec":"ac3","demux-channel-count":2},
            {"id":3,"type":"audio","lang":"und","title":"Director Commentary","codec":"aac","demux-channel-count":2},
            {"id":1,"type":"sub","lang":"eng","codec":"hdmv_pgs_subtitle","image":true},
            {"id":2,"type":"sub","lang":"ita","title":"Forced signs","codec":"ass","forced":true,"external":true}]"#;
        let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert_eq!(dv_profile_list(&list), 8);
        let t = parse_tracks(json);
        assert_eq!(t.len(), 5);
        assert_eq!(t[0].label, "English, 5.1, Dolby Digital Plus");
        assert!(t[0].selected && t[0].audio);
        assert_eq!(t[1].label, "Italiano, Stereo, Dolby Digital");
        assert!(t[2].lang.is_empty(), "undetermined language tags stay empty");
        assert!(extra_audio(&t[2]), "commentary tracks are flagged as extra");
        assert!(t[3].image && !t[3].audio);
        assert!(t[4].styled && t[4].forced && t[4].label.contains("External"));
    }

    /// Against the real libmpv: streaming stability properties (HTTP reconnect, network timeout,
    /// demuxer lookahead) are accepted and retained by the installed libmpv.
    #[test]
    fn streaming_resilience_options_reach_mpv() {
        ensure_c_numeric_locale();
        let mpv = Mpv::with_initializer(|o| {
            o.set_property("config", false)?;
            o.set_property("vo", "null")?;
            o.set_property("ao", "null")?;
            o.set_property("demuxer-readahead-secs", 60i64)?;
            o.set_property("network-timeout", 20.0f64)?;
            o.set_property("stream-lavf-o", "reconnect=1,reconnect_streamed=1,reconnect_on_network_error=1,reconnect_on_http_error=5xx,reconnect_delay_max=5")?;
            Ok(())
        }).unwrap();
        assert_eq!(mpv.get_property::<i64>("demuxer-readahead-secs").unwrap(), 60);
        assert!((mpv.get_property::<f64>("network-timeout").unwrap() - 20.0).abs() < 0.01);
        let lavf: String = mpv.get_property("stream-lavf-o").unwrap();
        assert!(lavf.contains("reconnect=1") && lavf.contains("reconnect_on_network_error=1"), "{lavf}");
    }

    /// Cancelling a session before its video surface attaches wakes wait_ready immediately.
    #[test]
    fn finish_wakes_wait_ready_without_blocking() {
        ensure_c_numeric_locale();
        let mpv = Mpv::with_initializer(|o| { o.set_property("config", false)?; o.set_property("vo", "null")?; o.set_property("ao", "null")?; Ok(()) }).unwrap();
        let (state, _) = watch::channel(State::default());
        let live = Arc::new(Live { mpv, request: PlayRequest::default(), screen: None, state, finish: Mutex::new(None), ready: (Mutex::new(false), Condvar::new()) });
        let stop = PlayerHandle::default();
        let live2 = live.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            live2.finish(Finish::Close);
        });
        let t0 = Instant::now();
        assert!(!live.wait_ready(Duration::from_secs(10), &stop));
        assert!(t0.elapsed() < Duration::from_secs(2), "wait_ready blocked for {:?}", t0.elapsed());
    }

    #[test]
    fn singular_and_append_glsl_shader_options_report_in_active_shaders() {
        ensure_c_numeric_locale();
        let mpv_dir = std::path::Path::new("/tmp/rstream-mpv-shader-test");
        let parsed = rstream_mpv_config::parse_mpv_conf(
            "glsl-shader=~~/shaders/a.glsl\nglsl-shaders-append=~~/shaders/b.glsl\n",
            mpv_dir,
        );
        let mpv = Mpv::with_initializer(|o| {
            o.set_property("config", false)?;
            o.set_property("vo", "null")?;
            o.set_property("ao", "null")?;
            Ok(())
        }).unwrap();
        apply_shader_options(&mpv, &parsed.options);
        let active = split_shader_list(&mpv.get_property::<String>("glsl-shaders").unwrap_or_default());
        assert_eq!(active, vec![
            "/tmp/rstream-mpv-shader-test/shaders/a.glsl".to_string(),
            "/tmp/rstream-mpv-shader-test/shaders/b.glsl".to_string(),
        ]);
    }
}
