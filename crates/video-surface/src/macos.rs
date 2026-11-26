//! macOS: mpv can't draw into another app's view there (no `wid`), so it draws through its render
//! API into a Core Animation OpenGL layer of ours, on a view placed below the WKWebView inside the
//! same window (the way IINA and other libmpv players on macOS do it). The WebView doesn't draw a
//! background (the app makes the window transparent): the page paints its own black, and nothing
//! while a title plays. HDR: on a screen with EDR headroom the layer turns EDR with a PQ colour
//! space and mpv sends PQ to it; anywhere else mpv tone-maps to SDR.
//!
//! Threads: AppKit work on the main thread (attach, detach); Core Animation asks `canDraw`/`draw` on
//! its own thread with our GL context current; mpv's update callback only raises a flag.
#![allow(deprecated)] // Apple marks OpenGL deprecated; it remains the API mpv's renderer offers on macOS.
use crate::{Host, Path, Player};
use std::sync::Arc;
use libmpv2_sys as sys;
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{define_class, msg_send, ClassType, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSScreen, NSView, NSWindow, NSWindowOrderingMode};
use objc2_core_graphics::{CGColor, CGColorSpace};
use objc2_core_video::CVTimeStamp;
use objc2_open_gl::{CGLChoosePixelFormat, CGLContextObj, CGLCreateContext, CGLLockContext, CGLPixelFormatAttribute as A,
    CGLPixelFormatObj, CGLReleasePixelFormat, CGLRetainContext, CGLRetainPixelFormat, CGLSetCurrentContext, CGLGetCurrentContext, CGLUnlockContext};
use objc2_quartz_core::{CALayer, CAOpenGLLayer, CATransaction};
use std::cell::RefCell;
use std::ffi::{c_char, c_void};
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Mutex;

#[link(name = "OpenGL", kind = "framework")]
extern "C" {
    fn glGetIntegerv(name: u32, data: *mut i32);
    fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    fn glClear(mask: u32);
    // The self-test's offscreen target.
    fn glGenFramebuffers(n: i32, ids: *mut u32);
    fn glBindFramebuffer(target: u32, id: u32);
    fn glDeleteFramebuffers(n: i32, ids: *const u32);
    fn glGenTextures(n: i32, ids: *mut u32);
    fn glBindTexture(target: u32, id: u32);
    fn glDeleteTextures(n: i32, ids: *const u32);
    fn glTexImage2D(target: u32, level: i32, internal: i32, w: i32, h: i32, border: i32, format: u32, kind: u32, data: *const c_void);
    fn glFramebufferTexture2D(target: u32, attachment: u32, textarget: u32, texture: u32, level: i32);
    fn glCheckFramebufferStatus(target: u32) -> u32;
    fn glReadPixels(x: i32, y: i32, w: i32, h: i32, format: u32, kind: u32, data: *mut c_void);
}
const GL_DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_VIEWPORT: u32 = 0x0BA2;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA16F: i32 = 0x881A;
const GL_RGBA: u32 = 0x1908;
const GL_FLOAT: u32 = 0x1406;

/// The session being drawn and mpv's renderer for it (created with our GL context current).
/// `_player` keeps mpv's core alive until the renderer is freed (mpv requires that order).
struct Session { _player: Arc<dyn Player>, render: *mut sys::mpv_render_context }
// SAFETY: the renderer is only used under RENDER's lock with our GL context current.
unsafe impl Send for Session {}
static RENDER: Mutex<Option<Session>> = Mutex::new(None);

/// One GL pixel format and context for the app's lifetime, shared with the layer.
struct Gl { format: CGLPixelFormatObj, context: CGLContextObj, float: bool, label: &'static str }
// SAFETY: CGL objects are reference-counted C objects usable from any thread under CGLLockContext.
unsafe impl Send for Gl {}
static GL: Mutex<Option<Gl>> = Mutex::new(None);

/// mpv has a new frame; or the layer must redraw (new session, resize) whatever mpv says.
static PENDING: AtomicBool = AtomicBool::new(false);
static DIRTY: AtomicBool = AtomicBool::new(false);
/// The screen's EDR headroom, read on the main thread at attach (f64 bits); 1.0 is SDR only.
static HEADROOM: AtomicU64 = AtomicU64::new(0);
static POTENTIAL: AtomicU64 = AtomicU64::new(0);
/// What the layer and mpv are set to: 0 SDR, 1 PQ in BT.2020, 2 PQ in Display P3.
static HDR_MODE: AtomicU64 = AtomicU64::new(0);

/// The video layer, for HDR changes from the engine thread (layer properties are thread-safe in
/// an explicit CATransaction).
struct SendLayer(Retained<VideoLayer>);
// SAFETY: only Core Animation's thread-safe property setters are used from other threads.
unsafe impl Send for SendLayer {}
static LAYER: Mutex<Option<SendLayer>> = Mutex::new(None);
thread_local! { static VIEW: RefCell<Option<Retained<NSView>>> = const { RefCell::new(None) }; }

define_class!(
    // SAFETY: CAOpenGLLayer may be subclassed; the overrides keep its contracts (retained returns).
    #[unsafe(super(CAOpenGLLayer, CALayer, NSObject))]
    #[name = "VeyloVideoLayer"]
    struct VideoLayer;

    impl VideoLayer {
        #[unsafe(method(copyCGLPixelFormatForDisplayMask:))]
        fn copy_pixel_format(&self, _mask: u32) -> CGLPixelFormatObj {
            match gl() { Some((format, _, _)) => unsafe { CGLRetainPixelFormat(format) }, None => null_mut() }
        }

        #[unsafe(method(copyCGLContextForPixelFormat:))]
        fn copy_context(&self, _pf: CGLPixelFormatObj) -> CGLContextObj {
            match gl() { Some((_, context, _)) => unsafe { CGLRetainContext(context) }, None => null_mut() }
        }

        #[unsafe(method(canDrawInCGLContext:pixelFormat:forLayerTime:displayTime:))]
        fn can_draw(&self, _ctx: CGLContextObj, _pf: CGLPixelFormatObj, _t: f64, _ts: *const CVTimeStamp) -> bool {
            wants_frame()
        }

        #[unsafe(method(drawInCGLContext:pixelFormat:forLayerTime:displayTime:))]
        fn draw(&self, ctx: CGLContextObj, pf: CGLPixelFormatObj, t: f64, ts: *const CVTimeStamp) {
            let rendered = unsafe { draw_frame() };
            // The superclass flushes the frame to the layer.
            unsafe { let _: () = msg_send![super(self), drawInCGLContext: ctx, pixelFormat: pf, forLayerTime: t, displayTime: ts]; }
            if rendered {
                if let Ok(session) = RENDER.lock() {
                    if let Some(s) = session.as_ref() {
                        unsafe { sys::mpv_render_context_report_swap(s.render); }
                    }
                }
            }
        }
    }
);

/// The pixel format and context, created once: 16-bit float colour (what EDR needs) on an OpenGL 3.2
/// core profile when the GPU offers it, then 8-bit, the legacy profile, any renderer, and last
/// Apple's software renderer (virtual machines without GPU OpenGL).
fn gl() -> Option<(CGLPixelFormatObj, CGLContextObj, bool)> {
    let mut gl = GL.lock().ok()?;
    if gl.is_none() {
        let accelerated = |profile: u32| vec![A::CGLPFAOpenGLProfile.0, profile, A::CGLPFAAccelerated.0, A::CGLPFADoubleBuffer.0, A::CGLPFAAllowOfflineRenderers.0];
        let float = [accelerated(0x3200), vec![A::CGLPFAColorSize.0, 64, A::CGLPFAColorFloat.0]].concat();
        let any = vec![A::CGLPFAOpenGLProfile.0, 0x3200, A::CGLPFADoubleBuffer.0];
        let software = vec![A::CGLPFAOpenGLProfile.0, 0x3200, A::CGLPFARendererID.0, objc2_open_gl::kCGLRendererGenericFloatID, A::CGLPFADoubleBuffer.0];
        let candidates = [("float, accelerated", float, true), ("8-bit, accelerated", accelerated(0x3200), false),
            ("8-bit, legacy profile", accelerated(0x1000), false), ("8-bit, any renderer", any, false), ("software", software, false)];
        let mut tried = Vec::new();
        for (label, attrs, is_float) in candidates {
            let mut attrs: Vec<A> = attrs.into_iter().map(A).collect();
            attrs.push(A(0));
            let (mut format, mut count): (CGLPixelFormatObj, i32) = (null_mut(), 0);
            let err = unsafe { CGLChoosePixelFormat(NonNull::new_unchecked(attrs.as_mut_ptr()), NonNull::from(&mut format), NonNull::from(&mut count)) };
            if format.is_null() { tried.push(format!("{label}: pixel format error {}", err.0)); continue; }
            let mut context: CGLContextObj = null_mut();
            let err = unsafe { CGLCreateContext(format, null_mut(), NonNull::from(&mut context)) };
            if context.is_null() {
                unsafe { CGLReleasePixelFormat(format); }
                tried.push(format!("{label}: context error {}", err.0));
                continue;
            }
            *gl = Some(Gl { format, context, float: is_float, label });
            break;
        }
        if gl.is_none() { eprintln!("veylo: no OpenGL context ({})", tried.join("; ")); }
    }
    gl.as_ref().map(|g| (g.format, g.context, g.float))
}

/// Self-test: the GL context the picture would use, and mpv's renderer on it.
pub fn check(mpv: &libmpv2::Mpv) -> Result<String, String> {
    gl().ok_or("no OpenGL context")?;
    let label = GL.lock().ok().and_then(|g| g.as_ref().map(|g| g.label)).unwrap_or("?");
    let render = unsafe { create_render_for(mpv.ctx.as_ptr())? };
    unsafe { with_context(|| sys::mpv_render_context_free(render)); }
    Ok(format!("OpenGL {label}; mpv's renderer starts on it"))
}

unsafe extern "C" fn proc_address(_: *mut c_void, name: *const c_char) -> *mut c_void { libc::dlsym(libc::RTLD_DEFAULT, name) }
unsafe extern "C" fn on_update(_: *mut c_void) { PENDING.store(true, Ordering::Release); }

/// Run `f` with our GL context locked and current (main thread), restoring whatever was current.
unsafe fn with_context<T>(f: impl FnOnce() -> T) -> Option<T> {
    let (_, context, _) = gl()?;
    CGLLockContext(context);
    let previous = CGLGetCurrentContext();
    CGLSetCurrentContext(context);
    let out = f();
    CGLSetCurrentContext(previous);
    CGLUnlockContext(context);
    Some(out)
}

fn wants_frame() -> bool {
    let Ok(session) = RENDER.lock() else { return false };
    let Some(s) = session.as_ref() else { return DIRTY.swap(false, Ordering::AcqRel) };
    let dirty = DIRTY.swap(false, Ordering::AcqRel);
    let frame = PENDING.swap(false, Ordering::AcqRel)
        && unsafe { sys::mpv_render_context_update(s.render) } & sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME as u64 != 0;
    dirty || frame
}

unsafe fn draw_frame() -> bool {
    let (mut fbo, mut viewport) = (0i32, [0i32; 4]);
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &mut fbo);
    glGetIntegerv(GL_VIEWPORT, viewport.as_mut_ptr());
    let float = gl().is_some_and(|g| g.2);
    let session = RENDER.lock();
    match session.as_ref().ok().and_then(|s| s.as_ref()) {
        Some(s) => {
            let mut target = sys::mpv_opengl_fbo { fbo, w: viewport[2], h: viewport[3], internal_format: 0 };
            let (mut flip, mut depth): (i32, i32) = (1, if float { 16 } else { 8 });
            let mut params = [
                sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO, data: &mut target as *mut _ as *mut c_void },
                sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y, data: &mut flip as *mut _ as *mut c_void },
                sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_DEPTH, data: &mut depth as *mut _ as *mut c_void },
                sys::mpv_render_param { type_: 0, data: null_mut() },
            ];
            sys::mpv_render_context_render(s.render, params.as_mut_ptr());
            true
        }
        None => { glClearColor(0.0, 0.0, 0.0, 1.0); glClear(GL_COLOR_BUFFER_BIT); false }
    }
}

/// mpv's renderer on our context; it must exist before mpv opens the file (vo=libmpv).
unsafe fn create_render(mpv: &libmpv2::Mpv) -> Result<*mut sys::mpv_render_context, String> { create_render_for(mpv.ctx.as_ptr()) }

unsafe fn create_render_for(mpv: *mut sys::mpv_handle) -> Result<*mut sys::mpv_render_context, String> {
    with_context(|| {
        let mut init = sys::mpv_opengl_init_params { get_proc_address: Some(proc_address), get_proc_address_ctx: null_mut() };
        let mut params = [
            sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE, data: sys::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void },
            sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS, data: &mut init as *mut _ as *mut c_void },
            sys::mpv_render_param { type_: 0, data: null_mut() },
        ];
        let mut render: *mut sys::mpv_render_context = null_mut();
        let err = sys::mpv_render_context_create(&mut render, mpv, params.as_mut_ptr());
        if err < 0 { return Err(format!("mpv renderer: error {err}")); }
        sys::mpv_render_context_set_update_callback(render, Some(on_update), null_mut());
        Ok(render)
    }).unwrap_or_else(|| Err("no OpenGL context".into()))
}

/// The video view under the WebView, created once per window.
fn view(mtm: MainThreadMarker) -> Option<Retained<NSView>> {
    if let Some(v) = VIEW.with(|v| v.borrow().clone()) { return Some(v); }
    let window = window()?;
    let content = window.contentView()?;
    let layer: Retained<VideoLayer> = unsafe { msg_send![VideoLayer::class(), new] };
    layer.setAsynchronous(false);
    layer.setNeedsDisplayOnBoundsChange(true);
    layer.setContentsScale(window.backingScaleFactor());
    layer.setBackgroundColor(Some(&CGColor::new_generic_gray(0.0, 1.0)));
    if gl().is_some_and(|g| g.2) { layer.setContentsFormat(unsafe { objc2_quartz_core::kCAContentsFormatRGBA16Float }); }
    let view = NSView::initWithFrame(NSView::alloc(mtm), content.bounds());
    view.setLayer(Some(&layer));
    view.setWantsLayer(true);
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    // Below every other subview: the WebView stays on top and keeps every click.
    content.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, None);
    view.setHidden(true);
    *LAYER.lock().ok()? = Some(SendLayer(layer));
    VIEW.with(|v| *v.borrow_mut() = Some(view.clone()));
    Some(view)
}

/// The app's window, from the last [`Host`] the app gave us (AppKit objects: main thread only).
static WINDOW: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
fn remember(host: &Host) { WINDOW.store(host.ns_window, Ordering::Release); }

fn window() -> Option<Retained<NSWindow>> {
    let ptr = WINDOW.load(Ordering::Acquire);
    // SAFETY: the app's NSWindow pointer is valid while the window lives; retain our own reference.
    unsafe { Retained::retain(ptr as *mut NSWindow) }
}

/// The window's screen: its EDR headroom now, and the most it can reach.
fn read_headroom() {
    let screen = window().and_then(|w| w.screen());
    let (now, potential) = headroom_of(screen.as_deref());
    HEADROOM.store(now.to_bits(), Ordering::Release);
    POTENTIAL.store(potential.to_bits(), Ordering::Release);
}

/// A screen's EDR headroom (now, most). Developer option: VEYLO_DX_EDR=<headroom> pretends the
/// screen has that much (CI's Macs have no HDR screen), to run the HDR path anywhere.
fn headroom_of(screen: Option<&NSScreen>) -> (f64, f64) {
    if let Some(fake) = std::env::var("VEYLO_DX_EDR").ok().and_then(|v| v.parse::<f64>().ok()) { return (fake, fake); }
    screen.map_or((1.0, 1.0), |s| (s.maximumExtendedDynamicRangeColorComponentValue(), s.maximumPotentialExtendedDynamicRangeColorComponentValue()))
}
/// The most EDR headroom the window's screen can reach (1.0: SDR only; 0: not read yet). Read fresh
/// on the main thread (`host` is only asked for there); elsewhere what the last read found.
pub fn potential_headroom(host: impl FnOnce() -> Option<Host>) -> f64 {
    if MainThreadMarker::new().is_some() {
        if let Some(host) = host() { remember(&host); }
        read_headroom();
    }
    f64::from_bits(POTENTIAL.load(Ordering::Acquire))
}

pub fn candidates() -> &'static [Path] { crate::fallback::order("macos", false) }

pub fn open(_path: Path, host: &Host, player: &Arc<dyn Player>) -> Result<(), String> {
    free_render();
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    remember(host);
    let view = view(mtm).ok_or("no window to put the video layer in")?;
    read_headroom();
    let render = unsafe { create_render(player.mpv()) }.map_err(|e| format!("video layer: {e}"))?;
    if let Ok(mut s) = RENDER.lock() { *s = Some(Session { _player: player.clone(), render }); }
    HDR_MODE.store(0, Ordering::Release);
    if let Ok(layer) = LAYER.lock() {
        if let Some(l) = layer.as_ref() {
            if let Some(w) = window() { l.0.setContentsScale(w.backingScaleFactor()); }
            set_edr(&l.0, None);
            l.0.setAsynchronous(true);
        }
    }
    DIRTY.store(true, Ordering::Release);
    view.setHidden(false);
    Ok(())
}

/// Free mpv's renderer (before mpv itself, which our Session keeps alive until then).
fn free_render() {
    let Some(session) = RENDER.lock().ok().and_then(|mut s| s.take()) else { return };
    unsafe {
        with_context(|| {
            sys::mpv_render_context_set_update_callback(session.render, None, null_mut());
            sys::mpv_render_context_free(session.render);
        });
    }
    // Dropping mpv's core waits for its threads: never block the AppKit main thread.
    let player = session._player;
    std::thread::spawn(move || drop(player));
}

pub fn close(_path: Path) {
    if let Ok(layer) = LAYER.lock() {
        if let Some(l) = layer.as_ref() {
            l.0.setAsynchronous(false);
        }
    }
    free_render();
    VIEW.with(|v| if let Some(v) = v.borrow().as_ref() { v.setHidden(true); });
    DIRTY.store(true, Ordering::Release);
}

/// The layer's colour space and EDR, in one explicit transaction (safe from any thread).
fn set_edr(layer: &VideoLayer, pq: Option<&objc2_core_foundation::CFString>) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    layer.setWantsExtendedDynamicRangeContent(pq.is_some());
    let space = pq.and_then(|name| CGColorSpace::with_name(Some(name)));
    layer.setColorspace(space.as_deref());
    CATransaction::commit();
}

/// The video's format is known (or changed): HDR video on an EDR screen goes out as PQ.
pub fn layout(player: &dyn Player, hdr_output: bool) {
    let mpv = player.mpv();
    let primaries: String = mpv.get_property("video-params/primaries").unwrap_or_default();
    let gamma: String = mpv.get_property("video-params/gamma").unwrap_or_default();
    let headroom = f64::from_bits(HEADROOM.load(Ordering::Acquire)).max(f64::from_bits(POTENTIAL.load(Ordering::Acquire)));
    // PQ code values fit any buffer; a 16-bit float one (when the GPU gives it) only bands less.
    let mode = match (gamma.as_str(), primaries.as_str()) {
        _ if headroom <= 1.0 || !hdr_output => 0,
        ("pq" | "hlg", "bt.2020") => 1,
        ("pq" | "hlg", "display-p3") => 2,
        _ => 0,
    };
    if HDR_MODE.swap(mode, Ordering::AcqRel) == mode { return; }
    let Ok(layer) = LAYER.lock() else { return };
    let Some(layer) = layer.as_ref() else { return };
    if mode == 0 {
        eprintln!("veylo: HDR output off ({gamma}/{primaries} video, EDR headroom {headroom:.1})");
        set_edr(&layer.0, None);
        for (k, v) in [("target-trc", "auto"), ("target-prim", "auto"), ("target-peak", "auto")] { let _ = mpv.set_property(k, v); }
        return;
    }
    // SAFETY: CoreGraphics' colour space names are immutable statics.
    let name = unsafe { if mode == 1 { objc2_core_graphics::kCGColorSpaceITUR_2100_PQ } else { objc2_core_graphics::kCGColorSpaceDisplayP3_PQ } };
    set_edr(&layer.0, Some(name));
    // EDR 1.0 is SDR white (203 nits by BT.2408); mpv tone-maps what the screen can't reach.
    let peak = (headroom * 203.0).round().clamp(203.0, 10000.0) as i64;
    let _ = mpv.set_property("icc-profile", "");
    let _ = mpv.set_property("target-prim", primaries.as_str());
    let _ = mpv.set_property("target-trc", "pq");
    let _ = mpv.set_property("target-peak", peak);
    eprintln!("veylo: HDR output on: PQ {primaries} to an EDR layer, headroom {headroom:.1}, peak {peak} nits");
}

/// Self-test: mpv renders an HDR10 clip through this module's GL context into a 16-bit float
/// target (what the layer shows), once as PQ (what an EDR screen gets) and once tone-mapped to SDR:
/// the PQ picture must keep the clip's code values, the SDR one must differ. The main screen's EDR
/// headroom says whether this Mac can show HDR. Runs without a window; call on the main thread.
pub fn hdr_check(clip: &str) -> Result<String, String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    let _app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    let (now, potential) = headroom_of(objc2_app_kit::NSScreen::mainScreen(mtm).as_deref());
    let (_, _, float) = gl().ok_or("no OpenGL context")?;
    let label = GL.lock().ok().and_then(|g| g.as_ref().map(|g| g.label)).unwrap_or("?");
    let pq = render_pixel(clip, &[("target-prim", "bt.2020"), ("target-trc", "pq"), ("target-peak", "1000")])?;
    let sdr = render_pixel(clip, &[("target-prim", "bt.709"), ("target-trc", "bt.1886")])?;
    // A float target keeps a hair of rounding either side of 0..1.
    let ok = |p: &[f32; 4]| p[..3].iter().all(|c| c.is_finite() && (-0.01..=1.01).contains(c)) && p[..3].iter().any(|c| *c > 0.05);
    if !ok(&pq) || !ok(&sdr) { return Err(format!("the rendered pictures look wrong (PQ {pq:?}, SDR {sdr:?})")); }
    let diff = pq[..3].iter().zip(&sdr[..3]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    if diff < 0.05 { return Err(format!("PQ and SDR output are the same picture ({pq:?}): mpv didn't send PQ")); }
    Ok(format!("HDR10 clip rendered as PQ BT.2020 (pixel {:.2}/{:.2}/{:.2}; SDR {:.2}/{:.2}/{:.2}) on the {label} context, {} target; screen EDR headroom {now:.1} (up to {potential:.1}): {}",
        pq[0], pq[1], pq[2], sdr[0], sdr[1], sdr[2], if float { "float" } else { "8-bit" },
        if potential > 1.0 { "HDR shows as HDR on this screen" } else { "this screen is SDR only, HDR is tone-mapped" }))
}

/// Play `clip` with mpv rendering into an offscreen RGBA16F target on our context; one pixel of the fifth frame.
fn render_pixel(clip: &str, target: &[(&str, &str)]) -> Result<[f32; 4], String> {
    let mpv = libmpv2::Mpv::with_initializer(|o| {
        for (k, v) in [("config", "no"), ("terminal", "no"), ("vo", "libmpv"), ("ao", "null"), ("keep-open", "yes"), ("hwdec", "no"),
            ("icc-profile", ""), ("dither-depth", "no")].iter().chain(target) { o.set_property(k, *v)?; }
        Ok(())
    }).map_err(|e| format!("mpv: {e}"))?;
    let render = unsafe { create_render_for(mpv.ctx.as_ptr())? };
    let (w, h) = (320, 240);
    let (mut fbo, mut tex) = (0u32, 0u32);
    let complete = unsafe { with_context(|| {
        glGenTextures(1, &mut tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA16F, w, h, 0, GL_RGBA, GL_FLOAT, std::ptr::null());
        glGenFramebuffers(1, &mut fbo);
        glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
        glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE
    }) };
    let result = (|| {
        if complete != Some(true) { return Err("no 16-bit float render target".to_owned()); }
        mpv.command("loadfile", &[clip]).map_err(|e| format!("loadfile: {e}"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let (mut frames, mut pixel) = (0, [0f32; 4]);
        while frames < 5 {
            if std::time::Instant::now() > deadline { return Err(format!("only {frames} frames rendered in 20 s")); }
            let _ = mpv.wait_event(0.02);
            unsafe { with_context(|| {
                if sys::mpv_render_context_update(render) & sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME as u64 == 0 { return; }
                let mut target = sys::mpv_opengl_fbo { fbo: fbo as i32, w, h, internal_format: GL_RGBA16F };
                let (mut flip, mut depth) = (0i32, 16i32);
                let mut params = [
                    sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO, data: &mut target as *mut _ as *mut c_void },
                    sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y, data: &mut flip as *mut _ as *mut c_void },
                    sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_DEPTH, data: &mut depth as *mut _ as *mut c_void },
                    sys::mpv_render_param { type_: 0, data: null_mut() },
                ];
                sys::mpv_render_context_render(render, params.as_mut_ptr());
                glBindFramebuffer(GL_FRAMEBUFFER, fbo);
                glReadPixels(w / 4, h / 2, 1, 1, GL_RGBA, GL_FLOAT, pixel.as_mut_ptr().cast());
                frames += 1;
            }); }
        }
        let source: String = mpv.get_property("video-params/gamma").unwrap_or_default();
        if source != "pq" { return Err(format!("the HDR clip read as {source}, not PQ")); }
        Ok(pixel)
    })();
    unsafe { with_context(|| { sys::mpv_render_context_free(render); glDeleteFramebuffers(1, &fbo); glDeleteTextures(1, &tex); }); }
    result
}

/// What goes to the screen, for the player's details (mpv's own report is empty with the render API).
pub fn sent_transfer() -> &'static str { if HDR_MODE.load(Ordering::Acquire) != 0 { "pq" } else { "bt.1886" } }
