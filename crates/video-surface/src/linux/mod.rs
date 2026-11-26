//! Linux: the picture in a GtkGLArea under the web view (Wayland and X11).
use crate::{Host, Path, Player};
use std::sync::Arc;
use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use libmpv2_sys as sys;
use std::ffi::{c_char, c_void, CStr};

fn wayland() -> bool { gtk::gdk::Display::default().is_some_and(|d| d.type_().name().contains("Wayland")) }

pub fn candidates() -> &'static [Path] { crate::fallback::order("linux", wayland()) }

pub fn open(path: Path, host: &Host, player: &Arc<dyn Player>) -> Result<(), String> {
    match path {
        Path::GlArea => glarea::open(host, player),
        _ => Err("not a Linux path".into()),
    }
}

pub fn close(path: Path) {
    if path == Path::GlArea {
        glarea::close();
    }
}

pub fn layout(_player: &dyn Player, _hdr_output: bool) {}

/// A GDK symbol by name (the backend-specific getters are not in gtk-rs).
unsafe fn gdk_fn(name: &CStr) -> *mut c_void { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }

/// A looked-up symbol as the function-pointer type its use site names.
unsafe fn symbol<F: Copy>(p: *mut c_void) -> F {
    assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>(), "a symbol is one pointer");
    std::mem::transmute_copy(&p)
}

// ------------------------------------------------------------ GL entry points

struct Gl {
    lookup: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    clear_color: unsafe extern "C" fn(f32, f32, f32, f32),
    clear: unsafe extern "C" fn(u32),
    get_integerv: unsafe extern "C" fn(u32, *mut i32),
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
}

/// EGL on Wayland, GLX (or EGL) on X11; mpv resolves everything through the same lookup.
fn gl() -> Option<&'static Gl> {
    static GL: std::sync::OnceLock<Option<Gl>> = std::sync::OnceLock::new();
    GL.get_or_init(|| unsafe {
        let candidates: &[(&CStr, &CStr)] = if wayland() { &[(c"libEGL.so.1", c"eglGetProcAddress")] }
            else { &[(c"libGL.so.1", c"glXGetProcAddressARB"), (c"libGLX.so.0", c"glXGetProcAddressARB"), (c"libEGL.so.1", c"eglGetProcAddress")] };
        let lookup = candidates.iter().find_map(|(lib, sym)| {
            let existing = libc::dlsym(libc::RTLD_DEFAULT, sym.as_ptr());
            if !existing.is_null() {
                return Some(std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*const c_char) -> *mut c_void>(existing));
            }
            let h = libc::dlopen(lib.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
            if h.is_null() { return None; }
            let f = libc::dlsym(h, sym.as_ptr());
            (!f.is_null()).then(|| std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*const c_char) -> *mut c_void>(f))
        })?;
        let get = |name: &CStr| {
            let p = lookup(name.as_ptr());
            if !p.is_null() { return Some(p); }
            let d = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
            (!d.is_null()).then_some(d)
        };
        Some(Gl {
            lookup,
            clear_color: symbol(get(c"glClearColor")?),
            clear: symbol(get(c"glClear")?),
            get_integerv: symbol(get(c"glGetIntegerv")?),
            bind_framebuffer: symbol(get(c"glBindFramebuffer")?),
        })
    }).as_ref()
}

unsafe extern "C" fn proc_address(_: *mut c_void, name: *const c_char) -> *mut c_void {
    if let Some(gl) = gl() {
        let p = (gl.lookup)(name);
        if !p.is_null() { return p; }
    }
    libc::dlsym(libc::RTLD_DEFAULT, name)
}

/// mpv's renderer for the current GL context; `native` lets hardware decoders hand frames to GL
/// without a copy (VAAPI over DMA-BUF).
unsafe fn create_render(player: &dyn Player, native: Option<(u32, *mut c_void)>,
                        on_update: unsafe extern "C" fn(*mut c_void), cb: *mut c_void) -> Result<*mut sys::mpv_render_context, String> {
    gl().ok_or("no OpenGL entry points")?;
    let mut init = sys::mpv_opengl_init_params { get_proc_address: Some(proc_address), get_proc_address_ctx: std::ptr::null_mut() };
    let mut params = vec![
        sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE, data: sys::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void },
        sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS, data: &mut init as *mut _ as *mut c_void },
    ];
    if let Some((kind, ptr)) = native.filter(|(_, p)| !p.is_null()) { params.push(sys::mpv_render_param { type_: kind, data: ptr }); }
    params.push(sys::mpv_render_param { type_: 0, data: std::ptr::null_mut() });
    let mut render: *mut sys::mpv_render_context = std::ptr::null_mut();
    let err = sys::mpv_render_context_create(&mut render, player.mpv().ctx.as_ptr(), params.as_mut_ptr());
    if err < 0 { return Err(format!("mpv renderer: error {err}")); }
    sys::mpv_render_context_set_update_callback(render, Some(on_update), cb);
    Ok(render)
}

unsafe fn render_frame(render: *mut sys::mpv_render_context, fbo: i32, w: i32, h: i32) {
    let mut target = sys::mpv_opengl_fbo { fbo, w, h, internal_format: 0 };
    let mut flip: i32 = 1;
    let mut block: i32 = i32::from(fbo == 0);
    let mut params = [
        sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO, data: &mut target as *mut _ as *mut c_void },
        sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y, data: &mut flip as *mut _ as *mut c_void },
        sys::mpv_render_param { type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME, data: &mut block as *mut _ as *mut c_void },
        sys::mpv_render_param { type_: 0, data: std::ptr::null_mut() },
    ];
    let err = sys::mpv_render_context_render(render, params.as_mut_ptr());
    if fbo != 0 {
        if let Some(gl) = gl() {
            (gl.bind_framebuffer)(0x8D40, fbo as u32); // GL_FRAMEBUFFER: mpv resets to 0 on exit
        }
    }
    if err < 0 {
        eprintln!("veylo: mpv_render_context_render error {err} (fbo={fbo}, {w}x{h})");
    }
}

// ------------------------------------------------------------ GtkGLArea under the WebView

mod glarea {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Video {
        overlay: gtk::Overlay,
        area: gtk::GLArea,
        /// The WebKit view over it, repainted with every frame (it has its own GDK window).
        web: gtk::Widget,
        render: *mut sys::mpv_render_context,
        /// Keeps the mpv core alive until the render context is freed (mpv requires that order).
        player: Option<Arc<dyn Player>>,
    }
    thread_local! { static VIDEO: RefCell<Option<Video>> = const { RefCell::new(None) }; }
    static REDRAW_QUEUED: AtomicBool = AtomicBool::new(false);

    /// Put the GL area under the web view, once: the web view sits in an overlay above it.
    fn install(host: &Host) -> Result<(), String> {
        if VIDEO.with(|v| v.borrow().is_some()) { return Ok(()); }
        let web = host.web.clone();
        let (overlay, area) = if let Some(overlay) = web.parent().and_then(|p| p.downcast::<gtk::Overlay>().ok()) {
            overlay.style_context().add_class("rstream-black-bg");
            let area = if let Some(existing) = overlay.child().and_then(|c| c.downcast::<gtk::GLArea>().ok()) {
                existing
            } else {
                if let Some(old) = overlay.child() {
                    overlay.remove(&old);
                }
                let area = gtk::GLArea::new();
                area.set_auto_render(false);
                area.set_has_alpha(false);
                area.set_hexpand(true);
                area.set_vexpand(true);
                overlay.add(&area);
                area.show();
                area
            };
            (overlay, area)
        } else {
            let vbox = host.vbox.clone().ok_or("no box holding the web view")?;
            let area = gtk::GLArea::new();
            area.set_auto_render(false);
            area.set_has_alpha(false);
            area.set_hexpand(true);
            area.set_vexpand(true);
            let overlay = gtk::Overlay::new();
            overlay.style_context().add_class("rstream-black-bg");
            overlay.set_hexpand(true);
            overlay.set_vexpand(true);
            web.set_hexpand(true);
            web.set_vexpand(true);
            vbox.remove(&web);
            if let Some(rgba) = gtk::gdk::Screen::default().and_then(|s| s.rgba_visual()) {
                web.set_visual(Some(&rgba));
            }
            overlay.add(&area);
            overlay.add_overlay(&web);
            vbox.pack_start(&overlay, true, true, 0);
            overlay.show_all();
            (overlay, area)
        };
        area.connect_render(|area, _| { draw(area); gtk::glib::Propagation::Stop });
        area.connect_unrealize(|_| free_render());
        VIDEO.with(|v| *v.borrow_mut() = Some(Video { overlay, area, web, render: std::ptr::null_mut(), player: None }));
        Ok(())
    }

    pub fn open(host: &Host, player: &Arc<dyn Player>) -> Result<(), String> {
        install(host)?;
        free_render();
        VIDEO.with(|v| {
            let mut v = v.borrow_mut();
            let v = v.as_mut().ok_or("no GL area")?;
            // Just put in place: realize it now, which creates its GL context.
            v.area.realize();
            v.area.make_current();
            if let Some(e) = v.area.error() { return Err(format!("GL unavailable: {e}")); }
            // The display mpv's hardware decoders share frames on: X11's, or Wayland's when this is
            // the Wayland fallback (never ask a Wayland display for its X11 one).
            let (getter, kind) = if wayland() {
                (c"gdk_wayland_display_get_wl_display", sys::mpv_render_param_type_MPV_RENDER_PARAM_WL_DISPLAY)
            } else {
                (c"gdk_x11_display_get_xdisplay", sys::mpv_render_param_type_MPV_RENDER_PARAM_X11_DISPLAY)
            };
            let native = gtk::gdk::Display::default().map(|d| unsafe {
                let f = gdk_fn(getter);
                let p = if f.is_null() { std::ptr::null_mut() } else {
                    let f: unsafe extern "C" fn(*mut gtk::gdk::ffi::GdkDisplay) -> *mut c_void = std::mem::transmute(f);
                    f(d.to_glib_none().0)
                };
                (kind, p)
            });
            let render = unsafe { create_render(&**player, native, on_update, std::ptr::null_mut()) }?;
            v.render = render;
            v.player = Some(player.clone());
            v.area.queue_render();
            v.overlay.queue_draw();
            v.web.queue_draw();
            Ok(())
        })
    }

    pub fn close() { free_render(); }

    fn free_render() {
        let player = VIDEO.with(|v| {
            let mut v = v.borrow_mut();
            let v = v.as_mut()?;
            if !v.render.is_null() {
                v.area.make_current();
                unsafe {
                    sys::mpv_render_context_set_update_callback(v.render, None, std::ptr::null_mut());
                    sys::mpv_render_context_free(v.render);
                }
                v.render = std::ptr::null_mut();
                v.area.queue_render();
                v.overlay.queue_draw();
                v.web.queue_draw();
            }
            v.player.take()
        });
        // Destroying the core waits for its threads: never on the UI thread.
        if let Some(player) = player { std::thread::spawn(move || drop(player)); }
    }

    /// From mpv's threads: a frame is due. Scheduled at `HIGH_IDLE` (priority 100, ahead of
    /// `GDK_PRIORITY_REDRAW` = 120) so `GtkGLArea`, `GtkOverlay`, and `WebKitWebView` always
    /// repaint in the same GDK frame-clock cycle.
    unsafe extern "C" fn on_update(_: *mut c_void) {
        if REDRAW_QUEUED.swap(true, Ordering::AcqRel) { return; }
        gtk::glib::source::idle_add_full(gtk::glib::Priority::HIGH_IDLE, || {
            REDRAW_QUEUED.store(false, Ordering::Release);
            VIDEO.with(|v| if let Ok(v) = v.try_borrow() {
                if let Some(v) = v.as_ref().filter(|v| !v.render.is_null()) {
                    v.area.queue_render();
                    v.overlay.queue_draw();
                    v.web.queue_draw();
                }
            });
            gtk::glib::ControlFlow::Break
        });
    }

    fn draw(area: &gtk::GLArea) {
        let Some(gl) = gl() else { return };
        VIDEO.with(|v| {
            let render = v.borrow().as_ref().map(|v| v.render).unwrap_or(std::ptr::null_mut());
            unsafe {
                (gl.clear_color)(0.0, 0.0, 0.0, 1.0);
                (gl.clear)(0x4000); // GL_COLOR_BUFFER_BIT
                if render.is_null() {
                    return;
                }
                let _ = sys::mpv_render_context_update(render);
                let mut fbo = 0;
                (gl.get_integerv)(0x8CA6, &mut fbo); // GL_DRAW_FRAMEBUFFER_BINDING: GtkGLArea's own
                let scale = area.scale_factor();
                let (w, h) = (area.allocated_width() * scale, area.allocated_height() * scale);
                render_frame(render, fbo, w, h);
                sys::mpv_render_context_report_swap(render);
            }
        });
    }
}
