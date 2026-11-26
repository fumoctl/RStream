use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[repr(C)]
struct RcloneRPCResult {
    output: *mut c_char,
    status: c_int,
}

type FnRcloneInitialize = unsafe extern "C" fn();
type FnRcloneFinalize = unsafe extern "C" fn();
type FnRcloneRPC = unsafe extern "C" fn(*const c_char, *const c_char) -> RcloneRPCResult;
type FnRcloneFreeString = unsafe extern "C" fn(*mut c_char);
type FnOpenURLCallback = unsafe extern "C" fn(*const c_char) -> c_int;
type FnRcloneSetOpenURL = unsafe extern "C" fn(Option<FnOpenURLCallback>);

pub type OpenUrlHandler = fn(&str) -> Result<()>;

struct Librclone {
    _handle: *mut c_void,
    rpc: FnRcloneRPC,
    free_string: FnRcloneFreeString,
    set_open_url: Option<FnRcloneSetOpenURL>,
    _finalize: FnRcloneFinalize,
}

// Go's librclone RcloneRPC is safe to call concurrently across threads.
unsafe impl Send for Librclone {}
unsafe impl Sync for Librclone {}

struct HttpServerEntry {
    base_url: String,
    id: Option<String>,
}

static LIBRCLONE: OnceLock<std::result::Result<Librclone, String>> = OnceLock::new();
static CONFIG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static HTTP_SERVERS: Mutex<Option<HashMap<String, HttpServerEntry>>> = Mutex::new(None);
static OPEN_URL_HANDLER: Mutex<Option<OpenUrlHandler>> = Mutex::new(None);

#[cfg(target_os = "macos")]
const LIB_FILENAME: &str = "librclone.dylib";
#[cfg(target_os = "windows")]
const LIB_FILENAME: &str = "librclone.dll";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const LIB_FILENAME: &str = "librclone.so";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub name: String,
    pub kind: String,
    pub raw: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgress {
    pub jobid: i64,
    pub finished: bool,
    pub success: bool,
    pub error: Option<String>,
    pub bytes: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFile {
    pub name: String,
    pub path: String,
    pub size: i64,
    pub is_dir: bool,
    pub mime_type: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderOption {
    pub name: String,
    pub help: String,
    pub required: bool,
    pub is_password: bool,
    pub default_value: String,
    pub examples: Vec<String>,
    pub advanced: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub description: String,
    pub prefix: String,
    pub is_oauth: bool,
    pub options: Vec<ProviderOption>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteSetupMode {
    CreateNew,
    ConnectExisting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CryptOptions {
    pub password: String,
    pub salt: Option<String>,
    pub folder: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteSetupRequest {
    pub name: String,
    pub provider: String,
    pub parameters: serde_json::Map<String, Value>,
    pub mode: RemoteSetupMode,
    pub crypt: Option<CryptOptions>,
}

#[cfg(unix)]
unsafe fn dlerror_string() -> String {
    let err = libc::dlerror();
    if err.is_null() {
        "unknown dlopen error".to_string()
    } else {
        CStr::from_ptr(err).to_string_lossy().into_owned()
    }
}

#[cfg(unix)]
unsafe fn load_library(path: &str) -> std::result::Result<*mut c_void, String> {
    let c_path = CString::new(path).map_err(|_| "interior NUL byte".to_string())?;
    // NEVER dlclose: Go runtime cannot be safely unloaded/reloaded in-process.
    let handle = libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if handle.is_null() {
        Err(dlerror_string())
    } else {
        Ok(handle)
    }
}

#[cfg(unix)]
unsafe fn load_symbol<T>(handle: *mut c_void, name: &CStr) -> std::result::Result<T, String> {
    let _ = libc::dlerror();
    let ptr = libc::dlsym(handle, name.as_ptr());
    if ptr.is_null() {
        return Err(format!(
            "missing symbol {}: {}",
            name.to_string_lossy(),
            dlerror_string()
        ));
    }
    Ok(std::mem::transmute_copy(&ptr))
}

#[cfg(windows)]
mod win_dl {
    use std::ffi::{c_char, c_void, CStr, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x0000_0100;
    const LOAD_LIBRARY_SEARCH_APPLICATION_DIR: u32 = 0x0000_0200;
    const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(
            lpLibFileName: *const u16,
            hFile: *mut c_void,
            dwFlags: u32,
        ) -> *mut c_void;
        fn GetProcAddress(hModule: *mut c_void, lpProcName: *const c_char) -> *mut c_void;
        fn GetLastError() -> u32;
    }

    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            hAlgorithm: *mut c_void,
            pbBuffer: *mut u8,
            cbBuffer: u32,
            dwFlags: u32,
        ) -> i32;
    }

    pub unsafe fn load_library(path: &str) -> std::result::Result<*mut c_void, String> {
        let wide: Vec<u16> = OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let flags = if !path.contains(['\\', '/', ':']) {
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32
        } else if Path::new(path).is_absolute() {
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32
        } else {
            0
        };
        // NEVER FreeLibrary: Go runtime cannot be safely unloaded/reloaded in-process.
        let handle = LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), flags);
        if handle.is_null() {
            Err(format!("LoadLibraryExW failed (error {})", GetLastError()))
        } else {
            Ok(handle)
        }
    }

    pub unsafe fn load_symbol<T>(
        handle: *mut c_void,
        name: &CStr,
    ) -> std::result::Result<T, String> {
        let ptr = GetProcAddress(handle, name.as_ptr());
        if ptr.is_null() {
            return Err(format!(
                "missing symbol {} (error {})",
                name.to_string_lossy(),
                GetLastError()
            ));
        }
        Ok(std::mem::transmute_copy(&ptr))
    }

    pub fn fill_random_bytes(buf: &mut [u8]) -> anyhow::Result<()> {
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            anyhow::bail!("BCryptGenRandom failed ({status:#x})");
        }
        Ok(())
    }
}

#[cfg(windows)]
use win_dl::{load_library, load_symbol};

fn candidate_library_paths() -> Vec<(String, bool)> {
    let mut paths = Vec::new();
    if let Ok(env_path) = std::env::var("RSTREAM_LIBRCLONE_PATH") {
        if !env_path.trim().is_empty() {
            paths.push((env_path, false));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push((dir.join(LIB_FILENAME).to_string_lossy().into_owned(), true));
        }
    }
    #[cfg(any(test, debug_assertions))]
    {
        let platform_dir = if cfg!(target_os = "windows") {
            "windows-x86_64"
        } else if cfg!(target_os = "macos") {
            "macos-aarch64"
        } else if cfg!(all(target_os = "android", target_arch = "aarch64")) {
            "android-arm64-v8a"
        } else if cfg!(all(target_os = "android", target_arch = "arm")) {
            "android-armeabi-v7a"
        } else if cfg!(all(target_os = "android", target_arch = "x86_64")) {
            "android-x86_64"
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            "linux-aarch64"
        } else {
            "linux-x86_64"
        };
        let manifest_vendor = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/librclone")
            .join(platform_dir)
            .join(LIB_FILENAME);
        paths.push((manifest_vendor.to_string_lossy().into_owned(), true));
        paths.push((
            format!("./vendor/librclone/{platform_dir}/{LIB_FILENAME}"),
            true,
        ));
    }
    // Android's Bionic linker resolves APK `jniLibs/<abi>/librclone.so` by soname (`current_exe` is app_process64).
    #[cfg(any(target_os = "android", test, debug_assertions))]
    paths.push((LIB_FILENAME.to_string(), false));
    paths
}

fn load_and_initialize() -> std::result::Result<Librclone, String> {
    let mut errors = Vec::new();
    for (candidate, check_exists) in candidate_library_paths() {
        if check_exists && !Path::new(&candidate).exists() {
            errors.push(format!("{candidate}: not found"));
            continue;
        }
        let handle = match unsafe { load_library(&candidate) } {
            Ok(h) => h,
            Err(e) => {
                errors.push(format!("{candidate}: {e}"));
                continue;
            }
        };
        let init: FnRcloneInitialize = match unsafe { load_symbol(handle, c"RcloneInitialize") } {
            Ok(f) => f,
            Err(e) => {
                errors.push(format!("{candidate}: {e}"));
                continue;
            }
        };
        let finalize: FnRcloneFinalize = match unsafe { load_symbol(handle, c"RcloneFinalize") } {
            Ok(f) => f,
            Err(e) => {
                errors.push(format!("{candidate}: {e}"));
                continue;
            }
        };
        let rpc: FnRcloneRPC = match unsafe { load_symbol(handle, c"RcloneRPC") } {
            Ok(f) => f,
            Err(e) => {
                errors.push(format!("{candidate}: {e}"));
                continue;
            }
        };
        let free_string: FnRcloneFreeString =
            match unsafe { load_symbol(handle, c"RcloneFreeString") } {
                Ok(f) => f,
                Err(e) => {
                    errors.push(format!("{candidate}: {e}"));
                    continue;
                }
            };
        let set_open_url: Option<FnRcloneSetOpenURL> =
            unsafe { load_symbol(handle, c"RcloneSetOpenURL") }.ok();
        unsafe { init() };
        if let Some(setter) = set_open_url {
            let has_handler = OPEN_URL_HANDLER.lock().ok().is_some_and(|g| g.is_some());
            unsafe {
                setter(if has_handler {
                    Some(c_open_url_trampoline)
                } else {
                    None
                });
            }
        }
        return Ok(Librclone {
            _handle: handle,
            rpc,
            free_string,
            set_open_url,
            _finalize: finalize,
        });
    }
    Err(format!(
        "failed to load librclone ({})",
        errors.join("; ")
    ))
}

fn librclone() -> Result<&'static Librclone> {
    LIBRCLONE
        .get_or_init(load_and_initialize)
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

// Unblocks rclone's loopback OAuth listener on 127.0.0.1:53682 when cancelled or browser launch fails.
fn abort_local_oauth_server(error_code: &str, description: &str) {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53682);
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let req = format!(
        "GET /?error={}&error_description={} HTTP/1.1\r\nHost: 127.0.0.1:53682\r\nConnection: close\r\n\r\n",
        percent_encode(error_code),
        percent_encode(description),
    );
    let _ = stream.write_all(req.as_bytes());
    let mut buf = [0u8; 256];
    let _ = std::io::Read::read(&mut stream, &mut buf);
}

pub fn cancel_oauth() {
    abort_local_oauth_server("cancelled", "Sign-in cancelled");
}

unsafe extern "C" fn c_open_url_trampoline(url_ptr: *const c_char) -> c_int {
    let res = std::panic::catch_unwind(|| -> c_int {
        if url_ptr.is_null() {
            return -1;
        }
        let url = unsafe { CStr::from_ptr(url_ptr) }.to_string_lossy();
        let handler = OPEN_URL_HANDLER.lock().ok().and_then(|g| *g);
        let Some(handler) = handler else {
            return -1;
        };
        match handler(&url) {
            Ok(()) => 0,
            Err(err) => {
                let msg = format!("{err:#}");
                std::thread::spawn(move || {
                    abort_local_oauth_server("browser_open_failed", &msg);
                });
                -1
            }
        }
    });
    res.unwrap_or(-1)
}

pub fn set_open_url_handler(handler: Option<OpenUrlHandler>) {
    if let Ok(mut guard) = OPEN_URL_HANDLER.lock() {
        *guard = handler;
    }
    if let Some(Ok(lib)) = LIBRCLONE.get() {
        if let Some(setter) = lib.set_open_url {
            unsafe {
                setter(handler.map(|_| c_open_url_trampoline as FnOpenURLCallback));
            }
        }
    }
}

pub fn init(config_path: &Path) -> Result<()> {
    if let Some(parent) = config_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("create rclone config dir {}", parent.display()))?;
    }
    if !config_path.exists() {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(config_path)
            .with_context(|| format!("initialize rclone config {}", config_path.display()))?;
        file.sync_all()?;
    }
    rpc(
        "config/setpath",
        json!({ "path": config_path.to_string_lossy() }),
    )?;
    {
        let mut guard = CONFIG_PATH
            .lock()
            .map_err(|_| anyhow!("config path lock poisoned"))?;
        *guard = Some(config_path.to_path_buf());
    }
    clear_http_server_cache();
    enforce_config_permissions()?;
    Ok(())
}

pub fn config_path() -> Option<PathBuf> {
    CONFIG_PATH.lock().ok().and_then(|g| g.clone())
}

fn enforce_config_permissions() -> Result<()> {
    #[cfg(unix)]
    if let Some(path) = config_path() {
        if path.exists() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("set 0600 permissions on {}", path.display()))?;
        }
    }
    Ok(())
}

fn matches_remote_fs_key(fs_key: &str, remote_name: &str) -> bool {
    fs_key == remote_name
        || fs_key
            .strip_prefix(remote_name)
            .is_some_and(|rest| rest.starts_with(':'))
}

fn evict_http_server_cache(remote_name: &str) {
    let removed_ids: Vec<String> = match HTTP_SERVERS.lock() {
        Ok(mut guard) => {
            let Some(map) = guard.as_mut() else {
                return;
            };
            let keys: Vec<String> = map
                .keys()
                .filter(|k| matches_remote_fs_key(k, remote_name))
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| map.remove(&k).and_then(|entry| entry.id))
                .collect()
        }
        Err(_) => return,
    };
    for id in removed_ids {
        let _ = rpc("serve/stop", json!({ "id": id }));
    }
}

fn clear_http_server_cache() {
    let removed_ids: Vec<String> = match HTTP_SERVERS.lock() {
        Ok(mut guard) => guard
            .take()
            .into_iter()
            .flat_map(|map| map.into_values().filter_map(|entry| entry.id))
            .collect(),
        Err(_) => return,
    };
    for id in removed_ids {
        let _ = rpc("serve/stop", json!({ "id": id }));
    }
}

pub fn rpc(method: &str, input: Value) -> Result<Value> {
    let lib = librclone()?;
    let c_method = CString::new(method).context("rpc method contains interior NUL")?;
    let payload = if input.is_null() { json!({}) } else { input };
    let input_str = serde_json::to_string(&payload)?;
    let c_input = CString::new(input_str).context("rpc input contains interior NUL")?;

    let result = unsafe { (lib.rpc)(c_method.as_ptr(), c_input.as_ptr()) };
    let out_str = if result.output.is_null() {
        String::new()
    } else {
        let s = unsafe { CStr::from_ptr(result.output) }
            .to_string_lossy()
            .into_owned();
        unsafe { (lib.free_string)(result.output) };
        s
    };

    let parsed: Value = if out_str.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&out_str).with_context(|| {
            format!(
                "invalid JSON from rclone RPC `{method}` (status {}): {out_str}",
                result.status
            )
        })?
    };

    if !(200..300).contains(&result.status) {
        let msg = parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or(&out_str);
        bail!(
            "rclone RPC `{method}` failed (status {}): {msg}",
            result.status
        );
    }
    Ok(parsed)
}

fn is_known_oauth_provider(name: &str, prefix: &str, raw_options: &[Value]) -> bool {
    if matches!(
        prefix,
        "drive" | "onedrive" | "dropbox" | "box" | "gphotos" | "gcs" | "pcloud" | "yandex" | "zoho"
    ) || matches!(
        name,
        "drive" | "onedrive" | "dropbox" | "box" | "google photos" | "google cloud storage"
    ) {
        return true;
    }
    let mut has_token = false;
    let mut has_auth_url = false;
    let mut has_token_url = false;
    for opt in raw_options {
        match opt.get("Name").and_then(Value::as_str) {
            Some("token") => has_token = true,
            Some("auth_url") => has_auth_url = true,
            Some("token_url") => has_token_url = true,
            _ => {}
        }
    }
    has_token && has_auth_url && has_token_url
}

pub fn list_providers() -> Result<Vec<ProviderInfo>> {
    let resp = rpc("config/providers", json!({}))?;
    let Some(items) = resp.get("providers").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut providers = Vec::with_capacity(items.len());
    for p in items {
        if p.get("Hide").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let prefix = p
            .get("Prefix")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = p
            .get("Name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&prefix)
            .to_string();
        if name.is_empty() {
            continue;
        }
        let description = p
            .get("Description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let raw_options = p
            .get("Options")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let is_oauth = is_known_oauth_provider(&name, &prefix, raw_options);

        let mut options = Vec::with_capacity(raw_options.len());
        for opt in raw_options {
            let hide_mask = opt.get("Hide").and_then(Value::as_u64).unwrap_or(0);
            if (hide_mask & 2) != 0 {
                continue;
            }
            let opt_name = opt
                .get("Name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if opt_name.is_empty() {
                continue;
            }
            let mut examples = Vec::new();
            if let Some(ex_arr) = opt.get("Examples").and_then(Value::as_array) {
                for ex in ex_arr {
                    let val = ex
                        .get("Value")
                        .and_then(Value::as_str)
                        .or_else(|| ex.as_str())
                        .unwrap_or_default();
                    if !val.is_empty() && !examples.iter().any(|existing| existing == val) {
                        examples.push(val.to_string());
                    }
                }
            }
            options.push(ProviderOption {
                name: opt_name,
                help: opt
                    .get("Help")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                required: opt
                    .get("Required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                is_password: opt
                    .get("IsPassword")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                default_value: opt
                    .get("DefaultStr")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                examples,
                advanced: opt
                    .get("Advanced")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }

        providers.push(ProviderInfo {
            name,
            description,
            prefix,
            is_oauth,
            options,
        });
    }
    providers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(providers)
}

pub fn list_remotes() -> Result<Vec<RemoteInfo>> {
    let dump = rpc("config/dump", json!({}))?;
    let Some(map) = dump.as_object() else {
        return Ok(Vec::new());
    };
    let mut remotes: Vec<RemoteInfo> = map
        .iter()
        .map(|(name, raw)| RemoteInfo {
            name: name.clone(),
            kind: raw
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            raw: raw.clone(),
        })
        .collect();
    remotes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(remotes)
}

pub fn create_remote(name: &str, kind: &str, params: Value) -> Result<()> {
    let parameters = if params.is_null() {
        json!({})
    } else if params.is_object() {
        params
    } else {
        bail!("create_remote params must be a JSON object");
    };
    rpc(
        "config/create",
        json!({
            "name": name,
            "type": kind,
            "parameters": parameters,
        }),
    )?;
    evict_http_server_cache(name);
    enforce_config_permissions()?;
    Ok(())
}

pub fn create_crypt_remote(
    name: &str,
    remote_target: &str,
    password: &str,
    salt_password: Option<&str>,
) -> Result<()> {
    let mut parameters = serde_json::Map::new();
    parameters.insert("remote".to_string(), Value::String(remote_target.to_string()));
    parameters.insert("password".to_string(), Value::String(password.to_string()));
    if let Some(salt) = salt_password.filter(|s| !s.is_empty()) {
        parameters.insert("password2".to_string(), Value::String(salt.to_string()));
    }
    rpc(
        "config/create",
        json!({
            "name": name,
            "type": "crypt",
            "parameters": Value::Object(parameters),
            "opt": { "obscure": true, "nonInteractive": true },
        }),
    )?;
    evict_http_server_cache(name);
    enforce_config_permissions()?;
    Ok(())
}

fn validate_remote_name(raw_name: &str) -> Result<String> {
    let trimmed = raw_name.trim();
    if trimmed.is_empty() {
        bail!("remote name cannot be empty");
    }
    if trimmed.ends_with(':') {
        bail!("remote name `{trimmed}` must not end with ':'");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("remote name `{trimmed}` may only contain ASCII letters, digits, '-', and '_'");
    }
    Ok(trimmed.to_string())
}

fn value_as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "y" => Some(true),
            "false" | "0" | "no" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn has_interactive_desktop() -> bool {
    if cfg!(test)
        || std::env::var_os("CI").is_some()
        || std::env::var_os("RSTREAM_HEADLESS").is_some()
    {
        return false;
    }
    #[cfg(target_os = "android")]
    {
        OPEN_URL_HANDLER.lock().ok().is_some_and(|g| g.is_some())
    }
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    {
        let has_display = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
        has_display || has_wayland
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        true
    }
}

fn has_explicit_oauth_credentials(params: &serde_json::Map<String, Value>) -> bool {
    for key in [
        "token",
        "service_account_file",
        "service_account_credentials",
        "access_token",
        "box_config_file",
    ] {
        if params
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        {
            return true;
        }
    }
    for bool_key in ["env_auth", "anonymous", "client_credentials"] {
        if params.get(bool_key).and_then(value_as_bool) == Some(true) {
            return true;
        }
    }
    false
}

fn trim_trailing_slashes_preserve_root(path: &str) -> String {
    let trimmed = path.trim();
    let stripped = trimmed.trim_end_matches(['/', '\\']);
    if stripped.is_empty() && !trimmed.is_empty() {
        return trimmed[..1].to_string();
    }
    if stripped.len() == 2 && stripped.as_bytes()[1] == b':' && trimmed.len() >= 3 {
        return trimmed[..3].to_string();
    }
    stripped.to_string()
}

fn build_crypt_remote_target(
    backing_name: &str,
    provider_prefix: &str,
    local_root: Option<&str>,
    crypt_folder: &str,
) -> String {
    if provider_prefix == "local" {
        let folder_trimmed = crypt_folder.trim();
        let local_target_path = match (local_root, folder_trimmed.is_empty()) {
            (Some(root), true) => trim_trailing_slashes_preserve_root(root),
            (Some(root), false) => {
                let base = trim_trailing_slashes_preserve_root(root);
                let sub = folder_trimmed.trim_matches(['/', '\\']);
                if sub.is_empty() {
                    base
                } else if base.ends_with(['/', '\\']) {
                    format!("{base}{sub}")
                } else {
                    format!("{base}/{sub}")
                }
            }
            (None, false) => trim_trailing_slashes_preserve_root(folder_trimmed),
            (None, true) => String::new(),
        };
        if local_target_path.is_empty() {
            format!("{backing_name}:")
        } else {
            format!("{backing_name}:{local_target_path}")
        }
    } else {
        let folder_clean = crypt_folder.trim().trim_matches(['/', '\\']);
        if folder_clean.is_empty() {
            format!("{backing_name}:")
        } else {
            format!("{backing_name}:{folder_clean}")
        }
    }
}

fn companion_raw_remote_to_cleanup(dump: &Value, name: &str) -> Option<String> {
    let map = dump.as_object()?;
    let backing_name = format!("{name}-raw");
    if !map.contains_key(&backing_name) {
        return None;
    }
    let entry = map.get(name)?;
    if entry.get("type").and_then(Value::as_str) != Some("crypt") {
        return None;
    }
    let remote_target = entry.get("remote").and_then(Value::as_str)?;
    let expected_prefix = format!("{backing_name}:");
    if !remote_target.starts_with(&expected_prefix) {
        return None;
    }
    let referenced_elsewhere = map.iter().any(|(other_name, other_cfg)| {
        other_name != name
            && other_cfg
                .get("remote")
                .and_then(Value::as_str)
                .is_some_and(|r| r.starts_with(&expected_prefix))
    });
    if referenced_elsewhere {
        None
    } else {
        Some(backing_name)
    }
}

pub fn setup_remote(req: RemoteSetupRequest) -> Result<String> {
    let name = validate_remote_name(&req.name)?;
    let raw_provider = req.provider.trim();
    if raw_provider.is_empty() {
        bail!("remote provider cannot be empty");
    }
    if let Some(ref crypt) = req.crypt {
        if crypt.password.is_empty() {
            bail!("crypt password cannot be empty");
        }
    }

    let providers = list_providers().unwrap_or_default();
    let matched_provider = providers
        .iter()
        .find(|p| p.prefix == raw_provider || p.name == raw_provider);
    let provider_prefix = matched_provider
        .map(|p| p.prefix.clone())
        .unwrap_or_else(|| raw_provider.to_string());
    let is_oauth = matched_provider.is_some_and(|p| p.is_oauth);

    if provider_prefix == "crypt" && req.crypt.is_some() {
        bail!("provider `crypt` cannot be wrapped inside another `crypt` layer");
    }

    let mut authorize_oauth: Option<bool> = None;
    let mut non_interactive_override: Option<bool> = None;
    let mut local_root: Option<String> = None;
    let mut clean_params = serde_json::Map::new();

    for (k, v) in req.parameters {
        if k == "authorize_oauth" {
            authorize_oauth = value_as_bool(&v);
            continue;
        }
        if k == "non_interactive" || k == "nonInteractive" {
            non_interactive_override = value_as_bool(&v);
            continue;
        }
        if provider_prefix == "local" && matches!(k.as_str(), "Root" | "root" | "path" | "Path") {
            if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                local_root = Some(s.to_string());
            }
            continue;
        }
        if v.is_null() {
            continue;
        }
        if let Value::String(ref s) = v {
            if s.is_empty() {
                continue;
            }
        }
        clean_params.insert(k, v);
    }

    let interactive_oauth = if is_oauth && !has_explicit_oauth_credentials(&clean_params) {
        if let Some(auth) = authorize_oauth {
            auth
        } else if let Some(ni) = non_interactive_override {
            !ni
        } else if clean_params.get("config_is_local").and_then(value_as_bool) == Some(false) {
            false
        } else {
            has_interactive_desktop()
        }
    } else {
        false
    };

    if interactive_oauth && !clean_params.contains_key("config_is_local") {
        clean_params.insert(
            "config_is_local".to_string(),
            Value::String("true".to_string()),
        );
    }

    if interactive_oauth {
        // Free 127.0.0.1:53682 if a prior OAuth attempt was abandoned in the browser.
        abort_local_oauth_server("superseded", "Replaced by a new sign-in request");
    }

    let create_opt = json!({
        "obscure": true,
        "nonInteractive": !interactive_oauth,
    });

    let prior_companion = rpc("config/dump", json!({}))
        .ok()
        .and_then(|dump| companion_raw_remote_to_cleanup(&dump, &name));

    match req.crypt {
        None => {
            rpc(
                "config/create",
                json!({
                    "name": name,
                    "type": provider_prefix,
                    "parameters": Value::Object(clean_params),
                    "opt": create_opt,
                }),
            )?;
            if let Some(stale_backing) = prior_companion {
                let _ = rpc("config/delete", json!({ "name": stale_backing }));
                evict_http_server_cache(&stale_backing);
            }
            evict_http_server_cache(&name);
            enforce_config_permissions()?;
            Ok(name)
        }
        Some(crypt) => {
            let backing_name = format!("{name}-raw");
            rpc(
                "config/create",
                json!({
                    "name": backing_name,
                    "type": provider_prefix,
                    "parameters": Value::Object(clean_params),
                    "opt": create_opt,
                }),
            )?;
            evict_http_server_cache(&backing_name);

            let crypt_target = build_crypt_remote_target(
                &backing_name,
                &provider_prefix,
                local_root.as_deref(),
                &crypt.folder,
            );

            if req.mode == RemoteSetupMode::CreateNew && provider_prefix == "local" {
                if let Some(dir_path) = crypt_target
                    .strip_prefix(&format!("{backing_name}:"))
                    .filter(|p| !p.is_empty())
                {
                    let _ = fs::create_dir_all(dir_path);
                }
            }

            if let Err(err) =
                create_crypt_remote(&name, &crypt_target, &crypt.password, crypt.salt.as_deref())
            {
                let _ = rpc("config/delete", json!({ "name": backing_name }));
                let _ = enforce_config_permissions();
                return Err(err);
            }

            evict_http_server_cache(&name);
            enforce_config_permissions()?;
            Ok(name)
        }
    }
}

pub fn delete_remote(name: &str) -> Result<()> {
    let trimmed = name.trim();
    let companion = rpc("config/dump", json!({}))
        .ok()
        .and_then(|dump| companion_raw_remote_to_cleanup(&dump, trimmed));

    rpc("config/delete", json!({ "name": trimmed }))?;
    evict_http_server_cache(trimmed);

    if let Some(backing_name) = companion {
        rpc("config/delete", json!({ "name": backing_name }))?;
        evict_http_server_cache(&backing_name);
    }

    enforce_config_permissions()?;
    Ok(())
}

pub fn import_config(raw_ini: &str) -> Result<()> {
    let path = config_path().context("rclone not initialized; call init() first")?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("create rclone config dir {}", parent.display()))?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&path)
        .with_context(|| format!("open config path {}", path.display()))?;
    file.write_all(raw_ini.as_bytes())?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    rpc("config/setpath", json!({ "path": path.to_string_lossy() }))?;
    clear_http_server_cache();
    Ok(())
}

pub fn copy_local_to_remote(src_file: &Path, dst_fs: &str, dst_remote_path: &str) -> Result<i64> {
    let file_name = src_file
        .file_name()
        .context("src_file has no file name")?
        .to_string_lossy()
        .into_owned();
    let src_dir = match src_file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_string_lossy().into_owned(),
        _ => ".".to_string(),
    };
    let res = rpc(
        "operations/copyfile",
        json!({
            "srcFs": src_dir,
            "srcRemote": file_name,
            "dstFs": dst_fs,
            "dstRemote": dst_remote_path,
            "_async": true,
        }),
    )?;
    res.get("jobid")
        .and_then(Value::as_i64)
        .context("operations/copyfile missing jobid")
}

pub fn job_progress(jobid: i64) -> Result<JobProgress> {
    let status = rpc("job/status", json!({ "jobid": jobid }))?;
    let finished = status
        .get("finished")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let success = status
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let error = status
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let group = format!("job/{jobid}");
    let stats = rpc("core/stats", json!({ "group": group })).unwrap_or(Value::Null);
    let mut bytes = stats.get("bytes").and_then(Value::as_u64).unwrap_or(0);
    let mut total = stats
        .get("totalBytes")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0);

    if bytes == 0 || total.is_none() {
        if let Ok(tr) = rpc("core/transferred", json!({ "group": format!("job/{jobid}") })) {
            if let Some(items) = tr.get("transferred").and_then(Value::as_array) {
                let sum_bytes: u64 = items
                    .iter()
                    .filter_map(|it| it.get("bytes").and_then(Value::as_u64))
                    .sum();
                let sum_total: u64 = items
                    .iter()
                    .filter_map(|it| it.get("size").and_then(Value::as_i64))
                    .filter(|&s| s > 0)
                    .map(|s| s as u64)
                    .sum();
                if bytes == 0 && sum_bytes > 0 {
                    bytes = sum_bytes;
                }
                if total.is_none() && sum_total > 0 {
                    total = Some(sum_total);
                }
            }
        }
    }

    Ok(JobProgress {
        jobid,
        finished,
        success,
        error,
        bytes,
        total,
    })
}

pub fn stop_job(jobid: i64) -> Result<()> {
    rpc("job/stop", json!({ "jobid": jobid }))?;
    Ok(())
}

pub fn list_remote_files(remote_fs: &str, dir: &str) -> Result<Vec<RemoteFile>> {
    list_remote_files_inner(remote_fs, dir, false)
}

pub fn list_remote_files_recursive(remote_fs: &str, dir: &str) -> Result<Vec<RemoteFile>> {
    match list_remote_files_inner(remote_fs, dir, true) {
        Ok(items) => Ok(items),
        Err(err) => {
            let msg = format!("{err:#}").to_ascii_lowercase();
            if msg.contains("directory not found")
                || msg.contains("object not found")
                || msg.contains("file does not exist")
                || msg.contains("not found")
            {
                Ok(Vec::new())
            } else {
                Err(err)
            }
        }
    }
}

fn list_remote_files_inner(remote_fs: &str, dir: &str, recurse: bool) -> Result<Vec<RemoteFile>> {
    let input = if recurse {
        json!({
            "fs": remote_fs,
            "remote": dir,
            "opt": { "recurse": true },
        })
    } else {
        json!({
            "fs": remote_fs,
            "remote": dir,
        })
    };
    let res = rpc("operations/list", input)?;
    let items = res
        .get("list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(items
        .into_iter()
        .map(|item| RemoteFile {
            name: item
                .get("Name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            path: item
                .get("Path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            size: item.get("Size").and_then(Value::as_i64).unwrap_or(0),
            is_dir: item.get("IsDir").and_then(Value::as_bool).unwrap_or(false),
            mime_type: item
                .get("MimeType")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect())
}

fn temp_transfer_file(tag: &str) -> Result<(PathBuf, String)> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let base_dir = config_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("rclone-tmp");
    fs::create_dir_all(&base_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&base_dir, fs::Permissions::from_mode(0o700))?;
    }
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = format!("{tag}-{}-{nanos}-{n}.tmp", std::process::id());
    Ok((base_dir, file_name))
}

pub fn put_remote_bytes(remote_fs: &str, remote_path: &str, bytes: &[u8]) -> Result<()> {
    let (dir, name) = temp_transfer_file("put")?;
    let full = dir.join(&name);
    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _cleanup = Guard(full.clone());
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&full)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    rpc(
        "operations/copyfile",
        json!({
            "srcFs": dir.to_string_lossy(),
            "srcRemote": name,
            "dstFs": remote_fs,
            "dstRemote": remote_path.trim_start_matches('/'),
        }),
    )?;
    Ok(())
}

pub fn get_remote_bytes(remote_fs: &str, remote_path: &str) -> Result<Vec<u8>> {
    let (dir, name) = temp_transfer_file("get")?;
    let full = dir.join(&name);
    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _cleanup = Guard(full.clone());
    rpc(
        "operations/copyfile",
        json!({
            "srcFs": remote_fs,
            "srcRemote": remote_path.trim_start_matches('/'),
            "dstFs": dir.to_string_lossy(),
            "dstRemote": name,
        }),
    )?;
    Ok(fs::read(&full)?)
}

pub fn delete_remote_file(remote_fs: &str, remote_path: &str) -> Result<()> {
    rpc(
        "operations/deletefile",
        json!({
            "fs": remote_fs,
            "remote": remote_path,
        }),
    )?;
    Ok(())
}

fn random_hex_token_16() -> Result<String> {
    let mut buf = [0u8; 16];
    #[cfg(unix)]
    {
        use std::io::Read;
        fs::File::open("/dev/urandom")
            .context("open /dev/urandom")?
            .read_exact(&mut buf)
            .context("read /dev/urandom")?;
    }
    #[cfg(windows)]
    {
        win_dl::fill_random_bytes(&mut buf)?;
    }
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn ensure_http_server(remote_fs: &str) -> Result<String> {
    let mut guard = HTTP_SERVERS
        .lock()
        .map_err(|_| anyhow!("http server cache lock poisoned"))?;
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(entry) = map.get(remote_fs) {
        return Ok(entry.base_url.clone());
    }
    let token = random_hex_token_16()?;
    let res = rpc(
        "serve/start",
        json!({
            "type": "http",
            "fs": remote_fs,
            "addr": "127.0.0.1:0",
            "baseurl": format!("/{token}"),
        }),
    )?;
    let addr = res
        .get("addr")
        .and_then(Value::as_str)
        .context("serve/start missing addr")?
        .trim_end_matches('/');
    let id = res.get("id").and_then(Value::as_str).map(str::to_owned);
    let addr_stripped = addr
        .strip_prefix("http://")
        .unwrap_or(addr)
        .strip_suffix(&format!("/{token}"))
        .unwrap_or(addr.strip_prefix("http://").unwrap_or(addr));
    let base_url = format!("http://{addr_stripped}/{token}");
    map.insert(
        remote_fs.to_string(),
        HttpServerEntry {
            base_url: base_url.clone(),
            id,
        },
    );
    Ok(base_url)
}

fn encode_url_path(remote_path: &str) -> String {
    remote_path
        .trim_start_matches('/')
        .split('/')
        .filter(|seg| !seg.is_empty())
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

pub fn file_stream_url(remote_fs: &str, remote_path: &str) -> Result<String> {
    let base = ensure_http_server(remote_fs)?;
    let encoded = encode_url_path(remote_path);
    if encoded.is_empty() {
        Ok(base)
    } else {
        Ok(format!("{base}/{encoded}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;
    use std::sync::MutexGuard;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    static TEST_SERIAL_LOCK: Mutex<()> = Mutex::new(());

    fn lock_rclone_test() -> MutexGuard<'static, ()> {
        TEST_SERIAL_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(unix)]
    fn assert_mode_0600(path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "expected 0600 permissions on {}, got {mode:04o}",
            path.display()
        );
        Ok(())
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Result<Self> {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "rstream-rclone-{prefix}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_dir() {
                collect_files(&p, out)?;
            } else if p.is_file() {
                out.push(p);
            }
        }
        Ok(())
    }

    #[test]
    fn percent_encodes_segments_without_touching_slashes() {
        assert_eq!(
            encode_url_path("/Shows/Season 01/Ep [01] (1080p).mkv"),
            "Shows/Season%2001/Ep%20%5B01%5D%20%281080p%29.mkv"
        );
    }

    #[test]
    fn crypt_upload_and_http_range_stream() -> Result<()> {
        let _serial = lock_rclone_test();
        let tmp = TempDir::new("integration")?;
        let vault_dir = tmp.path().join("encrypted_vault");
        fs::create_dir_all(&vault_dir)?;
        let conf_path = tmp.path().join("rclone.conf");

        init(&conf_path)?;
        assert_eq!(config_path().as_deref(), Some(conf_path.as_path()));

        create_remote("local_store", "local", json!({}))?;
        let target = format!("local_store:{}", vault_dir.display());
        create_crypt_remote("vault_crypt", &target, "super-secret-pw", Some("salt-pw"))?;

        let remotes = list_remotes()?;
        assert!(remotes
            .iter()
            .any(|r| r.name == "local_store" && r.kind == "local"));
        assert!(remotes
            .iter()
            .any(|r| r.name == "vault_crypt" && r.kind == "crypt"));

        let payload: Vec<u8> = (0..4096usize)
            .map(|i| ((i.wrapping_mul(31).wrapping_add(17)) & 0xff) as u8)
            .collect();
        let src_file = tmp.path().join("sample.bin");
        fs::write(&src_file, &payload)?;

        let jobid = copy_local_to_remote(&src_file, "vault_crypt:", "nested/sample file.bin")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let progress = loop {
            let p = job_progress(jobid)?;
            if p.finished {
                break p;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for rclone copy job {jobid}"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            progress.success && progress.error.is_none(),
            "copy job failed: {progress:?}"
        );

        let files = list_remote_files("vault_crypt:", "nested")?;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "sample file.bin");
        assert_eq!(files[0].size, 4096);
        assert!(!files[0].is_dir);

        let mut encrypted_files = Vec::new();
        collect_files(&vault_dir, &mut encrypted_files)?;
        assert!(
            !encrypted_files.is_empty(),
            "expected encrypted file inside {}",
            vault_dir.display()
        );
        let needle = &payload[100..200];
        for ef in &encrypted_files {
            let raw = fs::read(ef)?;
            assert!(
                !raw.windows(needle.len()).any(|w| w == needle),
                "encrypted file {} unexpectedly contained plaintext bytes",
                ef.display()
            );
        }

        let stream_url = file_stream_url("vault_crypt:", "nested/sample file.bin")?;
        let rest = stream_url
            .strip_prefix("http://")
            .context("expected http:// stream URL")?;
        let (host_port, req_path) = rest.split_once('/').context("expected URL path")?;

        let mut conn = TcpStream::connect(host_port)?;
        let req = format!(
            "GET /{req_path} HTTP/1.1\r\nHost: {host_port}\r\nRange: bytes=100-500\r\nConnection: close\r\n\r\n"
        );
        conn.write_all(req.as_bytes())?;

        let mut resp = Vec::new();
        conn.read_to_end(&mut resp)?;
        let split_pos = resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .context("malformed HTTP response from rclone serve http")?;
        let headers = String::from_utf8_lossy(&resp[..split_pos]);
        assert!(
            headers.starts_with("HTTP/1.1 206"),
            "expected 206 Partial Content, got headers:\n{headers}"
        );
        let body = &resp[split_pos + 4..];
        assert_eq!(body, &payload[100..=500]);

        // Token path prefix must contain a 32-hex token and block unauthenticated root/path probing.
        let (tok_seg, _) = req_path.split_once('/').context("expected token prefix")?;
        assert_eq!(tok_seg.len(), 32);
        assert!(tok_seg.bytes().all(|b| b.is_ascii_hexdigit()));
        for unauth in ["/", "/nested/", "/nested/sample%20file.bin"] {
            let mut probe_conn = TcpStream::connect(host_port)?;
            let probe_req =
                format!("GET {unauth} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
            probe_conn.write_all(probe_req.as_bytes())?;
            let mut probe_resp = Vec::new();
            probe_conn.read_to_end(&mut probe_resp)?;
            let head = String::from_utf8_lossy(&probe_resp);
            assert!(
                head.starts_with("HTTP/1.1 404"),
                "unauthenticated path `{unauth}` must 404, got:\n{head}"
            );
        }

        // Verify rclone-tmp directory enforces 0700 on Unix.
        put_remote_bytes("vault_crypt:", "nested/tiny.bin", b"ok")?;
        assert_eq!(get_remote_bytes("vault_crypt:", "nested/tiny.bin")?, b"ok");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp_mode = fs::metadata(tmp.path().join("rclone-tmp"))?
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(tmp_mode, 0o700, "expected 0700 on rclone-tmp, got {tmp_mode:04o}");
        }
        assert_eq!(trim_trailing_slashes_preserve_root("C:\\Media\\\\"), "C:\\Media");
        assert_eq!(trim_trailing_slashes_preserve_root("C:\\"), "C:\\");
        assert_eq!(trim_trailing_slashes_preserve_root("///"), "/");

        delete_remote_file("vault_crypt:", "nested/tiny.bin")?;
        delete_remote_file("vault_crypt:", "nested/sample file.bin")?;
        assert!(list_remote_files("vault_crypt:", "nested")?.is_empty());

        delete_remote("vault_crypt")?;
        assert!(!list_remotes()?.iter().any(|r| r.name == "vault_crypt"));

        import_config("[imported_local]\ntype = local\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&conf_path)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(list_remotes()?
            .iter()
            .any(|r| r.name == "imported_local" && r.kind == "local"));

        Ok(())
    }

    #[test]
    fn list_providers_includes_major_backends_and_password_metadata() -> Result<()> {
        let _serial = lock_rclone_test();
        let providers = list_providers()?;
        assert!(!providers.is_empty(), "expected non-empty provider list");
        assert!(
            providers.windows(2).all(|w| w[0].name <= w[1].name),
            "expected providers sorted by name"
        );

        for expected in [
            "drive",
            "onedrive",
            "dropbox",
            "protondrive",
            "s3",
            "webdav",
            "sftp",
            "local",
        ] {
            let info = providers
                .iter()
                .find(|p| p.prefix == expected || p.name == expected)
                .unwrap_or_else(|| panic!("missing expected provider `{expected}`"));
            assert!(
                !info.options.is_empty(),
                "expected non-empty options for provider `{expected}`"
            );
        }

        let by_prefix = |prefix: &str| -> &ProviderInfo {
            providers
                .iter()
                .find(|p| p.prefix == prefix)
                .unwrap_or_else(|| panic!("missing provider prefix `{prefix}`"))
        };

        assert!(by_prefix("drive").is_oauth);
        assert!(by_prefix("onedrive").is_oauth);
        assert!(by_prefix("dropbox").is_oauth);
        assert!(by_prefix("box").is_oauth);
        assert!(!by_prefix("protondrive").is_oauth);
        assert!(!by_prefix("s3").is_oauth);
        assert!(!by_prefix("webdav").is_oauth);
        assert!(!by_prefix("sftp").is_oauth);
        assert!(!by_prefix("local").is_oauth);

        let proton_pw = by_prefix("protondrive")
            .options
            .iter()
            .find(|o| o.name == "password")
            .context("protondrive missing password option")?;
        assert!(proton_pw.is_password);
        assert!(proton_pw.required);

        let webdav_pass = by_prefix("webdav")
            .options
            .iter()
            .find(|o| o.name == "pass")
            .context("webdav missing pass option")?;
        assert!(webdav_pass.is_password);

        let webdav_vendor = by_prefix("webdav")
            .options
            .iter()
            .find(|o| o.name == "vendor")
            .context("webdav missing vendor option")?;
        assert!(!webdav_vendor.examples.is_empty());

        let sftp_pass = by_prefix("sftp")
            .options
            .iter()
            .find(|o| o.name == "pass")
            .context("sftp missing pass option")?;
        assert!(sftp_pass.is_password);

        // S3 secret_access_key is stored plaintext by rclone (IsPassword == false).
        let s3_secret = by_prefix("s3")
            .options
            .iter()
            .find(|o| o.name == "secret_access_key")
            .context("s3 missing secret_access_key option")?;
        assert!(!s3_secret.is_password);

        Ok(())
    }

    #[test]
    fn setup_remote_obscures_only_is_password_fields_and_handles_oauth_headless() -> Result<()> {
        let _serial = lock_rclone_test();
        let tmp = TempDir::new("obscure-and-oauth")?;
        let conf_path = tmp.path().join("rclone.conf");
        init(&conf_path)?;

        // Validation checks
        assert!(setup_remote(RemoteSetupRequest {
            name: "   ".to_string(),
            provider: "local".to_string(),
            parameters: serde_json::Map::new(),
            mode: RemoteSetupMode::CreateNew,
            crypt: None,
        })
        .is_err());
        assert!(setup_remote(RemoteSetupRequest {
            name: "bad:name:".to_string(),
            provider: "local".to_string(),
            parameters: serde_json::Map::new(),
            mode: RemoteSetupMode::CreateNew,
            crypt: None,
        })
        .is_err());
        assert!(setup_remote(RemoteSetupRequest {
            name: "empty_crypt_pw".to_string(),
            provider: "local".to_string(),
            parameters: serde_json::Map::new(),
            mode: RemoteSetupMode::CreateNew,
            crypt: Some(CryptOptions {
                password: String::new(),
                salt: None,
                folder: String::new(),
            }),
        })
        .is_err());

        // S3: secret_access_key has IsPassword == false -> must stay plaintext even with obscure: true.
        let mut s3_params = serde_json::Map::new();
        s3_params.insert("provider".into(), json!("AWS"));
        s3_params.insert("access_key_id".into(), json!("AKIAIOSFODNN7EXAMPLE"));
        s3_params.insert(
            "secret_access_key".into(),
            json!("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
        );
        let s3_name = setup_remote(RemoteSetupRequest {
            name: " test_s3 ".to_string(),
            provider: "s3".to_string(),
            parameters: s3_params,
            mode: RemoteSetupMode::CreateNew,
            crypt: None,
        })?;
        assert_eq!(s3_name, "test_s3");
        #[cfg(unix)]
        assert_mode_0600(&conf_path)?;

        // WebDAV: pass has IsPassword == true -> must be obscured in rclone.conf.
        let mut wd_params = serde_json::Map::new();
        wd_params.insert("url".into(), json!("https://dav.example.com"));
        wd_params.insert("vendor".into(), json!("other"));
        wd_params.insert("user".into(), json!("alice"));
        wd_params.insert("pass".into(), json!("my-cleartext-webdav-pw"));
        setup_remote(RemoteSetupRequest {
            name: "test_webdav".to_string(),
            provider: "webdav".to_string(),
            parameters: wd_params,
            mode: RemoteSetupMode::CreateNew,
            crypt: None,
        })?;

        // SFTP with empty optional password -> must NOT obscure "" into a bogus password.
        let mut sftp_params = serde_json::Map::new();
        sftp_params.insert("host".into(), json!("sftp.example.com"));
        sftp_params.insert("user".into(), json!("bob"));
        sftp_params.insert("pass".into(), json!(""));
        setup_remote(RemoteSetupRequest {
            name: "test_sftp_keyonly".to_string(),
            provider: "sftp".to_string(),
            parameters: sftp_params,
            mode: RemoteSetupMode::CreateNew,
            crypt: None,
        })?;

        // OAuth provider with explicit token AND without token (must never hang headless tests).
        let mut drive_tok_params = serde_json::Map::new();
        drive_tok_params.insert(
            "token".into(),
            json!(r#"{"access_token":"ya29.unit","token_type":"Bearer","expiry":"2030-01-01T00:00:00Z"}"#),
        );
        setup_remote(RemoteSetupRequest {
            name: "test_drive_token".to_string(),
            provider: "drive".to_string(),
            parameters: drive_tok_params,
            mode: RemoteSetupMode::ConnectExisting,
            crypt: None,
        })?;
        setup_remote(RemoteSetupRequest {
            name: "test_drive_headless".to_string(),
            provider: "drive".to_string(),
            parameters: serde_json::Map::new(),
            mode: RemoteSetupMode::CreateNew,
            crypt: None,
        })?;

        let remotes = list_remotes()?;
        let s3_remote = remotes
            .iter()
            .find(|r| r.name == "test_s3")
            .context("missing test_s3")?;
        assert_eq!(
            s3_remote.raw.get("secret_access_key").and_then(Value::as_str),
            Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
        );

        let wd_remote = remotes
            .iter()
            .find(|r| r.name == "test_webdav")
            .context("missing test_webdav")?;
        let obscured_pass = wd_remote
            .raw
            .get("pass")
            .and_then(Value::as_str)
            .context("missing obscured webdav pass")?;
        assert!(!obscured_pass.is_empty());
        assert_ne!(obscured_pass, "my-cleartext-webdav-pw");

        let sftp_remote = remotes
            .iter()
            .find(|r| r.name == "test_sftp_keyonly")
            .context("missing test_sftp_keyonly")?;
        assert!(sftp_remote.raw.get("pass").is_none());

        let drive_remote = remotes
            .iter()
            .find(|r| r.name == "test_drive_token")
            .context("missing test_drive_token")?;
        assert!(drive_remote
            .raw
            .get("token")
            .and_then(Value::as_str)
            .is_some_and(|t| t.contains("ya29.unit")));

        #[cfg(unix)]
        assert_mode_0600(&conf_path)?;
        Ok(())
    }

    #[test]
    fn setup_remote_crypt_create_stream_reconnect_and_delete_companion() -> Result<()> {
        let _serial = lock_rclone_test();
        let storage_tmp = TempDir::new("backing-storage")?;
        let base_root = storage_tmp.path().join("local_backing");
        fs::create_dir_all(&base_root)?;

        // Phase 1: CreateNew + CryptOptions on initial rclone.conf
        let conf1_tmp = TempDir::new("conf-create")?;
        let conf1_path = conf1_tmp.path().join("rclone.conf");
        init(&conf1_path)?;

        let mut create_params = serde_json::Map::new();
        create_params.insert(
            "Root".to_string(),
            Value::String(base_root.to_string_lossy().into_owned()),
        );
        let created_name = setup_remote(RemoteSetupRequest {
            name: "media_vault".to_string(),
            provider: "local".to_string(),
            parameters: create_params,
            mode: RemoteSetupMode::CreateNew,
            crypt: Some(CryptOptions {
                password: "recovery-passphrase-alpha".to_string(),
                salt: Some("recovery-salt-beta".to_string()),
                folder: "encrypted_folder".to_string(),
            }),
        })?;
        assert_eq!(created_name, "media_vault");
        #[cfg(unix)]
        assert_mode_0600(&conf1_path)?;

        let remotes = list_remotes()?;
        assert!(remotes
            .iter()
            .any(|r| r.name == "media_vault-raw" && r.kind == "local"));
        assert!(remotes
            .iter()
            .any(|r| r.name == "media_vault" && r.kind == "crypt"));

        // Write a binary file through the encrypted remote and verify disk cipher + HTTP stream.
        let plaintext: Vec<u8> = (0..8192usize)
            .map(|i| ((i.wrapping_mul(73).wrapping_add(19)) & 0xff) as u8)
            .collect();
        let src_file = conf1_tmp.path().join("episode.bin");
        fs::write(&src_file, &plaintext)?;

        let jobid = copy_local_to_remote(&src_file, "media_vault:", "series/episode 01.bin")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let p = job_progress(jobid)?;
            if p.finished {
                assert!(p.success && p.error.is_none(), "copy failed: {p:?}");
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for copy job");
            std::thread::sleep(Duration::from_millis(20));
        }

        let vault_subdir = base_root.join("encrypted_folder");
        let mut raw_files = Vec::new();
        collect_files(&vault_subdir, &mut raw_files)?;
        assert!(!raw_files.is_empty(), "expected encrypted files on disk");
        let probe = &plaintext[64..192];
        for rf in &raw_files {
            let disk_bytes = fs::read(rf)?;
            assert!(
                !disk_bytes.windows(probe.len()).any(|w| w == probe),
                "encrypted file {} leaked plaintext bytes",
                rf.display()
            );
        }

        let fetch_http_bytes = |url: &str| -> Result<Vec<u8>> {
            let rest = url
                .strip_prefix("http://")
                .context("expected http:// stream URL")?;
            let (host_port, req_path) = rest.split_once('/').context("expected URL path")?;
            let mut conn = TcpStream::connect(host_port)?;
            let req = format!(
                "GET /{req_path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"
            );
            conn.write_all(req.as_bytes())?;
            let mut resp = Vec::new();
            conn.read_to_end(&mut resp)?;
            let split_pos = resp
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .context("malformed HTTP response")?;
            let headers = String::from_utf8_lossy(&resp[..split_pos]);
            assert!(
                headers.starts_with("HTTP/1.1 200"),
                "expected 200 OK, got:\n{headers}"
            );
            Ok(resp[split_pos + 4..].to_vec())
        };

        let stream_url_1 = file_stream_url("media_vault:", "series/episode 01.bin")?;
        assert_eq!(fetch_http_bytes(&stream_url_1)?, plaintext);

        // Phase 2: Brand-new rclone.conf reconnecting via ConnectExisting + recovery password & salt
        let conf2_tmp = TempDir::new("conf-reconnect")?;
        let conf2_path = conf2_tmp.path().join("rclone.conf");
        init(&conf2_path)?;
        assert!(list_remotes()?.is_empty());

        let mut reconnect_params = serde_json::Map::new();
        reconnect_params.insert(
            "path".to_string(),
            Value::String(base_root.to_string_lossy().into_owned()),
        );
        let reconnected_name = setup_remote(RemoteSetupRequest {
            name: "media_vault".to_string(),
            provider: "local".to_string(),
            parameters: reconnect_params,
            mode: RemoteSetupMode::ConnectExisting,
            crypt: Some(CryptOptions {
                password: "recovery-passphrase-alpha".to_string(),
                salt: Some("recovery-salt-beta".to_string()),
                folder: "encrypted_folder".to_string(),
            }),
        })?;
        assert_eq!(reconnected_name, "media_vault");
        #[cfg(unix)]
        assert_mode_0600(&conf2_path)?;

        let listed = list_remote_files("media_vault:", "series")?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "episode 01.bin");
        assert_eq!(listed[0].size, plaintext.len() as i64);

        let stream_url_2 = file_stream_url("media_vault:", "series/episode 01.bin")?;
        assert_eq!(fetch_http_bytes(&stream_url_2)?, plaintext);

        // Phase 3: delete_remote removes BOTH {name} and its companion {name}-raw
        delete_remote("media_vault")?;
        #[cfg(unix)]
        assert_mode_0600(&conf2_path)?;
        let remaining = list_remotes()?;
        assert!(
            !remaining
                .iter()
                .any(|r| r.name == "media_vault" || r.name == "media_vault-raw"),
            "expected both media_vault and media_vault-raw to be deleted, got {remaining:?}"
        );

        // Also verify cloud remote target formatting & companion deletion.
        let mut s3_params = serde_json::Map::new();
        s3_params.insert("provider".into(), json!("AWS"));
        s3_params.insert("access_key_id".into(), json!("AKIAEXAMPLE"));
        s3_params.insert("secret_access_key".into(), json!("SECRETEXAMPLE"));
        setup_remote(RemoteSetupRequest {
            name: "cloud_crypt".to_string(),
            provider: "s3".to_string(),
            parameters: s3_params,
            mode: RemoteSetupMode::CreateNew,
            crypt: Some(CryptOptions {
                password: "cloud-pw".to_string(),
                salt: None,
                folder: "/my-bucket/rstream-vault/".to_string(),
            }),
        })?;
        let after_s3 = list_remotes()?;
        let cloud_crypt = after_s3
            .iter()
            .find(|r| r.name == "cloud_crypt")
            .context("missing cloud_crypt")?;
        assert_eq!(
            cloud_crypt.raw.get("remote").and_then(Value::as_str),
            Some("cloud_crypt-raw:my-bucket/rstream-vault")
        );
        delete_remote("cloud_crypt")?;
        assert!(!list_remotes()?
            .iter()
            .any(|r| r.name == "cloud_crypt" || r.name == "cloud_crypt-raw"));

        Ok(())
    }

    #[test]
    fn setup_remote_oauth_invokes_open_url_handler_and_cancels_cleanly() -> Result<()> {
        static CAPTURED_URLS: Mutex<Vec<String>> = Mutex::new(Vec::new());

        fn record_and_cancel(url: &str) -> Result<()> {
            CAPTURED_URLS.lock().unwrap().push(url.to_string());
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(25));
                cancel_oauth();
            });
            Ok(())
        }

        fn fail_browser(_url: &str) -> Result<()> {
            bail!("no browser installed on device")
        }

        let _serial = lock_rclone_test();
        let conf_tmp = TempDir::new("conf-oauth-cb")?;
        init(&conf_tmp.path().join("rclone.conf"))?;
        CAPTURED_URLS.lock().unwrap().clear();

        struct ResetGuard;
        impl Drop for ResetGuard {
            fn drop(&mut self) {
                set_open_url_handler(None);
            }
        }
        let _reset = ResetGuard;

        set_open_url_handler(Some(record_and_cancel));
        let mut params = serde_json::Map::new();
        params.insert("authorize_oauth".into(), Value::Bool(true));
        let res = setup_remote(RemoteSetupRequest {
            name: "test_gdrive_oauth".to_string(),
            provider: "drive".to_string(),
            parameters: params.clone(),
            mode: RemoteSetupMode::CreateNew,
            crypt: None,
        });
        assert!(res.is_err(), "cancelled OAuth must return Err");
        let urls = CAPTURED_URLS.lock().unwrap().clone();
        assert_eq!(urls.len(), 1, "expected OpenURL callback once, got {urls:?}");
        assert!(
            urls[0].starts_with("http://127.0.0.1:53682/auth?state="),
            "unexpected OAuth URL: {}",
            urls[0]
        );

        // Immediate retry must bind 127.0.0.1:53682 cleanly and abort without hanging if browser launch fails.
        set_open_url_handler(Some(fail_browser));
        let err_msg = setup_remote(RemoteSetupRequest {
            name: "test_gdrive_oauth".to_string(),
            provider: "drive".to_string(),
            parameters: params,
            mode: RemoteSetupMode::CreateNew,
            crypt: None,
        })
        .expect_err("failed browser launch must abort OAuth")
        .to_string();
        assert!(
            err_msg.contains("no browser installed on device") || err_msg.contains("browser_open_failed"),
            "expected browser error detail in `{err_msg}`"
        );

        Ok(())
    }
}
