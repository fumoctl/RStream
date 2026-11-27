//! JNI glue for Android: the app's private files folder, the Rust player's video surface
//! (RStreamVideo.kt), Back (RStreamBack.kt), and VPN transport monitor (RStreamVpn.kt).
use anyhow::{anyhow, Result};
use jni::{
    objects::{GlobalRef, JClass, JObject, JString, JValue},
    sys::{jboolean, jint, jlong},
    JNIEnv, JavaVM,
};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

fn with_activity<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<T>) -> Result<T> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| anyhow!("JNI: {e}"))?;
    let mut env = vm.attach_current_thread().map_err(|e| anyhow!("JNI attach: {e}"))?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
    env.with_local_frame(16, |env| {
        let result = f(env, &activity);
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        result
    })
    .map_err(|e| anyhow!("JNI: {e}"))
}

/// A system property ("ro.product.model"), None when unset.
pub fn property(name: &std::ffi::CStr) -> Option<String> {
    let mut value = [0 as libc::c_char; 92]; // PROP_VALUE_MAX
    let n = unsafe { libc::__system_property_get(name.as_ptr(), value.as_mut_ptr()) };
    (n > 0)
        .then(|| unsafe { std::ffi::CStr::from_ptr(value.as_ptr()) }.to_string_lossy().trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Whether this device is a television: TVs say so in ro.build.characteristics ("tv").
pub fn is_tv() -> bool {
    static TV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TV.get_or_init(|| property(c"ro.build.characteristics").is_some_and(|v| v.split(',').any(|c| c.trim() == "tv")))
}

static FILES_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn files_dir() -> Result<PathBuf> {
    if let Some(dir) = FILES_DIR.get() {
        return Ok(dir.clone());
    }
    let dir = with_activity(|env, activity| {
        let dir = env.call_method(activity, "getFilesDir", "()Ljava/io/File;", &[])?.l()?;
        let path = JString::from(env.call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])?.l()?);
        let path: String = env.get_string(&path)?.into();
        Ok(PathBuf::from(path))
    })?;
    let _ = FILES_DIR.set(dir.clone());
    Ok(dir)
}

/// ACTION_VIEW for an https link or local loopback OAuth redirect (`http://127.0.0.1:...`).
pub fn open_url(url: &str) -> Result<()> {
    let allowed = url.starts_with("https://")
        || url.starts_with("http://127.0.0.1:")
        || url.starts_with("http://localhost:");
    if !allowed {
        anyhow::bail!("Refusing to open untrusted URL scheme: {url}");
    }
    let err = with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamBack")?;
        let uri_str = env.new_string(url)?;
        let res = env
            .call_static_method(
                &class,
                "openUrl",
                "(Landroid/app/Activity;Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(activity), JValue::Object(&uri_str)],
            )?
            .l()?;
        if res.is_null() {
            Ok(None)
        } else {
            let msg: String = env.get_string(&JString::from(res))?.into();
            Ok(Some(msg))
        }
    })?;
    if let Some(msg) = err {
        log(&format!("open_url error: {msg}"));
        anyhow::bail!("Couldn't open browser: {msg}");
    }
    Ok(())
}

/// Initializes rustls-platform-verifier with the Android ApplicationContext and JVM before any TLS client runs.
pub fn tls_verifier_init() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let ctx = ndk_context::android_context();
        if ctx.vm().is_null() || ctx.context().is_null() {
            log("rustls-platform-verifier init skipped: missing Android context");
            return;
        }
        // SAFETY: `ctx.vm()` is the live process JavaVM registered by `android-activity`/`ndk_context`.
        let vm = unsafe { jni_022::JavaVM::from_raw(ctx.vm().cast()) };
        let res: std::result::Result<(), jni_022::errors::Error> = vm.attach_current_thread(|env| {
            // SAFETY: `ctx.context()` is a valid Android Context jobject from `ndk_context`.
            let raw_ctx = unsafe { jni_022::objects::JObject::from_raw(env, ctx.context().cast()) };
            let app_ctx = jni_022::strings::JNIStr::from_cstr(c"getApplicationContext")
                .zip(jni_022::signature::RuntimeMethodSignature::from_str("()Landroid/content/Context;").ok())
                .and_then(|(name, sig)| {
                    env.call_method(&raw_ctx, name, sig.method_signature(), &[])
                        .and_then(|v| v.l())
                        .ok()
                })
                .filter(|obj| !obj.is_null())
                .unwrap_or(raw_ctx);
            rustls_platform_verifier::android::init_with_env(env, app_ctx)
        });
        if let Err(e) = res {
            log(&format!("rustls-platform-verifier init failed: {e}"));
        }
    });
}

/// Stops the Android OAuth foreground keepalive service and optionally pops the Custom Tab back to RStream.
pub fn finish_oauth(bring_to_front: bool) {
    let _ = with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamBack")?;
        env.call_static_method(
            &class,
            "finishOAuth",
            "(Landroid/app/Activity;Z)V",
            &[JValue::Object(activity), JValue::Bool(bring_to_front.into())],
        )?;
        Ok(())
    });
}

// ---------------------------------------------------------------- Foreground Transfer & Sync Service

static TRANSFER_CLASS: OnceLock<(JavaVM, GlobalRef)> = OnceLock::new();

pub fn transfer_service_init() -> Result<()> {
    let _ = files_dir();
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamTransferService")?;
        if TRANSFER_CLASS.get().is_none() {
            if let (Ok(vm), Ok(global)) = (env.get_java_vm(), env.new_global_ref(&class)) {
                let _ = TRANSFER_CLASS.set((vm, global));
            }
        }
        env.call_static_method(
            &class,
            "init",
            "(Landroid/app/Activity;)V",
            &[JValue::Object(activity)],
        )?;
        Ok(())
    })
}

pub fn sync_transfer_service(act: &crate::backend::BackgroundActivity) {
    let Some((vm, class_ref)) = TRANSFER_CLASS.get() else { return };
    let Ok(mut env) = vm.attach_current_thread() else { return };
    let _ = env.with_local_frame(16, |env| -> jni::errors::Result<()> {
        let title = env.new_string(act.primary_title.as_deref().unwrap_or(""))?;
        let class = unsafe { JClass::from_raw(class_ref.as_obj().as_raw()) };
        let _ = env.call_static_method(
            &class,
            "sync",
            "(IIJJLjava/lang/String;Z)V",
            &[
                JValue::Int(act.running_downloads as i32),
                JValue::Int(act.queued_downloads as i32),
                JValue::Long(act.downloaded_bytes.min(i64::MAX as u64) as i64),
                JValue::Long(act.total_bytes.min(i64::MAX as u64) as i64),
                JValue::Object(&title),
                JValue::Bool(act.db_syncing.into()),
            ],
        );
        let _ = std::mem::ManuallyDrop::new(class);
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        Ok(())
    });
}

#[no_mangle]
pub extern "system" fn Java_dev_rstream_app_RStreamTransferService_nativeTransferAction(
    _env: JNIEnv,
    _class: JClass,
    code: jint,
) {
    match code {
        1 => crate::backend::pause_all_downloads(),
        2 => crate::backend::cancel_all_downloads(),
        _ => {}
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_rstream_app_RStreamTransferService_nativeAppLifecycle(
    _env: JNIEnv,
    _class: JClass,
    _resumed: jboolean,
) {
    crate::backend::trigger_profile_sync_now();
}

/// Status bar and navigation bar heights in dp (= CSS px in the WebView).
pub fn system_bar_insets() -> Result<(f32, f32)> {
    with_activity(|env, activity| {
        let res = env.call_method(activity, "getResources", "()Landroid/content/res/Resources;", &[])?.l()?;
        let metrics = env.call_method(&res, "getDisplayMetrics", "()Landroid/util/DisplayMetrics;", &[])?.l()?;
        let density = env.get_field(&metrics, "density", "F")?.f()?.max(0.5);
        let window = env.call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?.l()?;
        let decor = env.call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?.l()?;
        let insets = env.call_method(&decor, "getRootWindowInsets", "()Landroid/view/WindowInsets;", &[])?.l()?;
        if !insets.is_null() {
            let sdk = env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i()?;
            let (top, bottom) = if sdk >= 30 {
                let bars = env.call_static_method("android/view/WindowInsets$Type", "systemBars", "()I", &[])?.i()?;
                let i = env.call_method(&insets, "getInsetsIgnoringVisibility", "(I)Landroid/graphics/Insets;", &[JValue::Int(bars)])?.l()?;
                (env.get_field(&i, "top", "I")?.i()?, env.get_field(&i, "bottom", "I")?.i()?)
            } else {
                (env.call_method(&insets, "getStableInsetTop", "()I", &[])?.i()?, env.call_method(&insets, "getStableInsetBottom", "()I", &[])?.i()?)
            };
            return Ok((top as f32 / density, bottom as f32 / density));
        }
        let mut dp = |name: &str| -> jni::errors::Result<f32> {
            let (n, kind, pkg) = (env.new_string(name)?, env.new_string("dimen")?, env.new_string("android")?);
            let id = env.call_method(&res, "getIdentifier", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
                &[JValue::Object(&n), JValue::Object(&kind), JValue::Object(&pkg)])?.i()?;
            if id == 0 { return Ok(0.0); }
            Ok(env.call_method(&res, "getDimensionPixelSize", "(I)I", &[JValue::Int(id)])?.i()? as f32 / density)
        };
        Ok((dp("status_bar_height")?, dp("navigation_bar_height")?))
    })
}

fn load_class<'a>(env: &mut JNIEnv<'a>, activity: &JObject, name: &str) -> jni::errors::Result<JClass<'a>> {
    let loader = env.call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
    let name = env.new_string(name)?;
    let class = env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?.l()?;
    Ok(JClass::from(class))
}

/// While a page or Search is open, Back closes it (RStreamBack.kt) instead of leaving the app.
pub fn set_back_enabled(enabled: bool) -> Result<()> {
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamBack")?;
        env.call_static_method(&class, "setEnabled", "(Landroid/app/Activity;Z)V", &[JValue::Object(activity), JValue::Bool(enabled.into())])?;
        Ok(())
    })
}

/// Reads plain text from the Android system clipboard when WebView's navigator.clipboard is restricted.
pub fn read_clipboard() -> Option<String> {
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamBack")?;
        let res = env
            .call_static_method(
                &class,
                "readClipboard",
                "(Landroid/app/Activity;)Ljava/lang/String;",
                &[JValue::Object(activity)],
            )?
            .l()?;
        if res.is_null() {
            Ok(None)
        } else {
            let text: String = env.get_string(&JString::from(res))?.into();
            Ok(Some(text))
        }
    })
    .ok()
    .flatten()
}

// ---------------------------------------------------------------- VPN monitor (RStreamVpn.kt)

static VPN_STATE: Mutex<(bool, Option<String>)> = Mutex::new((false, None));

pub fn vpn_init() -> Result<()> {
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamVpn")?;
        env.call_static_method(&class, "init", "(Landroid/app/Activity;)V", &[JValue::Object(activity)])?;
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_rstream_app_RStreamVpn_nativeVpnChanged(
    mut env: JNIEnv,
    _class: JClass,
    active: jboolean,
    iface: JString,
    net_handle: jlong,
    addrs_csv: JString,
    dns_csv: JString,
) {
    let is_active = active != 0;
    let iface_name = if iface.is_null() {
        None
    } else {
        env.get_string(&iface).ok().map(String::from).filter(|s| !s.is_empty())
    };
    let link_addrs: Vec<String> = if addrs_csv.is_null() {
        Vec::new()
    } else {
        env.get_string(&addrs_csv)
            .ok()
            .map(String::from)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let dns_servers: Vec<std::net::IpAddr> = if dns_csv.is_null() {
        Vec::new()
    } else {
        env.get_string(&dns_csv)
            .ok()
            .map(String::from)
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse::<std::net::IpAddr>().ok())
            .collect()
    };
    if let Ok(mut slot) = VPN_STATE.lock() {
        *slot = (is_active, iface_name.clone());
    }
    let addr_count = link_addrs.len();
    let dns_count = dns_servers.len();
    stremio_torrent::set_android_vpn_state_with_addrs(
        is_active,
        iface_name.clone(),
        net_handle as u64,
        link_addrs,
        dns_servers,
    );
    log(&format!(
        "vpn state: active={is_active} iface={iface_name:?} handle={net_handle} addrs={addr_count} dns={dns_count}"
    ));
}

// ---------------------------------------------------------------- Rust player surface (RStreamVideo.kt)

static SURFACE: Mutex<Option<(GlobalRef, i32, i32)>> = Mutex::new(None);

fn video_call(name: &str, sig: &str, args: &[JValue]) -> Result<()> {
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamVideo")?;
        let mut all = vec![JValue::Object(activity)];
        all.extend_from_slice(args);
        env.call_static_method(&class, name, sig, &all)?;
        Ok(())
    })
}

pub fn video_show() -> Result<()> { video_call("show", "(Landroid/app/Activity;)V", &[]) }
pub fn video_hide() -> Result<()> { video_call("hide", "(Landroid/app/Activity;)V", &[]) }
pub fn video_layout(aspect: f64, fit: i32) -> Result<()> {
    video_call("layout", "(Landroid/app/Activity;DI)V", &[JValue::Double(aspect), JValue::Int(fit)])
}
pub fn video_frame_rate(fps: f64) -> Result<()> {
    video_call("setFrameRate", "(Landroid/app/Activity;D)V", &[JValue::Double(fps)])
}
pub fn video_media_meta(title: &str, subtitle: &str) -> Result<()> {
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamVideo")?;
        let t = env.new_string(title)?;
        let s = env.new_string(subtitle)?;
        env.call_static_method(&class, "setMediaMetadata", "(Landroid/app/Activity;Ljava/lang/String;Ljava/lang/String;)V",
            &[JValue::Object(activity), JValue::Object(&t), JValue::Object(&s)])?;
        Ok(())
    })
}
pub fn video_media_state(playing: bool, position_ms: i64, speed: f32) -> Result<()> {
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamVideo")?;
        env.call_static_method(&class, "setMediaState", "(ZJF)V",
            &[JValue::Bool(playing.into()), JValue::Long(position_ms), JValue::Float(speed)])?;
        Ok(())
    })
}
pub fn video_get_brightness() -> f32 {
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamVideo")?;
        env.call_static_method(&class, "getBrightness", "(Landroid/app/Activity;)F", &[JValue::Object(activity)])?.f()
    }).unwrap_or(0.5)
}
pub fn video_set_brightness(level: f32) -> Result<()> {
    video_call("setBrightness", "(Landroid/app/Activity;F)V", &[JValue::Float(level)])
}
pub fn video_get_volume() -> f32 {
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamVideo")?;
        env.call_static_method(&class, "getVolume", "(Landroid/app/Activity;)F", &[JValue::Object(activity)])?.f()
    }).unwrap_or(0.5)
}
pub fn video_set_volume(level: f32) -> Result<()> {
    video_call("setVolume", "(Landroid/app/Activity;F)V", &[JValue::Float(level)])
}
pub fn video_set_rotation_lock(locked: bool) -> Result<()> {
    video_call("setRotationLock", "(Landroid/app/Activity;Z)V", &[JValue::Bool(locked.into())])
}
pub fn video_enter_pip(w: i32, h: i32) -> bool {
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamVideo")?;
        env.call_static_method(&class, "enterPip", "(Landroid/app/Activity;II)Z",
            &[JValue::Object(activity), JValue::Int(w), JValue::Int(h)])?.z()
    }).unwrap_or(false)
}

/// What the panel declares (Display.HdrCapabilities).
pub fn screen() -> Result<crate::engine::Screen> {
    let mask = with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamVideo")?;
        env.call_static_method(&class, "hdrTypes", "(Landroid/app/Activity;)I", &[JValue::Object(activity)])?.i()
    })?;
    Ok(crate::engine::Screen { hdr10: mask & 1 != 0, hlg: mask & 2 != 0, hdr10_plus: mask & 4 != 0, dolby_vision: mask & 8 != 0, peak_nits: 0 })
}

/// Before mpv starts: FFmpeg's MediaCodec decoders need the Java VM and the app context,
/// and TLS needs the CA bundle. Returns the bundle's path.
pub fn prepare_mpv() -> Result<String> {
    static JVM: std::sync::Once = std::sync::Once::new();
    JVM.call_once(|| unsafe {
        type SetVm = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> i32;
        let lib = libc::dlopen(c"libavcodec.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() { return; }
        let set_vm = libc::dlsym(lib, c"av_jni_set_java_vm".as_ptr());
        let set_ctx = libc::dlsym(lib, c"av_jni_set_android_app_ctx".as_ptr());
        if !set_vm.is_null() {
            let set_vm: SetVm = std::mem::transmute(set_vm);
            set_vm(ndk_context::android_context().vm(), std::ptr::null_mut());
        }
        if !set_ctx.is_null() {
            let app = with_activity(|env, activity| {
                let app = env.call_method(activity, "getApplicationContext", "()Landroid/content/Context;", &[])?.l()?;
                env.new_global_ref(app)
            });
            if let Ok(app) = app {
                let set_ctx: SetVm = std::mem::transmute(set_ctx);
                set_ctx(app.as_obj().as_raw().cast(), std::ptr::null_mut());
                std::mem::forget(app);
            }
        }
    });
    with_activity(|env, activity| {
        let class = load_class(env, activity, "dev.rstream.app.RStreamVideo")?;
        let path = env.call_static_method(&class, "prepare", "(Landroid/app/Activity;)Ljava/lang/String;", &[JValue::Object(activity)])?.l()?;
        let path: String = env.get_string(&JString::from(path))?.into();
        Ok(path)
    })
}

pub fn attach_surface(live: &crate::engine::Live) {
    if let Ok(slot) = SURFACE.lock() {
        if let Some((surface, w, h)) = slot.as_ref() { give_surface(live, surface.as_obj().as_raw() as i64, *w, *h); }
    }
}

fn give_surface(live: &crate::engine::Live, wid: i64, w: i32, h: i32) {
    let mpv = &live.mpv;
    let _ = mpv.set_property("wid", wid);
    if w > 0 && h > 0 { let _ = mpv.set_property("android-surface-size", format!("{w}x{h}").as_str()); }
    let _ = mpv.set_property("force-window", "yes");
    let _ = mpv.set_property("vo", if live.state().direct { "mediacodec_embed" } else { "gpu-next" });
    live.mark_ready();
}

#[no_mangle]
pub extern "system" fn Java_dev_rstream_app_RStreamVideo_nativeSurface(env: JNIEnv, _class: JClass, surface: JObject, width: jint, height: jint) {
    let Ok(mut slot) = SURFACE.lock() else { return };
    let live = crate::engine::active();
    if surface.is_null() {
        if let Some(live) = &live {
            let _ = live.mpv.set_property("pause", true);
            let _ = live.mpv.set_property("vo", "null");
            let _ = live.mpv.set_property("force-window", "no");
            let _ = live.mpv.set_property("wid", 0i64);
        }
        *slot = None;
        return;
    }
    let same = slot.as_ref().is_some_and(|(g, _, _)| env.is_same_object(g.as_obj(), &surface).unwrap_or(false));
    if same {
        if let Some((_, w, h)) = slot.as_mut() { *w = width; *h = height; }
        if let Some(live) = &live { let _ = live.mpv.set_property("android-surface-size", format!("{width}x{height}").as_str()); }
        return;
    }
    let Ok(global) = env.new_global_ref(&surface) else { return };
    let wid = global.as_obj().as_raw() as i64;
    *slot = Some((global, width, height));
    if let Some(live) = &live { give_surface(live, wid, width, height); }
}

#[no_mangle]
pub extern "system" fn Java_dev_rstream_app_RStreamVideo_nativeCommand(mut env: JNIEnv, _class: JClass, cmd: JString) {
    let Ok(cmd_str) = env.get_string(&cmd) else { return };
    let cmd: String = cmd_str.into();
    let Some(live) = crate::engine::active() else { return };
    match cmd.as_str() {
        "play" => live.set_paused(false),
        "pause" => live.set_paused(true),
        "toggle" => live.toggle_pause(),
        "seek_fwd" => live.seek_by(10.0),
        "seek_back" => live.seek_by(-10.0),
        "next" if live.request.has_next => live.finish(crate::engine::Finish::Next),
        "stop" => live.finish(crate::engine::Finish::Close),
        _ => {}
    }
}

/// One line in logcat under "rstream" and files/dev-log.txt.
pub fn log(message: &str) {
    extern "C" { fn __android_log_write(prio: i32, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> i32; }
    if let Ok(text) = std::ffi::CString::new(message) { unsafe { __android_log_write(4, c"rstream".as_ptr(), text.as_ptr()); } }
    static FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let path = match FILE.get() {
        Some(path) => path,
        None => match files_dir() { Ok(dir) => FILE.get_or_init(|| dir.join("dev-log.txt")), Err(_) => return },
    };
    static LOG_LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOG_LOCK.lock();
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 64 * 1024) { let _ = std::fs::rename(path, path.with_extension("old.txt")); }
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) { let _ = writeln!(f, "{ms} {message}"); }
}

pub fn take_dev_play() -> Option<serde_json::Value> {
    let path = files_dir().ok()?.join("dev-play.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(&text).ok()
}

pub fn take_dev_autoplay() -> Option<String> {
    let path = files_dir().ok()?.join("dev-autoplay.txt");
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(text)
}
