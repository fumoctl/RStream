//! Isolated, single-lease BitTorrent v1 streaming and download adapter.
//! No remote-control API, persistent DHT, automatic resume, UPnP, or rolling cache.
use anyhow::{anyhow, bail, ensure, Context, Result};
use fs2::FileExt;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerMode,
    ListenerOptions, ManagedTorrent, Session, SessionOptions,
};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    fs::{File, OpenOptions},
    future::Future,
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;
const MAX_FILES: usize = 4096;
const MAX_PIECE: u64 = 128 * 1024 * 1024;
const MAX_HTTP_CLIENTS: usize = 4;
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
const VPN_WATCH_INTERVAL: Duration = Duration::from_millis(500);
const LEAK_PROBE_DURATION: Duration = Duration::from_secs(60);
const LEAK_PROBE_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(15);

const PUBLIC_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "http://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://opentracker.io:6969/announce",
    "udp://explodie.org:6969/announce",
    "udp://tracker.dler.org:6969/announce",
    "https://tracker.tamersunion.org:443/announce",
];

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "m2ts", "vob",
    "ogv", "3gp",
];

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeakProbeStatus {
    pub active: bool,
    pub bound_interface: Option<String>,
    pub trackers_total: usize,
    pub trackers_contacted: usize,
    pub trackers_ok: usize,
    pub remaining_secs: u64,
    pub summary: String,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    pub addrs: Vec<String>,
    pub is_up: bool,
    pub is_vpn_like: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VpnPolicy {
    pub killswitch: bool,
    pub bind_interface: Option<String>,
}

static ANDROID_VPN_STATE: StdMutex<(bool, Option<String>)> = StdMutex::new((false, None));

#[cfg(test)]
static TEST_INTERFACES_OVERRIDE: StdMutex<Option<Vec<NetworkInterface>>> = StdMutex::new(None);

pub fn set_android_vpn_state(active: bool, iface_name: Option<String>) {
    let iface = iface_name.and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_owned())
    });
    if let Ok(mut guard) = ANDROID_VPN_STATE.lock() {
        *guard = (active, iface);
    }
}

pub fn android_vpn_state() -> (bool, Option<String>) {
    ANDROID_VPN_STATE
        .lock()
        .map(|g| g.clone())
        .unwrap_or((false, None))
}

fn is_vpn_interface_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["tun", "tap", "wg", "utun", "ppp"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
        || [
            "wireguard",
            "wintun",
            "openvpn",
            "tap-windows",
            "proton",
            "mullvad",
            "nordlynx",
        ]
        .iter()
        .any(|kw| lower.contains(kw))
}

#[cfg(unix)]
pub fn list_interfaces() -> Vec<NetworkInterface> {
    use std::collections::BTreeMap;
    struct IfAddrsGuard(*mut libc::ifaddrs);
    impl Drop for IfAddrsGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` was returned by a successful `libc::getifaddrs` call.
                unsafe { libc::freeifaddrs(self.0) };
            }
        }
    }

    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `head` is a valid out-pointer for `getifaddrs`.
    if unsafe { libc::getifaddrs(&mut head) } != 0 || head.is_null() {
        return Vec::new();
    }
    let _guard = IfAddrsGuard(head);

    let mut map: BTreeMap<String, (u32, Vec<String>)> = BTreeMap::new();
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: `cur` is a non-null node in the `getifaddrs` linked list until `freeifaddrs`.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_name.is_null() {
            continue;
        }
        // SAFETY: `ifa_name` is a NUL-terminated C string when non-null.
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .into_owned();
        if name.is_empty() {
            continue;
        }
        #[allow(clippy::unnecessary_cast)]
        let flags = ifa.ifa_flags as u32;
        let entry = map.entry(name).or_insert((0, Vec::new()));
        entry.0 |= flags;

        if !ifa.ifa_addr.is_null() {
            // SAFETY: `ifa_addr` is non-null and points to a `sockaddr` header.
            let family = unsafe { (*ifa.ifa_addr).sa_family as i32 };
            let ip = match family {
                libc::AF_INET => {
                    // SAFETY: `sa_family == AF_INET` guarantees `sockaddr_in` layout.
                    let sin = unsafe {
                        std::ptr::read_unaligned(ifa.ifa_addr.cast::<libc::sockaddr_in>())
                    };
                    let v4 = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    (!v4.is_loopback() && !v4.is_unspecified()).then(|| v4.to_string())
                }
                libc::AF_INET6 => {
                    // SAFETY: `sa_family == AF_INET6` guarantees `sockaddr_in6` layout.
                    let sin6 = unsafe {
                        std::ptr::read_unaligned(ifa.ifa_addr.cast::<libc::sockaddr_in6>())
                    };
                    let v6 = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                    (!v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local())
                        .then(|| v6.to_string())
                }
                _ => None,
            };
            if let Some(ip) = ip {
                if !entry.1.contains(&ip) {
                    entry.1.push(ip);
                }
            }
        }
    }

    #[allow(clippy::unnecessary_cast)]
    let iff_loopback = libc::IFF_LOOPBACK as u32;
    #[allow(clippy::unnecessary_cast)]
    let iff_up = libc::IFF_UP as u32;
    #[allow(clippy::unnecessary_cast)]
    let iff_running = libc::IFF_RUNNING as u32;

    map.into_iter()
        .filter(|(name, (flags, _))| (*flags & iff_loopback) == 0 && name != "lo")
        .map(|(name, (flags, addrs))| {
            let is_up = (flags & iff_up) != 0 && ((flags & iff_running) != 0 || !addrs.is_empty());
            let is_vpn_like = is_vpn_interface_name(&name);
            NetworkInterface {
                name,
                addrs,
                is_up,
                is_vpn_like,
            }
        })
        .collect()
}

#[cfg(windows)]
pub fn list_interfaces() -> Vec<NetworkInterface> {
    use core::ffi::{c_char, c_void};

    const AF_UNSPEC: u32 = 0;
    const AF_INET: u16 = 2;
    const AF_INET6: u16 = 23;
    const GAA_FLAGS: u32 = 0x0002 | 0x0004 | 0x0008; // SKIP_ANYCAST | SKIP_MULTICAST | SKIP_DNS_SERVER
    const ERROR_SUCCESS: u32 = 0;
    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
    const IF_OPER_STATUS_UP: i32 = 1;

    #[repr(C)]
    #[allow(dead_code)]
    struct WinSockAddr {
        sa_family: u16,
        sa_data: [u8; 14],
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct WinSockAddrIn {
        sin_family: u16,
        sin_port: u16,
        sin_addr: [u8; 4],
        sin_zero: [u8; 8],
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct WinSockAddrIn6 {
        sin6_family: u16,
        sin6_port: u16,
        sin6_flowinfo: u32,
        sin6_addr: [u8; 16],
        sin6_scope_id: u32,
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct WinSocketAddress {
        lp_socket_address: *const WinSockAddr,
        i_socket_address_length: i32,
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct WinAdapterUnicastAddress {
        alignment: u64,
        next: *const Self,
        address: WinSocketAddress,
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct WinAdapterAddresses {
        alignment: u64,
        next: *const Self,
        adapter_name: *const c_char,
        first_unicast_address: *const WinAdapterUnicastAddress,
        first_anycast_address: *const c_void,
        first_multicast_address: *const c_void,
        first_dns_server_address: *const c_void,
        dns_suffix: *const u16,
        description: *const u16,
        friendly_name: *const u16,
        physical_address: [u8; 8],
        physical_address_length: u32,
        flags: u32,
        mtu: u32,
        if_type: u32,
        oper_status: i32,
    }

    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetAdaptersAddresses(
            family: u32,
            flags: u32,
            reserved: *mut c_void,
            adapter_addresses: *mut WinAdapterAddresses,
            size_pointer: *mut u32,
        ) -> u32;
    }

    unsafe fn pwstr_to_string(ptr: *const u16) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        while len < 1024 && *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
            .trim()
            .to_owned()
    }

    let mut size: u32 = 32 * 1024;
    let mut backing: Vec<u64> = Vec::new();
    let mut rc = ERROR_BUFFER_OVERFLOW;
    for _ in 0..3 {
        backing.resize((size as usize).div_ceil(8).max(1), 0);
        // SAFETY: `backing` is 8-byte aligned and holds at least `size` bytes.
        rc = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC,
                GAA_FLAGS,
                std::ptr::null_mut(),
                backing.as_mut_ptr().cast::<WinAdapterAddresses>(),
                &mut size,
            )
        };
        if rc != ERROR_BUFFER_OVERFLOW {
            break;
        }
    }
    if rc != ERROR_SUCCESS || backing.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut cur = backing.as_ptr().cast::<WinAdapterAddresses>();
    while !cur.is_null() {
        // SAFETY: `cur` points into `backing` populated by `GetAdaptersAddresses`.
        let adapter = unsafe { &*cur };
        cur = adapter.next;
        if adapter.if_type == IF_TYPE_SOFTWARE_LOOPBACK {
            continue;
        }

        let adapter_name = if adapter.adapter_name.is_null() {
            String::new()
        } else {
            // SAFETY: `adapter_name` is a NUL-terminated ANSI string when non-null.
            unsafe { std::ffi::CStr::from_ptr(adapter.adapter_name) }
                .to_string_lossy()
                .trim()
                .to_owned()
        };
        // SAFETY: `friendly_name` and `description` are NUL-terminated UTF-16 strings when non-null.
        let friendly = unsafe { pwstr_to_string(adapter.friendly_name) };
        let description = unsafe { pwstr_to_string(adapter.description) };
        let name = if !friendly.is_empty() {
            friendly
        } else if !adapter_name.is_empty() {
            adapter_name.clone()
        } else {
            continue;
        };

        let mut addrs = Vec::new();
        let mut uni = adapter.first_unicast_address;
        while !uni.is_null() {
            // SAFETY: `uni` walks the unicast address list inside `backing`.
            let node = unsafe { &*uni };
            uni = node.next;
            let sa = node.address.lp_socket_address;
            let sa_len = node.address.i_socket_address_length as usize;
            if sa.is_null() || sa_len < std::mem::size_of::<u16>() {
                continue;
            }
            // SAFETY: `sa` has at least 2 bytes for `sa_family`.
            let family = unsafe { (*sa).sa_family };
            let ip = if family == AF_INET && sa_len >= std::mem::size_of::<WinSockAddrIn>() {
                // SAFETY: `sa_len` covers `WinSockAddrIn`.
                let sin = unsafe { std::ptr::read_unaligned(sa.cast::<WinSockAddrIn>()) };
                let v4 = std::net::Ipv4Addr::from(sin.sin_addr);
                (!v4.is_loopback() && !v4.is_unspecified()).then(|| v4.to_string())
            } else if family == AF_INET6 && sa_len >= std::mem::size_of::<WinSockAddrIn6>() {
                // SAFETY: `sa_len` covers `WinSockAddrIn6`.
                let sin6 = unsafe { std::ptr::read_unaligned(sa.cast::<WinSockAddrIn6>()) };
                let v6 = std::net::Ipv6Addr::from(sin6.sin6_addr);
                (!v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local())
                    .then(|| v6.to_string())
            } else {
                None
            };
            if let Some(ip) = ip {
                if !addrs.contains(&ip) {
                    addrs.push(ip);
                }
            }
        }

        let is_up = adapter.oper_status == IF_OPER_STATUS_UP;
        let is_vpn_like = is_vpn_interface_name(&name)
            || is_vpn_interface_name(&adapter_name)
            || is_vpn_interface_name(&description);
        out.push(NetworkInterface {
            name,
            addrs,
            is_up,
            is_vpn_like,
        });
    }
    out
}

#[cfg(not(any(unix, windows)))]
pub fn list_interfaces() -> Vec<NetworkInterface> {
    Vec::new()
}

#[cfg(not(target_os = "android"))]
fn has_usable_addr(iface: &NetworkInterface) -> bool {
    iface.addrs.iter().any(|a| {
        if let Ok(v4) = a.parse::<std::net::Ipv4Addr>() {
            !v4.is_loopback() && !v4.is_unspecified()
        } else if let Ok(v6) = a.parse::<std::net::Ipv6Addr>() {
            !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local()
        } else {
            false
        }
    })
}

fn current_interfaces() -> Vec<NetworkInterface> {
    #[cfg(test)]
    if let Ok(guard) = TEST_INTERFACES_OVERRIDE.lock() {
        if let Some(override_ifaces) = guard.as_ref() {
            return override_ifaces.clone();
        }
    }
    list_interfaces()
}

fn normalize_policy(mut policy: VpnPolicy) -> VpnPolicy {
    policy.bind_interface = policy.bind_interface.and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_owned())
    });
    policy
}

#[cfg(not(target_os = "android"))]
fn resolve_bind_device(policy: &VpnPolicy) -> Result<Option<String>> {
    let ifaces = current_interfaces();
    if let Some(iface) = policy.bind_interface.as_deref() {
        let found = ifaces
            .iter()
            .find(|i| i.name == iface && i.is_up && has_usable_addr(i));
        if found.is_none() {
            if policy.killswitch {
                bail!("VPN killswitch blocked torrent: bound interface '{iface}' is not active");
            }
            bail!("bound network interface '{iface}' is not up or has no addresses");
        }
        return Ok(Some(iface.to_owned()));
    }
    if policy.killswitch {
        let vpn = ifaces
            .iter()
            .find(|i| i.is_vpn_like && i.is_up && has_usable_addr(i))
            .context("VPN killswitch blocked torrent: no active VPN tunnel found")?;
        return Ok(Some(vpn.name.clone()));
    }
    Ok(None)
}

#[cfg(target_os = "android")]
fn resolve_bind_device(policy: &VpnPolicy) -> Result<Option<String>> {
    if policy.killswitch || policy.bind_interface.is_some() {
        let (active, current_iface) = android_vpn_state();
        ensure!(
            active,
            "VPN killswitch blocked torrent: no active VPN tunnel found"
        );
        if let Some(expected) = policy.bind_interface.as_deref() {
            ensure!(
                current_iface.as_deref() == Some(expected),
                "VPN killswitch blocked torrent: active VPN interface does not match '{expected}'"
            );
        }
    }
    // Unprivileged Android apps cannot use SO_BINDTODEVICE; ConnectivityManager gates tunnel state.
    Ok(None)
}

fn is_vpn_policy_satisfied(policy: &VpnPolicy, bound_iface: Option<&str>) -> bool {
    #[cfg(not(target_os = "android"))]
    {
        let ifaces = current_interfaces();
        if let Some(iface) = bound_iface {
            return ifaces
                .iter()
                .any(|i| i.name == iface && i.is_up && has_usable_addr(i));
        }
        if policy.killswitch {
            return ifaces
                .iter()
                .any(|i| i.is_vpn_like && i.is_up && has_usable_addr(i));
        }
        true
    }
    #[cfg(target_os = "android")]
    {
        let _ = bound_iface;
        if policy.killswitch || policy.bind_interface.is_some() {
            let (active, current_iface) = android_vpn_state();
            if !active {
                return false;
            }
            if let Some(expected) = policy.bind_interface.as_deref() {
                return current_iface.as_deref() == Some(expected);
            }
        }
        true
    }
}

fn spawn_vpn_watcher(
    policy: VpnPolicy,
    bound_iface: Option<String>,
    token: CancellationToken,
    session: Arc<Session>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(VPN_WATCH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = token.cancelled() => break,
                _ = interval.tick() => {
                    if !is_vpn_policy_satisfied(&policy, bound_iface.as_deref()) {
                        token.cancel();
                        session.stop().await;
                        break;
                    }
                }
            }
        }
    })
}

#[derive(Clone, Debug)]
pub struct TorrentConfig {
    pub cache_budget_bytes: u64,
    pub free_space_reserve_bytes: u64,
    pub metadata_timeout: Duration,
}
impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            cache_budget_bytes: 64 * GIB,
            free_space_reserve_bytes: GIB,
            metadata_timeout: Duration::from_secs(90),
        }
    }
}

pub struct TorrentEngine {
    cache_dir: PathBuf,
    config: TorrentConfig,
    state: Arc<Mutex<Option<Active>>>,
    cancellation: StdMutex<CancellationEpoch>,
    vpn: StdMutex<VpnState>,
    leak_probe: Arc<StdMutex<LeakProbeInner>>,
    runtime: StdMutex<Option<tokio::runtime::Handle>>,
    reserved_bytes: Arc<AtomicU64>,
}

#[derive(Default)]
struct LeakProbeInner {
    status: LeakProbeStatus,
    expires_at: Option<Instant>,
    cancel: Option<CancellationToken>,
    run_id: u64,
}

struct VpnState {
    policy: VpnPolicy,
    active_tokens: Vec<CancellationToken>,
}

struct SpaceReservation {
    counter: Arc<AtomicU64>,
    bytes: u64,
}

impl Drop for SpaceReservation {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

// All prepares register against the epoch BEFORE waiting for state. A stop keeps
// that epoch cancelled until every overlapping stop has left (even if aborted).
// Never hold this synchronous mutex across an await or while acquiring state.
struct CancellationEpoch {
    token: CancellationToken,
    stops: usize,
}
struct StopGuard<'a>(&'a StdMutex<CancellationEpoch>);
impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut epoch) = self.0.lock() {
            epoch.stops -= 1;
            if epoch.stops == 0 {
                epoch.token = CancellationToken::new();
            }
        }
    }
}
struct Active {
    lease: Lease,
    torrent: Arc<ManagedTorrent>,
    server: JoinHandle<()>,
    response: Value,
}
enum LeaseCleanup {
    Ephemeral,
    Resumable {
        stop_token: CancellationToken,
        keep_on_drop: Arc<AtomicBool>,
    },
    Handled,
}
struct Lease {
    dir: PathBuf,
    lock: Option<File>,
    token: CancellationToken,
    session: Option<Arc<Session>>,
    watcher: Option<JoinHandle<()>>,
    runtime: Option<tokio::runtime::Handle>,
    space_reservation: Option<SpaceReservation>,
    cleanup: LeaseCleanup,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.token.cancel();
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        let session = self.session.take();
        let lock = self.lock.take();
        let space_reservation = self.space_reservation.take();
        let dir = self.dir.clone();
        let remove_dir = match &self.cleanup {
            LeaseCleanup::Ephemeral => true,
            LeaseCleanup::Resumable {
                stop_token,
                keep_on_drop,
            } => stop_token.is_cancelled() && !keep_on_drop.load(Ordering::Relaxed),
            LeaseCleanup::Handled => false,
        };
        if session.is_none() && !remove_dir {
            return;
        }
        // Dropping an in-flight prepare/download must also release networking, not seed forever,
        // even when dropped from Dioxus UI or Android JNI threads outside Tokio.
        let rt_handle = tokio::runtime::Handle::try_current()
            .ok()
            .or_else(|| self.runtime.take());
        if let Some(rt) = rt_handle {
            rt.spawn(async move {
                if let Some(s) = session {
                    s.stop().await;
                }
                drop(space_reservation);
                drop(lock);
                if remove_dir {
                    let _ = tokio::task::spawn_blocking(move || remove_cache_dir(&dir)).await;
                }
            });
        }
        // On runtime shutdown retain the files; `lock` drops here so next prepare reclaims unlocked leases.
    }
}
impl Drop for TorrentEngine {
    fn drop(&mut self) {
        if let Ok(epoch) = self.cancellation.lock() {
            epoch.token.cancel();
        }
        if let Ok(mut vpn) = self.vpn.lock() {
            for token in vpn.active_tokens.drain(..) {
                token.cancel();
            }
        }
        self.stop_leak_probe();
    }
}
impl TorrentEngine {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self::with_config(cache_dir, TorrentConfig::default())
    }
    pub fn with_config(cache_dir: PathBuf, config: TorrentConfig) -> Self {
        Self {
            cache_dir,
            config,
            state: Arc::new(Mutex::new(None)),
            cancellation: StdMutex::new(CancellationEpoch {
                token: CancellationToken::new(),
                stops: 0,
            }),
            vpn: StdMutex::new(VpnState {
                policy: VpnPolicy::default(),
                active_tokens: Vec::new(),
            }),
            leak_probe: Arc::new(StdMutex::new(LeakProbeInner::default())),
            runtime: StdMutex::new(tokio::runtime::Handle::try_current().ok()),
            reserved_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    fn runtime_handle(&self) -> Option<tokio::runtime::Handle> {
        if let Ok(cur) = tokio::runtime::Handle::try_current() {
            if let Ok(mut guard) = self.runtime.lock() {
                *guard = Some(cur.clone());
            }
            return Some(cur);
        }
        self.runtime.lock().ok().and_then(|g| g.clone())
    }

    pub fn set_vpn_policy(&self, policy: VpnPolicy) {
        let policy = normalize_policy(policy);
        let changed = if let Ok(mut vpn) = self.vpn.lock() {
            if vpn.policy == policy {
                false
            } else {
                vpn.policy = policy;
                for token in vpn.active_tokens.drain(..) {
                    token.cancel();
                }
                true
            }
        } else {
            false
        };
        if !changed {
            return;
        }
        if let Ok(mut probe) = self.leak_probe.lock() {
            if probe.status.active {
                if let Some(cancel) = probe.cancel.take() {
                    cancel.cancel();
                }
                probe.status.active = false;
                probe.status.remaining_secs = 0;
                probe.expires_at = None;
                probe.status.error = Some("VPN policy changed; leak probe stopped".into());
                probe.status.summary = "Leak probe stopped: VPN policy changed.".into();
            }
        }
        let rt = self.runtime_handle();
        if let Ok(mut guard) = self.state.try_lock() {
            if guard
                .as_ref()
                .is_some_and(|active| active.lease.token.is_cancelled())
            {
                if let Some(active) = guard.take() {
                    if let Some(rt) = rt {
                        rt.spawn(async move {
                            let _ = shutdown(active).await;
                        });
                    }
                }
            }
        } else if let Some(rt) = rt {
            let state = self.state.clone();
            rt.spawn(async move {
                let mut guard = state.lock().await;
                if guard
                    .as_ref()
                    .is_some_and(|active| active.lease.token.is_cancelled())
                {
                    if let Some(active) = guard.take() {
                        let _ = shutdown(active).await;
                    }
                }
            });
        }
    }

    pub fn vpn_policy(&self) -> VpnPolicy {
        self.vpn
            .lock()
            .map(|vpn| vpn.policy.clone())
            .unwrap_or_default()
    }

    fn register_lease_token(&self) -> Result<CancellationToken> {
        let token = self
            .cancellation
            .lock()
            .map_err(|_| anyhow!("cancellation lock poisoned"))?
            .token
            .child_token();
        let mut vpn = self
            .vpn
            .lock()
            .map_err(|_| anyhow!("vpn policy lock poisoned"))?;
        vpn.active_tokens.retain(|t| !t.is_cancelled());
        vpn.active_tokens.push(token.clone());
        Ok(token)
    }

    /// Accepts Stremio {infoHash: 40hex, fileIdx?: original_index, sources?: ["tracker:..."]}.
    /// Replaces the previous lease; caller must stop on player release/backgrounding.
    pub fn prepare(&self, stream: Value) -> impl std::future::Future<Output = Result<Value>> + '_ {
        let _ = self.runtime_handle();
        // Register cancellation synchronously, before this future can be queued or delayed.
        let registered = parse_input(&stream).and_then(|input| {
            let policy = self.vpn_policy();
            resolve_bind_device(&policy)?;
            let token = self.register_lease_token()?;
            Ok((input, token))
        });
        async move {
            let rt = self.runtime_handle();
            let (input, token) = registered?;
            let mut state = self.state.lock().await;
            ensure!(!token.is_cancelled(), "torrent preparation cancelled");
            if let Some(active) = state.take() {
                shutdown(active).await?;
            }
            // stop can run while old shutdown is awaited. Do not create a new
            // session/lease (or spend the metadata timeout) after that cancellation.
            ensure!(!token.is_cancelled(), "torrent preparation cancelled");
            let policy = self.vpn_policy();
            let bound_device = resolve_bind_device(&policy)?;
            // Directory sweeps and the manager flock are blocking; keep them off
            // the bridge's async workers.
            let mut lease = {
                let (cache, lease_token) = (self.cache_dir.clone(), token.clone());
                tokio::task::spawn_blocking(move || create_lease(&cache, lease_token, rt))
                    .await
                    .context("torrent cache worker failed")??
            };
            let ep_hint = input.episode_hint.as_deref();
            let result = tokio::select! {
                biased;
                _ = token.cancelled() => Err(anyhow!("torrent preparation cancelled")),
                result = tokio::time::timeout(
                    self.config.metadata_timeout,
                    self.admit_torrent(
                        &input,
                        &mut lease,
                        &policy,
                        bound_device,
                        ep_hint,
                        false,
                    ),
                ) => {
                    result.context("torrent metadata/initialization timeout")?
                }
            };
            let (torrent, file_idx, file_name, _rel_path, file_length, reserved) = result?;
            let mime = mime_guess::from_path(&file_name)
                .first_or_octet_stream()
                .to_string();
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
            ensure!(!token.is_cancelled(), "torrent preparation cancelled");
            let authority = listener.local_addr()?.to_string();
            let lease_id = random_token();
            let path = format!("/stream/{lease_id}");
            let response = json!({"url": format!("http://{authority}{path}"), "lease_id": lease_id,
            "mime_type": mime, "file_name": file_name, "file_length": file_length,
            "file_index": file_idx, "info_hash": input.hash, "transport": "bittorrent-v1",
            "cache_reserved_bytes": reserved, "ignored_sources": input.ignored_sources});
            let server = tokio::spawn(serve(
                listener,
                StreamEndpoint {
                    host: authority,
                    path,
                    mime,
                    length: file_length,
                    index: file_idx,
                    torrent: torrent.clone(),
                },
                token,
            ));
            *state = Some(Active {
                lease,
                torrent,
                server,
                response: response.clone(),
            });
            Ok(response)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_resumable(
        &self,
        job_id: &str,
        stream: Value,
        dest_path: &Path,
        episode_hint: Option<&str>,
        on_progress: Arc<dyn Fn(u64, Option<u64>) + Send + Sync>,
        stop_token: CancellationToken,
        keep_partial_on_stop: Arc<AtomicBool>,
    ) -> Result<u64> {
        let rt = self.runtime_handle();
        ensure!(!job_id.trim().is_empty(), "job_id must not be empty");
        let input = parse_input(&stream)?;
        let policy = self.vpn_policy();
        let bound_device = resolve_bind_device(&policy)?;
        let token = self.register_lease_token()?;
        if token.is_cancelled() || stop_token.is_cancelled() {
            if stop_token.is_cancelled() && !keep_partial_on_stop.load(Ordering::Relaxed) {
                let cache = self.cache_dir.clone();
                let job_key = job_id.to_owned();
                let _ = tokio::task::spawn_blocking(move || {
                    cleanup_resumable_in_cache(&cache, &job_key)
                })
                .await;
            }
            bail!("torrent download cancelled");
        }

        let mut lease = {
            let cache = self.cache_dir.clone();
            let job_key = job_id.to_owned();
            let lease_token = token.clone();
            let stop_for_drop = stop_token.clone();
            let keep_flag = keep_partial_on_stop.clone();
            tokio::task::spawn_blocking(move || {
                open_resumable_lease(&cache, &job_key, lease_token, stop_for_drop, keep_flag, rt)
            })
            .await
            .context("torrent cache worker failed")??
        };

        if token.is_cancelled() || stop_token.is_cancelled() {
            lease.cleanup = LeaseCleanup::Handled;
            let dir = lease.dir.clone();
            drop(lease.lock.take());
            if stop_token.is_cancelled() && !keep_partial_on_stop.load(Ordering::Relaxed) {
                let _ = tokio::task::spawn_blocking(move || remove_cache_dir(&dir)).await;
            }
            bail!("torrent download cancelled");
        }

        let ep_hint = episode_hint.or(input.episode_hint.as_deref());
        let lease_token = lease.token.clone();
        let admit_result = tokio::select! {
            biased;
            _ = stop_token.cancelled() => {
                lease_token.cancel();
                if keep_partial_on_stop.load(Ordering::Relaxed) {
                    Err(anyhow!("torrent download paused"))
                } else {
                    Err(anyhow!("torrent download cancelled"))
                }
            }
            _ = lease_token.cancelled() => Err(anyhow!("torrent download cancelled")),
            result = tokio::time::timeout(
                self.config.metadata_timeout,
                self.admit_torrent(&input, &mut lease, &policy, bound_device, ep_hint, true),
            ) => {
                result.context("torrent metadata/initialization timeout")?
            }
        };

        let download_result = match admit_result {
            Ok((torrent, file_idx, _file_name, rel_path, file_length, _reserved)) => {
                let initial_done = torrent
                    .stats()
                    .file_progress
                    .get(file_idx)
                    .copied()
                    .unwrap_or(0)
                    .min(file_length);
                on_progress(initial_done, Some(file_length));
                let completed = torrent.wait_until_completed();
                tokio::pin!(completed);
                let mut tick = tokio::time::interval(Duration::from_millis(250));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut last_reported = initial_done;

                let wait_res = loop {
                    tokio::select! {
                        biased;
                        _ = stop_token.cancelled() => {
                            lease.token.cancel();
                            if keep_partial_on_stop.load(Ordering::Relaxed) {
                                break Err(anyhow!("torrent download paused"));
                            } else {
                                break Err(anyhow!("torrent download cancelled"));
                            }
                        }
                        _ = lease.token.cancelled() => {
                            break Err(anyhow!("torrent download cancelled"));
                        }
                        res = &mut completed => {
                            break res;
                        }
                        _ = tick.tick() => {
                            let stats = torrent.stats();
                            if let Some(err) = stats.error {
                                break Err(anyhow!("torrent download error: {err}"));
                            }
                            let done = stats
                                .file_progress
                                .get(file_idx)
                                .copied()
                                .unwrap_or(0)
                                .min(file_length);
                            if done != last_reported {
                                last_reported = done;
                                on_progress(done, Some(file_length));
                            }
                            if stats.finished {
                                break Ok(());
                            }
                        }
                    }
                };

                wait_res?;
                on_progress(file_length, Some(file_length));

                if let Some(watcher) = lease.watcher.take() {
                    watcher.abort();
                }
                if let Some(session) = lease.session.take() {
                    session.stop().await;
                }

                let src_path = lease.dir.join(&rel_path);
                let dest = dest_path.to_path_buf();
                tokio::task::spawn_blocking(move || move_or_copy_file(&src_path, &dest))
                    .await
                    .context("file placement worker failed")??;
                Ok(file_length)
            }
            Err(e) => Err(e),
        };

        lease.token.cancel();
        if let Some(watcher) = lease.watcher.take() {
            watcher.abort();
        }
        if let Some(session) = lease.session.take() {
            session.stop().await;
        }
        drop(lease.space_reservation.take());
        lease.cleanup = LeaseCleanup::Handled;
        let should_delete = download_result.is_ok()
            || (stop_token.is_cancelled() && !keep_partial_on_stop.load(Ordering::Relaxed));
        let dir = lease.dir.clone();
        drop(lease.lock.take());
        if should_delete {
            let _ = tokio::task::spawn_blocking(move || remove_cache_dir(&dir)).await;
        }

        download_result
    }

    pub fn cleanup_resumable(&self, job_id: &str) -> Result<()> {
        cleanup_resumable_in_cache(&self.cache_dir, job_id)
    }

    pub fn start_leak_probe(&self, magnet_uri: &str) -> Result<()> {
        let parsed = parse_leak_probe_magnet(magnet_uri)?;
        let policy = self.vpn_policy();
        let bound_device = resolve_bind_device(&policy)?;
        #[cfg(target_os = "android")]
        let display_iface = bound_device.clone().or_else(|| android_vpn_state().1);
        #[cfg(not(target_os = "android"))]
        let display_iface = bound_device.clone();

        let http_client = build_leak_probe_http_client(bound_device.as_deref())?;
        let probe_token = self.register_lease_token()?;
        let expires_at = Instant::now() + LEAK_PROBE_DURATION;
        let trackers_total = parsed.trackers.len();
        let iface_label = display_iface.as_deref().unwrap_or("default route");

        let run_id = {
            let mut inner = self
                .leak_probe
                .lock()
                .map_err(|_| anyhow!("leak probe lock poisoned"))?;
            if let Some(prev) = inner.cancel.take() {
                prev.cancel();
            }
            inner.run_id = inner.run_id.wrapping_add(1);
            let run_id = inner.run_id;
            inner.expires_at = Some(expires_at);
            inner.cancel = Some(probe_token.clone());
            inner.status = LeakProbeStatus {
                active: true,
                bound_interface: display_iface.clone(),
                trackers_total,
                trackers_contacted: 0,
                trackers_ok: 0,
                remaining_secs: LEAK_PROBE_DURATION.as_secs(),
                summary: format!(
                    "Probing {trackers_total} tracker(s) via {iface_label} (60s left)…"
                ),
                error: None,
            };
            run_id
        };

        let shared = self.leak_probe.clone();
        let fut = run_leak_probe_task(
            shared,
            run_id,
            parsed,
            policy,
            bound_device,
            display_iface,
            http_client,
            probe_token,
            expires_at,
        );

        if let Some(rt) = self.runtime_handle() {
            rt.spawn(fut);
        } else {
            std::thread::Builder::new()
                .name("rstream-leak-probe".into())
                .spawn(move || {
                    if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        rt.block_on(fut);
                    }
                })
                .context("failed to spawn leak probe worker")?;
        }
        Ok(())
    }

    pub fn stop_leak_probe(&self) {
        if let Ok(mut inner) = self.leak_probe.lock() {
            if let Some(cancel) = inner.cancel.take() {
                cancel.cancel();
            }
            let was_active = inner.status.active;
            inner.status.active = false;
            inner.status.remaining_secs = 0;
            inner.expires_at = None;
            if was_active {
                inner.status.summary = format!(
                    "Leak probe stopped ({}/{} tracker(s) responded).",
                    inner.status.trackers_ok, inner.status.trackers_total
                );
            }
        }
    }

    pub fn leak_probe_status(&self) -> LeakProbeStatus {
        let Ok(mut inner) = self.leak_probe.lock() else {
            return LeakProbeStatus::default();
        };
        if inner.status.active {
            let rem = inner
                .expires_at
                .map(|exp| exp.saturating_duration_since(Instant::now()).as_secs())
                .unwrap_or(0);
            inner.status.remaining_secs = rem;
        }
        inner.status.clone()
    }

    async fn admit_torrent(
        &self,
        input: &Input,
        lease: &mut Lease,
        policy: &VpnPolicy,
        bound_device: Option<String>,
        episode_hint: Option<&str>,
        resumable: bool,
    ) -> Result<(Arc<ManagedTorrent>, usize, String, PathBuf, u64, u64)> {
        let ipv4_only = should_use_ipv4_only(bound_device.as_deref());
        #[cfg(windows)]
        let listen_addr = bound_interface_ip(bound_device.as_deref(), ipv4_only)
            .map(|ip| std::net::SocketAddr::new(ip, 0))
            .unwrap_or_else(|| {
                if ipv4_only {
                    std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0))
                } else {
                    std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
                }
            });
        #[cfg(not(windows))]
        let listen_addr = if ipv4_only {
            std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0))
        } else {
            std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
        };
        #[cfg(windows)]
        let session_bind_device = None;
        #[cfg(not(windows))]
        let session_bind_device = bound_device.clone();

        let session = Session::new_with_opts(
            lease.dir.clone(),
            SessionOptions {
                dht: Some(DhtSessionConfig {
                    persistence: None,
                    ..Default::default()
                }),
                bind_device_name: session_bind_device,
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpAndUtp,
                    listen_addr,
                    enable_upnp_port_forwarding: false,
                    ipv4_only,
                    ..Default::default()
                }),
                ipv4_only,
                disable_local_service_discovery: true,
                cancellation_token: Some(lease.token.clone()),
                concurrent_init_limit: Some(1),
                ..Default::default()
            },
        )
        .await?;
        lease.session = Some(session.clone());

        if policy.killswitch || policy.bind_interface.is_some() {
            lease.watcher = Some(spawn_vpn_watcher(
                policy.clone(),
                bound_device,
                lease.token.clone(),
                session.clone(),
            ));
        }

        let cached_meta_path = lease.dir.join(".rstream.torrent");
        let cached_listed = if resumable {
            match tokio::fs::read(&cached_meta_path).await {
                Ok(bytes) if !bytes.is_empty() && bytes.len() <= 32 * 1024 * 1024 => {
                    match session
                        .add_torrent(
                            AddTorrent::from_bytes(bytes),
                            Some(AddTorrentOptions {
                                list_only: true,
                                ..Default::default()
                            }),
                        )
                        .await
                    {
                        Ok(AddTorrentResponse::ListOnly(r))
                            if r.info_hash.as_string().eq_ignore_ascii_case(&input.hash) =>
                        {
                            Some(r)
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        } else {
            None
        };

        // Resolve metadata without creating/admitting torrent payload files.
        let listed = match cached_listed {
            Some(r) => r,
            None => {
                let resp = session
                    .add_torrent(
                        AddTorrent::from_url(input.magnet.clone()),
                        Some(AddTorrentOptions {
                            list_only: true,
                            ..Default::default()
                        }),
                    )
                    .await?;
                match resp {
                    AddTorrentResponse::ListOnly(r) => r,
                    _ => bail!("unexpected metadata response"),
                }
            }
        };
        ensure!(
            listed.torrent_bytes.len() <= 32 * 1024 * 1024,
            "torrent metadata exceeds 32 MiB"
        );
        if resumable {
            let _ = tokio::fs::write(&cached_meta_path, &listed.torrent_bytes).await;
        }
        let mut files = Vec::new();
        let mut rel_paths = Vec::new();
        for d in listed.info.iter_file_details() {
            ensure!(
                files.len() < MAX_FILES,
                "too many torrent files (maximum {MAX_FILES})"
            );
            ensure!(
                !d.attrs().symlink && d.symlink_path.is_none(),
                "symlink metadata is not supported"
            );
            let components = d.filename.to_vec();
            validate_components(&components)?;
            files.push((components.join("/"), d.len));
            rel_paths.push(d.filename.to_pathbuf());
        }
        let idx = select_download_file(&files, input.file_idx, episode_hint)?;
        let (name, length) = files[idx].clone();
        let rel_path = rel_paths[idx].clone();
        ensure!(length > 0, "selected file is empty");
        let piece = listed.info.info().piece_length as u64;
        ensure!(
            piece > 0 && piece <= MAX_PIECE,
            "unsupported torrent piece length"
        );
        let reserved = length
            .checked_add(piece.checked_mul(2).context("piece overflow")?)
            .context("length overflow")?;
        ensure!(
            reserved <= self.config.cache_budget_bytes,
            "selected file plus boundary pieces exceeds cache budget"
        );
        let already_allocated = if resumable {
            std::fs::metadata(lease.dir.join(&rel_path))
                .map(|m| m.len().min(reserved))
                .unwrap_or(0)
        } else {
            0
        };
        let needed = reserved.saturating_sub(already_allocated);
        let prev_reserved = self.reserved_bytes.fetch_add(needed, Ordering::AcqRel);
        let reservation = SpaceReservation {
            counter: self.reserved_bytes.clone(),
            bytes: needed,
        };
        let total_in_flight = prev_reserved.saturating_add(needed);
        ensure!(
            total_in_flight <= self.config.cache_budget_bytes,
            "selected file plus boundary pieces exceeds cache budget"
        );
        let free = fs2::available_space(&lease.dir)?;
        ensure!(
            free >= total_in_flight.saturating_add(self.config.free_space_reserve_bytes),
            "insufficient free space (reserve protected)"
        );
        lease.space_reservation = Some(reservation);
        // Reuse verified metadata rather than resolve the magnet twice. Keep original index.
        let added = session
            .add_torrent(
                AddTorrent::from_bytes(listed.torrent_bytes),
                Some(AddTorrentOptions {
                    overwrite: resumable,
                    only_files: Some(vec![idx]),
                    output_folder: Some(lease.dir.to_string_lossy().into_owned()),
                    initial_peers: (!listed.seen_peers.is_empty()).then_some(listed.seen_peers),
                    trackers: (!input.trackers.is_empty()).then_some(input.trackers.clone()),
                    ..Default::default()
                }),
            )
            .await?;
        let torrent = added.into_handle().context("torrent was not admitted")?;
        torrent.wait_until_initialized().await?;
        Ok((torrent, idx, name, rel_path, length, reserved))
    }

    pub async fn stop(&self) -> Result<()> {
        let _ = self.runtime_handle();
        // Registration is synchronous: every prepare already queued, or begun
        // while any stop is waiting/shutting down, observes a cancelled epoch.
        let _stop = {
            let mut epoch = self
                .cancellation
                .lock()
                .map_err(|_| anyhow!("cancellation lock poisoned"))?;
            epoch.stops += 1;
            epoch.token.cancel();
            StopGuard(&self.cancellation)
        };
        if let Ok(mut vpn) = self.vpn.lock() {
            for token in vpn.active_tokens.drain(..) {
                token.cancel();
            }
        }
        self.stop_leak_probe();
        let mut state = self.state.lock().await;
        if let Some(active) = state.take() {
            shutdown(active).await?;
        }
        Ok(())
    }
    /// Release only the lease owned by a retiring Android service, never a newer source.
    /// Unlike stop(), a stale scoped stop must not cancel replacement preparations.
    pub async fn stop_lease(&self, lease_id: &str) -> Result<()> {
        let _ = self.runtime_handle();
        let mut state = self.state.lock().await;
        if state
            .as_ref()
            .is_some_and(|active| active.response["lease_id"].as_str() == Some(lease_id))
        {
            if let Some(active) = state.take() {
                shutdown(active).await?;
            }
        }
        Ok(())
    }
    pub async fn stats(&self) -> Result<Value> {
        let state = self.state.lock().await;
        Ok(match state.as_ref() {
            Some(active) => {
                json!({"active": !active.lease.token.is_cancelled(), "lease_id": active.response["lease_id"],
                "torrent": active.torrent.stats(), "cache_budget_bytes": self.config.cache_budget_bytes})
            }
            None => json!({"active": false, "cache_budget_bytes": self.config.cache_budget_bytes}),
        })
    }
}

async fn shutdown(mut active: Active) -> Result<()> {
    active.lease.token.cancel();
    if let Some(watcher) = active.lease.watcher.take() {
        watcher.abort();
    }
    let _ = active.server.await;
    if let Some(session) = active.lease.session.take() {
        session.stop().await;
    }
    drop(active.lease.space_reservation.take());
    active.lease.cleanup = LeaseCleanup::Handled;
    drop(active.lease.lock.take());
    // remove_dir_all of a multi-GiB cache must not stall an async worker.
    let dir = active.lease.dir.clone();
    tokio::task::spawn_blocking(move || remove_cache_dir(&dir))
        .await
        .context("torrent cache worker failed")??;
    Ok(())
}

fn move_or_copy_file(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            let parent = dest.parent().unwrap_or_else(|| Path::new("."));
            let tmp = parent.join(format!(".rstream-part-{}", random_token()));
            let copy_res = (|| -> Result<()> {
                std::fs::copy(src, &tmp)?;
                File::open(&tmp)?.sync_all()?;
                std::fs::rename(&tmp, dest)?;
                let _ = std::fs::remove_file(src);
                Ok(())
            })();
            if copy_res.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            copy_res
        }
    }
}

struct Input {
    hash: String,
    magnet: String,
    trackers: Vec<String>,
    file_idx: Option<usize>,
    episode_hint: Option<String>,
    ignored_sources: Vec<String>,
}

fn decode_btih(raw: &str) -> Result<[u8; 20]> {
    let s = raw.trim();
    if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        let mut out = [0u8; 20];
        hex::decode_to_slice(s, &mut out)?;
        return Ok(out);
    }
    let unpadded = s.trim_end_matches('=');
    if unpadded.len() == 32 {
        let mut acc: u32 = 0;
        let mut bits: u32 = 0;
        let mut out = [0u8; 20];
        let mut idx = 0;
        for b in unpadded.bytes() {
            let val: u32 = match b {
                b'A'..=b'Z' => (b - b'A') as u32,
                b'a'..=b'z' => (b - b'a') as u32,
                b'2'..=b'7' => (b - b'2' + 26) as u32,
                _ => bail!("invalid Base32 character in infoHash"),
            };
            acc = (acc << 5) | val;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out[idx] = ((acc >> bits) & 0xff) as u8;
                idx += 1;
            }
        }
        if idx == 20 {
            return Ok(out);
        }
    }
    bail!("infoHash must be 40 hexadecimal or 32 Base32 characters (BitTorrent v1)")
}

fn sanitize_tracker_url(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 2048 {
        return None;
    }
    let mut u = url::Url::parse(trimmed).ok()?;
    if !matches!(u.scheme(), "http" | "https" | "udp") {
        return None;
    }
    if u.host_str().is_none() || !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    if u.scheme() == "udp" && u.port().is_none() {
        return None;
    }
    let had_fragment = u.fragment().is_some();
    if had_fragment {
        u.set_fragment(None);
    }
    let canonical = u.to_string();
    let display = if had_fragment {
        canonical.clone()
    } else {
        trimmed.to_owned()
    };
    Some((display, canonical))
}

fn parse_input(stream: &Value) -> Result<Input> {
    let raw_hash = stream
        .get("infoHash")
        .or_else(|| stream.get("magnet"))
        .and_then(Value::as_str)
        .context("torrent stream requires infoHash; direct HTTP is handled by the player")?
        .trim();

    let mut ignored_sources = Vec::new();
    let mut trackers = Vec::new();
    let mut seen_trackers = HashSet::new();

    let add_tracker = |tr: &str,
                       trackers: &mut Vec<String>,
                       seen: &mut HashSet<String>,
                       magnet: &mut url::Url|
     -> bool {
        if let Some((display, canonical)) = sanitize_tracker_url(tr) {
            if seen.insert(canonical) {
                magnet.query_pairs_mut().append_pair("tr", &display);
                trackers.push(display);
            }
            true
        } else {
            false
        }
    };

    let (hash_bytes, magnet_trackers) = if raw_hash
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("magnet:"))
    {
        let parsed_mag = url::Url::parse(raw_hash).context("invalid magnet URI in infoHash")?;
        let mut btih: Option<[u8; 20]> = None;
        let mut m_trs = Vec::new();
        for (k, v) in parsed_mag.query_pairs() {
            if k == "xt" {
                if let Some(rest) = v
                    .strip_prefix("urn:btih:")
                    .or_else(|| v.strip_prefix("URN:BTIH:"))
                {
                    btih = Some(decode_btih(rest)?);
                }
            } else if k == "tr" {
                m_trs.push(v.into_owned());
            }
        }
        (
            btih.context("magnet URI missing xt=urn:btih:<hash>")?,
            m_trs,
        )
    } else {
        (decode_btih(raw_hash)?, Vec::new())
    };

    let hash = hex::encode(hash_bytes);
    let file_idx = match stream.get("fileIdx") {
        None | Some(Value::Null) => None,
        Some(v) => Some(usize::try_from(
            v.as_u64()
                .context("fileIdx must be a nonnegative integer")?,
        )?),
    };
    let episode_hint = stream
        .get("episodeHint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let mut magnet = url::Url::parse(&format!("magnet:?xt=urn:btih:{hash}"))?;
    for tr in magnet_trackers {
        if !add_tracker(&tr, &mut trackers, &mut seen_trackers, &mut magnet) {
            ignored_sources.push(tr);
        }
    }

    if let Some(sources_val) = stream.get("sources") {
        if !sources_val.is_null() {
            let sources = sources_val.as_array().context("sources must be an array")?;
            ensure!(sources.len() <= 256, "too many sources");
            for source in sources {
                let source = source.as_str().context("source must be a string")?;
                let trimmed = source.trim();
                if trimmed.is_empty() || trimmed.len() > 2048 || trimmed.starts_with("dht:") {
                    ignored_sources.push(source.to_owned());
                    continue;
                }
                let candidate = trimmed.strip_prefix("tracker:").unwrap_or(trimmed);
                if !add_tracker(candidate, &mut trackers, &mut seen_trackers, &mut magnet) {
                    ignored_sources.push(source.to_owned());
                }
            }
        }
    }

    for &fallback in PUBLIC_TRACKERS {
        let _ = add_tracker(fallback, &mut trackers, &mut seen_trackers, &mut magnet);
    }

    Ok(Input {
        hash,
        magnet: magnet.into(),
        trackers,
        file_idx,
        episode_hint,
        ignored_sources,
    })
}
fn is_windows_reserved_stem(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_matches(' ');
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn validate_components(components: &[String]) -> Result<()> {
    ensure!(
        !components.is_empty() && components.len() <= 32,
        "invalid metadata path depth"
    );
    for c in components {
        let drive_prefix =
            c.as_bytes().get(1) == Some(&b':') && c.as_bytes()[0].is_ascii_alphabetic();
        ensure!(
            !c.is_empty()
                && c != "."
                && c != ".."
                && c.len() <= 255
                && !drive_prefix
                && !c.ends_with(['.', ' '])
                && !is_windows_reserved_stem(c)
                && !c.contains(['/', '\\', '\0'])
                && !c.chars().any(char::is_control),
            "unsafe torrent metadata path"
        );
    }
    Ok(())
}

fn is_video_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            let lower = ext.to_ascii_lowercase();
            VIDEO_EXTENSIONS.contains(&lower.as_str())
        })
        .unwrap_or(false)
}

fn parse_season_episode(hint: &str) -> Option<(u32, u32)> {
    let lower = hint.to_ascii_lowercase();
    for (idx, ch) in lower.char_indices() {
        if ch == 's' {
            let rest = &lower[idx + 1..];
            let s_digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if s_digits.is_empty() {
                continue;
            }
            let after_s = &rest[s_digits.len()..];
            if let Some(after_sep) = after_s.strip_prefix('e').or_else(|| after_s.strip_prefix('x'))
            {
                let e_digits: String = after_sep
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let (Ok(s), Ok(e)) = (s_digits.parse::<u32>(), e_digits.parse::<u32>()) {
                    return Some((s, e));
                }
            }
        }
        if ch.is_ascii_digit()
            && lower[..idx]
                .chars()
                .next_back()
                .is_none_or(|p| !p.is_ascii_alphanumeric())
        {
            let rest = &lower[idx..];
            let s_digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Some(after_x) = rest[s_digits.len()..].strip_prefix('x') {
                let e_digits: String = after_x
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let (Ok(s), Ok(e)) = (s_digits.parse::<u32>(), e_digits.parse::<u32>()) {
                    return Some((s, e));
                }
            }
        }
    }
    // Also support Stremio video_id hints like "tt0903747:1:2" (<id>:<season>:<episode>).
    let parts: Vec<&str> = lower.split(':').collect();
    if parts.len() >= 3 && parts[0] != "kitsu" {
        if let (Ok(s), Ok(e)) = (
            parts[parts.len() - 2].parse::<u32>(),
            parts[parts.len() - 1].parse::<u32>(),
        ) {
            if (1..=99).contains(&s) && e >= 1 {
                return Some((s, e));
            }
        }
    }
    None
}

fn contains_token_boundary(haystack_lower: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = haystack_lower[start..].find(needle_lower) {
        let abs = start + pos;
        let prev = haystack_lower[..abs].chars().next_back();
        let before_ok = prev.is_none_or(|c| {
            !(c.is_ascii_digit()
                || (needle_lower.starts_with(char::is_alphabetic) && c.is_alphabetic()))
        });
        let after = abs + needle_lower.len();
        let after_ok = haystack_lower[after..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_digit());
        if before_ok && after_ok {
            return true;
        }
        let step = haystack_lower[abs..].chars().next().map_or(1, char::len_utf8);
        start = abs + step;
    }
    false
}

fn matches_episode_hint(path: &str, hint: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if let Some((season, episode)) = parse_season_episode(hint) {
        let patterns = [
            format!("s{season:02}e{episode:02}"),
            format!("s{season}e{episode:02}"),
            format!("s{season:02}e{episode}"),
            format!("{season}x{episode:02}"),
            format!("{season:02}x{episode:02}"),
        ];
        if patterns
            .iter()
            .any(|pat| contains_token_boundary(&lower, pat))
        {
            return true;
        }
    }
    contains_token_boundary(&lower, &hint.to_ascii_lowercase())
}

fn select_download_file(
    files: &[(String, u64)],
    requested: Option<usize>,
    episode_hint: Option<&str>,
) -> Result<usize> {
    ensure!(!files.is_empty(), "torrent contains no files");
    if let Some(i) = requested {
        ensure!(i < files.len(), "fileIdx out of bounds");
        return Ok(i);
    }
    let video_indices: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| is_video_file(name))
        .map(|(i, _)| i)
        .collect();
    let pool: Vec<usize> = if video_indices.is_empty() {
        (0..files.len()).collect()
    } else {
        video_indices
    };
    if let Some(hint) = episode_hint.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(&best_match) = pool
            .iter()
            .filter(|&&i| matches_episode_hint(&files[i].0, hint))
            .max_by_key(|&&i| (files[i].1, std::cmp::Reverse(i)))
        {
            return Ok(best_match);
        }
    }
    Ok(*pool
        .iter()
        .max_by_key(|&&i| (files[i].1, std::cmp::Reverse(i)))
        .unwrap())
}

fn should_use_ipv4_only(bound_device: Option<&str>) -> bool {
    if std::net::UdpSocket::bind((std::net::Ipv6Addr::UNSPECIFIED, 0)).is_err() {
        return true;
    }
    if let Some(dev) = bound_device {
        let ifaces = current_interfaces();
        if let Some(iface) = ifaces.iter().find(|i| i.name == dev) {
            let has_v6 = iface.addrs.iter().any(|a| {
                a.parse::<std::net::Ipv6Addr>().is_ok_and(|v6| {
                    !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local()
                })
            });
            if !has_v6 {
                return true;
            }
        }
    }
    false
}

#[cfg(windows)]
fn bound_interface_ip(bound_device: Option<&str>, ipv4_only: bool) -> Option<std::net::IpAddr> {
    let dev = bound_device?;
    let ifaces = current_interfaces();
    let iface = ifaces.iter().find(|i| i.name == dev)?;
    for raw in &iface.addrs {
        if let Ok(v4) = raw.parse::<std::net::Ipv4Addr>() {
            if !v4.is_loopback() && !v4.is_unspecified() {
                return Some(std::net::IpAddr::V4(v4));
            }
        }
    }
    if !ipv4_only {
        for raw in &iface.addrs {
            if let Ok(v6) = raw.parse::<std::net::Ipv6Addr>() {
                if !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local() {
                    return Some(std::net::IpAddr::V6(v6));
                }
            }
        }
    }
    None
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn resumable_dir_name(job_id: &str) -> String {
    let hex = sha256_hex(job_id.as_bytes());
    format!("resumable-{}", &hex[..48])
}

fn is_resumable_name(name: &str) -> bool {
    name.strip_prefix("resumable-")
        .map(|s| (32..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or(false)
}

fn is_lease_name(name: &str) -> bool {
    name.strip_prefix("lease-")
        .map(|s| s.len() == 48 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or(false)
}

fn remove_cache_dir(path: &Path) -> Result<()> {
    ensure!(
        path.file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| is_lease_name(n) || is_resumable_name(n)),
        "refusing unsafe cache cleanup"
    );
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "unsafe cache directory"
            );
            std::fs::remove_dir_all(path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn cleanup_resumable_in_cache(cache_dir: &Path, job_id: &str) -> Result<()> {
    if job_id.trim().is_empty() {
        return Ok(());
    }
    let root = cache_dir.join("torrent-v1");
    match std::fs::symlink_metadata(&root) {
        Ok(m) => {
            ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "cache root must not be a symlink"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    let root = root.canonicalize()?;
    let manager = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".manager.lock"))?;
    manager.lock_exclusive()?;
    let dir = root.join(resumable_dir_name(job_id));
    let lock_path = dir.join(".lock");
    if let Ok(m) = std::fs::symlink_metadata(&lock_path) {
        ensure!(!m.file_type().is_symlink(), "unsafe resumable lock file");
        let lock = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        ensure!(
            lock.try_lock_exclusive().is_ok(),
            "resumable download directory is currently active"
        );
        drop(lock);
    }
    remove_cache_dir(&dir)
}

fn ensure_cache_root(cache: &Path) -> Result<PathBuf> {
    let root = cache.join("torrent-v1");
    std::fs::create_dir_all(&root)?;
    ensure!(
        !std::fs::symlink_metadata(&root)?.file_type().is_symlink(),
        "cache root must not be a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(root.canonicalize()?)
}

fn open_resumable_lease(
    cache: &Path,
    job_id: &str,
    token: CancellationToken,
    stop_token: CancellationToken,
    keep_on_drop: Arc<AtomicBool>,
    runtime: Option<tokio::runtime::Handle>,
) -> Result<Lease> {
    let root = ensure_cache_root(cache)?;
    let dir = {
        let manager = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(".manager.lock"))?;
        manager.lock_exclusive()?;
        let dir = root.join(resumable_dir_name(job_id));
        std::fs::create_dir_all(&dir)?;
        ensure!(
            !std::fs::symlink_metadata(&dir)?.file_type().is_symlink(),
            "unsafe resumable directory"
        );
        dir
    };
    let lock_path = dir.join(".lock");
    if let Ok(m) = std::fs::symlink_metadata(&lock_path) {
        ensure!(!m.file_type().is_symlink(), "unsafe resumable lock file");
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;
    Ok(Lease {
        dir,
        lock: Some(lock),
        token,
        session: None,
        watcher: None,
        runtime,
        space_reservation: None,
        cleanup: LeaseCleanup::Resumable {
            stop_token,
            keep_on_drop,
        },
    })
}

struct ParsedLeakProbe {
    info_hash: [u8; 20],
    trackers: Vec<url::Url>,
}

fn parse_leak_probe_magnet(magnet_uri: &str) -> Result<ParsedLeakProbe> {
    let trimmed = magnet_uri.trim();
    let parsed = url::Url::parse(trimmed).context("invalid magnet URI")?;
    ensure!(
        parsed.scheme().eq_ignore_ascii_case("magnet"),
        "leak probe requires a magnet:? URI"
    );
    let mut info_hash: Option<[u8; 20]> = None;
    let mut trackers = Vec::new();
    let mut seen = HashSet::new();
    for (k, v) in parsed.query_pairs() {
        if k == "xt" {
            if let Some(btih) = v
                .strip_prefix("urn:btih:")
                .or_else(|| v.strip_prefix("URN:BTIH:"))
            {
                info_hash = Some(decode_btih(btih)?);
            }
        } else if k == "tr" {
            if let Some((_display, canonical)) = sanitize_tracker_url(&v) {
                if seen.insert(canonical.clone()) {
                    if let Ok(u) = url::Url::parse(&canonical) {
                        trackers.push(u);
                    }
                }
            }
        }
    }
    let info_hash = info_hash.context("magnet URI missing xt=urn:btih:<infoHash>")?;
    ensure!(
        !trackers.is_empty(),
        "magnet URI contains no supported HTTP/HTTPS/UDP tracker URLs"
    );
    Ok(ParsedLeakProbe {
        info_hash,
        trackers,
    })
}

fn build_leak_probe_http_client(bound_device: Option<&str>) -> Result<reqwest::Client> {
    #[allow(unused_mut)]
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::limited(3))
        .user_agent("RStream/0.1");
    #[cfg(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "illumos",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "solaris",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    if let Some(iface) = bound_device {
        builder = builder.interface(iface);
    }
    #[cfg(windows)]
    if let Some(ip) = bound_interface_ip(bound_device, should_use_ipv4_only(bound_device)) {
        builder = builder.local_address(ip);
    }
    #[cfg(not(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "illumos",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "solaris",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
        windows,
    )))]
    let _ = bound_device;
    Ok(builder.build()?)
}

fn percent_encode_20(bytes: &[u8; 20]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(60);
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn random_peer_id() -> [u8; 20] {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut out = [0u8; 20];
    out[..8].copy_from_slice(b"-RS0100-");
    let mut rnd = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut rnd);
    for (dst, &b) in out[8..].iter_mut().zip(rnd.iter()) {
        *dst = ALPHABET[(b as usize) % ALPHABET.len()];
    }
    out
}

async fn announce_http_leak_tracker(
    client: &reqwest::Client,
    tracker_url: &url::Url,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
) -> Result<()> {
    let mut u = tracker_url.clone();
    let mut query = format!(
        "info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left=16384&compact=1&event=started",
        percent_encode_20(info_hash),
        percent_encode_20(peer_id),
    );
    if let Some(existing) = u.query() {
        if !existing.is_empty() {
            query.push('&');
            query.push_str(existing);
        }
    }
    u.set_query(Some(&query));
    let resp = client.get(u.as_str()).send().await?;
    ensure!(
        resp.status().is_success(),
        "HTTP tracker returned status {}",
        resp.status()
    );
    let body = resp.bytes().await?;
    ensure!(
        body.len() <= 64 * 1024,
        "HTTP tracker response unexpectedly large"
    );
    ensure!(
        !body.windows(17).any(|w| w == b"14:failure reason"),
        "HTTP tracker returned failure reason"
    );
    Ok(())
}

async fn announce_udp_leak_tracker(
    tracker_url: &url::Url,
    bound_device: Option<&str>,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    key: u32,
) -> Result<()> {
    let host = tracker_url.host_str().context("UDP tracker missing host")?;
    let port = tracker_url.port().context("UDP tracker missing port")?;
    let ipv4_only = should_use_ipv4_only(bound_device);
    let mut resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("failed resolving UDP tracker {host}:{port}"))?
        .filter(|a| !ipv4_only || a.is_ipv4())
        .collect();
    // Prefer IPv4 first when both families exist so IPv4-oriented leak detectors match reliably.
    resolved.sort_by_key(|a| a.is_ipv6());
    let target = *resolved
        .first()
        .context("no usable address for UDP tracker")?;

    #[cfg(windows)]
    let bind_addr: std::net::SocketAddr = bound_interface_ip(bound_device, target.is_ipv4())
        .filter(|ip| ip.is_ipv6() == target.is_ipv6())
        .map(|ip| std::net::SocketAddr::new(ip, 0))
        .unwrap_or_else(|| {
            if target.is_ipv6() {
                (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
            } else {
                (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
            }
        });
    #[cfg(not(windows))]
    let bind_addr: std::net::SocketAddr = if target.is_ipv6() {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let sock = tokio::net::UdpSocket::bind(bind_addr).await?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(iface) = bound_device {
        sock.bind_device(Some(iface.as_bytes()))
            .with_context(|| format!("failed binding UDP socket to interface '{iface}'"))?;
    }
    #[cfg(target_os = "macos")]
    if let Some(iface) = bound_device {
        use std::os::fd::AsRawFd;
        let c_iface = std::ffi::CString::new(iface)
            .with_context(|| format!("invalid interface name '{iface}'"))?;
        // SAFETY: `c_iface` is a valid NUL-terminated string.
        let idx = unsafe { libc::if_nametoindex(c_iface.as_ptr()) };
        ensure!(idx != 0, "unknown network interface '{iface}'");
        let (level, optname) = if target.is_ipv6() {
            (libc::IPPROTO_IPV6, libc::IPV6_BOUND_IF)
        } else {
            (libc::IPPROTO_IP, libc::IP_BOUND_IF)
        };
        // SAFETY: `sock` is a valid UDP socket fd and `idx` is a `c_uint` interface index.
        let rc = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                level,
                optname,
                (&idx as *const libc::c_uint).cast(),
                std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
            )
        };
        ensure!(
            rc == 0,
            "failed binding UDP socket to interface '{iface}': {}",
            std::io::Error::last_os_error()
        );
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
    let _ = bound_device;

    // BEP-15 Step 1: Connect request (16 bytes)
    let tid_connect = rand::thread_rng().next_u32();
    let mut connect_req = [0u8; 16];
    connect_req[0..8].copy_from_slice(&0x41727101980u64.to_be_bytes());
    connect_req[8..12].copy_from_slice(&0u32.to_be_bytes());
    connect_req[12..16].copy_from_slice(&tid_connect.to_be_bytes());
    sock.send_to(&connect_req, target).await?;

    let mut recv_buf = [0u8; 2048];
    let (n_conn, _) = tokio::time::timeout(Duration::from_secs(5), sock.recv_from(&mut recv_buf))
        .await
        .context("UDP tracker connect timeout")??;
    ensure!(n_conn >= 16, "UDP tracker connect response too short");
    let action_conn = u32::from_be_bytes([recv_buf[0], recv_buf[1], recv_buf[2], recv_buf[3]]);
    let rx_tid_conn = u32::from_be_bytes([recv_buf[4], recv_buf[5], recv_buf[6], recv_buf[7]]);
    ensure!(
        rx_tid_conn == tid_connect,
        "UDP tracker connect transaction mismatch"
    );
    if action_conn == 3 {
        let msg = String::from_utf8_lossy(&recv_buf[8..n_conn]);
        bail!("UDP tracker error: {msg}");
    }
    ensure!(
        action_conn == 0,
        "unexpected UDP tracker connect action {action_conn}"
    );
    let connection_id = u64::from_be_bytes([
        recv_buf[8],
        recv_buf[9],
        recv_buf[10],
        recv_buf[11],
        recv_buf[12],
        recv_buf[13],
        recv_buf[14],
        recv_buf[15],
    ]);

    // BEP-15 Step 2: Announce request (98 bytes)
    let tid_ann = rand::thread_rng().next_u32();
    let mut ann_req = [0u8; 98];
    ann_req[0..8].copy_from_slice(&connection_id.to_be_bytes());
    ann_req[8..12].copy_from_slice(&1u32.to_be_bytes()); // ACTION_ANNOUNCE
    ann_req[12..16].copy_from_slice(&tid_ann.to_be_bytes());
    ann_req[16..36].copy_from_slice(info_hash);
    ann_req[36..56].copy_from_slice(peer_id);
    ann_req[56..64].copy_from_slice(&0u64.to_be_bytes()); // downloaded
    ann_req[64..72].copy_from_slice(&16384u64.to_be_bytes()); // left
    ann_req[72..80].copy_from_slice(&0u64.to_be_bytes()); // uploaded
    ann_req[80..84].copy_from_slice(&2u32.to_be_bytes()); // EVENT_STARTED
    ann_req[84..88].copy_from_slice(&0u32.to_be_bytes()); // ip = 0
    ann_req[88..92].copy_from_slice(&key.to_be_bytes());
    ann_req[92..96].copy_from_slice(&(-1i32).to_be_bytes()); // num_want = -1
    ann_req[96..98].copy_from_slice(&6881u16.to_be_bytes()); // port = 6881
    sock.send_to(&ann_req, target).await?;

    let (n_ann, _) = tokio::time::timeout(Duration::from_secs(5), sock.recv_from(&mut recv_buf))
        .await
        .context("UDP tracker announce timeout")??;
    ensure!(n_ann >= 8, "UDP tracker announce response too short");
    let action_ann = u32::from_be_bytes([recv_buf[0], recv_buf[1], recv_buf[2], recv_buf[3]]);
    let rx_tid_ann = u32::from_be_bytes([recv_buf[4], recv_buf[5], recv_buf[6], recv_buf[7]]);
    ensure!(
        rx_tid_ann == tid_ann,
        "UDP tracker announce transaction mismatch"
    );
    if action_ann == 3 {
        let msg = String::from_utf8_lossy(&recv_buf[8..n_ann]);
        bail!("UDP tracker error: {msg}");
    }
    ensure!(
        action_ann == 1 && n_ann >= 20,
        "invalid UDP tracker announce response"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_leak_probe_task(
    shared: Arc<StdMutex<LeakProbeInner>>,
    run_id: u64,
    parsed: ParsedLeakProbe,
    policy: VpnPolicy,
    bound_device: Option<String>,
    display_iface: Option<String>,
    http_client: reqwest::Client,
    cancel: CancellationToken,
    expires_at: Instant,
) {
    let total = parsed.trackers.len();
    let contacted = Arc::new(StdMutex::new(vec![false; total]));
    let succeeded = Arc::new(StdMutex::new(vec![false; total]));
    let peer_id = random_peer_id();
    let key = rand::thread_rng().next_u32();
    let watch_vpn = policy.killswitch || policy.bind_interface.is_some();

    let update_counts = |last_err: Option<String>| {
        let c_count = contacted
            .lock()
            .map(|g| g.iter().filter(|&&v| v).count())
            .unwrap_or(0);
        let ok_count = succeeded
            .lock()
            .map(|g| g.iter().filter(|&&v| v).count())
            .unwrap_or(0);
        if let Ok(mut inner) = shared.lock() {
            if inner.run_id != run_id || !inner.status.active {
                return;
            }
            let rem = expires_at
                .saturating_duration_since(Instant::now())
                .as_secs();
            let iface_label = display_iface.as_deref().unwrap_or("default route");
            inner.status.trackers_contacted = c_count;
            inner.status.trackers_ok = ok_count;
            inner.status.remaining_secs = rem;
            if ok_count > 0 {
                inner.status.error = None;
                inner.status.summary = format!(
                    "Announced to {ok_count}/{total} tracker(s) OK via {iface_label} ({rem}s left) — check ipleak.net."
                );
            } else if let Some(err) = last_err {
                inner.status.error = Some(err);
                inner.status.summary = format!(
                    "Contacted {c_count}/{total} tracker(s) via {iface_label} ({rem}s left); waiting for response…"
                );
            }
        }
    };

    let run_round = || async {
        let mut set = JoinSet::new();
        for (idx, tr) in parsed.trackers.iter().cloned().enumerate() {
            if let Ok(mut c) = contacted.lock() {
                c[idx] = true;
            }
            let client = http_client.clone();
            let bd = bound_device.clone();
            let ih = parsed.info_hash;
            set.spawn(async move {
                let res = if tr.scheme() == "udp" {
                    announce_udp_leak_tracker(&tr, bd.as_deref(), &ih, &peer_id, key).await
                } else {
                    announce_http_leak_tracker(&client, &tr, &ih, &peer_id).await
                };
                (idx, res)
            });
        }
        update_counts(None);
        while let Some(joined) = set.join_next().await {
            if let Ok((idx, res)) = joined {
                match res {
                    Ok(()) => {
                        if let Ok(mut ok) = succeeded.lock() {
                            ok[idx] = true;
                        }
                        update_counts(None);
                    }
                    Err(e) => {
                        update_counts(Some(e.to_string()));
                    }
                }
            }
        }
    };

    let mut announce_tick = tokio::time::interval(LEAK_PROBE_ANNOUNCE_INTERVAL);
    announce_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    announce_tick.tick().await; // Immediate t=0 round handled via select/first call below

    let mut vpn_tick = tokio::time::interval(VPN_WATCH_INTERVAL);
    vpn_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    vpn_tick.tick().await;

    let deadline = tokio::time::sleep_until(tokio::time::Instant::from_std(expires_at));
    tokio::pin!(deadline);

    // Run initial announce round concurrently with cancellation, VPN drop check, and deadline.
    let mut need_round = true;
    loop {
        if watch_vpn && !is_vpn_policy_satisfied(&policy, bound_device.as_deref()) {
            cancel.cancel();
            if let Ok(mut inner) = shared.lock() {
                if inner.run_id == run_id {
                    inner.status.active = false;
                    inner.status.remaining_secs = 0;
                    inner.expires_at = None;
                    inner.cancel = None;
                    inner.status.error =
                        Some("VPN interface dropped; leak probe aborted".into());
                    inner.status.summary = "Leak probe stopped: VPN interface dropped.".into();
                }
            }
            return;
        }

        if need_round {
            need_round = false;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = &mut deadline => {
                    finish_leak_probe_expired(&shared, run_id);
                    return;
                }
                _ = async {
                    if watch_vpn {
                        loop {
                            vpn_tick.tick().await;
                            if !is_vpn_policy_satisfied(&policy, bound_device.as_deref()) {
                                break;
                            }
                        }
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    continue;
                }
                () = run_round() => {}
            }
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = &mut deadline => {
                finish_leak_probe_expired(&shared, run_id);
                return;
            }
            _ = vpn_tick.tick() => {
                if watch_vpn && !is_vpn_policy_satisfied(&policy, bound_device.as_deref()) {
                    continue;
                }
                update_counts(None);
            }
            _ = announce_tick.tick() => {
                need_round = true;
            }
        }
    }

    if let Ok(mut inner) = shared.lock() {
        if inner.run_id == run_id && inner.status.active {
            inner.status.active = false;
            inner.status.remaining_secs = 0;
            inner.expires_at = None;
            inner.cancel = None;
            inner.status.summary = format!(
                "Leak probe stopped ({}/{} tracker(s) responded).",
                inner.status.trackers_ok, inner.status.trackers_total
            );
        }
    }
}

fn finish_leak_probe_expired(shared: &StdMutex<LeakProbeInner>, run_id: u64) {
    if let Ok(mut inner) = shared.lock() {
        if inner.run_id == run_id {
            if let Some(cancel) = inner.cancel.take() {
                cancel.cancel();
            }
            inner.status.active = false;
            inner.status.remaining_secs = 0;
            inner.expires_at = None;
            inner.status.summary = format!(
                "Leak probe finished ({}/{} tracker(s) responded).",
                inner.status.trackers_ok, inner.status.trackers_total
            );
        }
    }
}

fn random_token() -> String {
    let mut b = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}
fn create_lease(
    cache: &Path,
    token: CancellationToken,
    runtime: Option<tokio::runtime::Handle>,
) -> Result<Lease> {
    let root = ensure_cache_root(cache)?;
    let manager = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".manager.lock"))?;
    manager.lock_exclusive()?;
    // Only reclaim recognized, unlocked private lease directories; never active files or resumable-* dirs.
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if !is_lease_name(&entry.file_name().to_string_lossy()) || !entry.file_type()?.is_dir() {
            continue;
        }
        let lock_path = entry.path().join(".lock");
        if std::fs::symlink_metadata(&lock_path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }
        if let Ok(lock) = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
        {
            if lock.try_lock_exclusive().is_ok() {
                drop(lock);
                remove_cache_dir(&entry.path())?;
            }
        }
    }
    let dir = root.join(format!("lease-{}", random_token()));
    std::fs::create_dir(&dir)?;
    let lock = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(dir.join(".lock"))?;
    lock.lock_exclusive()?;
    Ok(Lease {
        dir,
        lock: Some(lock),
        token,
        session: None,
        watcher: None,
        runtime,
        space_reservation: None,
        cleanup: LeaseCleanup::Ephemeral,
    })
}

/// One range only; RFC-style suffix/open-ended ranges; multi-range returns 416.
fn parse_range(value: Option<&str>, length: u64) -> Result<Option<(u64, u64)>> {
    let Some(value) = value else { return Ok(None) };
    let value = value
        .strip_prefix("bytes=")
        .context("unsupported range unit")?;
    ensure!(!value.contains(',') && length > 0, "unsatisfiable range");
    let (start, end) = value.split_once('-').context("invalid range")?;
    if start.is_empty() {
        let count: u64 = end.parse()?;
        ensure!(count > 0, "zero suffix range");
        return Ok(Some((length.saturating_sub(count), length - 1)));
    }
    let start: u64 = start.parse()?;
    ensure!(start < length, "range beyond end");
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>()?.min(length - 1)
    };
    ensure!(start <= end, "reversed range");
    Ok(Some((start, end)))
}
#[derive(Clone)]
struct StreamEndpoint {
    host: String,
    path: String,
    mime: String,
    length: u64,
    index: usize,
    torrent: Arc<ManagedTorrent>,
}
async fn serve(listener: TcpListener, endpoint: StreamEndpoint, cancel: CancellationToken) {
    serve_connections(listener, cancel, move |socket| {
        let endpoint = endpoint.clone();
        async move {
            let _ = handle_http(
                socket,
                &endpoint.host,
                &endpoint.path,
                &endpoint.mime,
                endpoint.length,
                endpoint.index,
                endpoint.torrent,
            )
            .await;
        }
    })
    .await;
}
async fn serve_connections<F, Fut>(listener: TcpListener, cancel: CancellationToken, handle: F)
where
    F: Fn(TcpStream) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut tasks = JoinSet::new();
    // Oldest first. A player abandons the previous range request when it
    // seeks; that handler may sit on a missing piece until the idle timeout.
    // The newest request wins the slot instead of being refused.
    let mut order: VecDeque<tokio::task::AbortHandle> = VecDeque::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            Some(done) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                let id = match done { Ok((id, ())) => id, Err(error) => error.id() };
                order.retain(|task| task.id() != id);
            },
            accepted = listener.accept() => {
                let (socket, peer) = match accepted {
                    Ok(accepted) => accepted,
                    // Transient (EMFILE, ECONNABORTED): keep the lease's listener alive.
                    Err(_) => {
                        tokio::select! { biased; _ = cancel.cancelled() => break, _ = tokio::time::sleep(ACCEPT_BACKOFF) => continue }
                    }
                };
                if !peer.ip().is_loopback() { continue; }
                if order.len() >= MAX_HTTP_CLIENTS {
                    if let Some(oldest) = order.pop_front() { oldest.abort(); }
                }
                let cancel = cancel.clone();
                let handler = handle(socket);
                order.push_back(tasks.spawn(async move {
                    tokio::select! { _ = cancel.cancelled() => (), _ = handler => () }
                }));
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}
async fn error_response(socket: &mut TcpStream, code: &str, extra: &str) -> Result<()> {
    socket
        .write_all(
            format!("HTTP/1.1 {code}\r\nContent-Length: 0\r\nConnection: close\r\n{extra}\r\n")
                .as_bytes(),
        )
        .await?;
    Ok(())
}
async fn handle_http(
    socket: TcpStream,
    host: &str,
    path: &str,
    mime: &str,
    length: u64,
    index: usize,
    torrent: Arc<ManagedTorrent>,
) -> Result<()> {
    handle_http_with_reader(
        socket,
        host,
        path,
        mime,
        length,
        || async move { torrent.stream(index).await },
        BODY_IDLE_TIMEOUT,
    )
    .await
}
async fn handle_http_with_reader<R, Fut, F>(
    mut socket: TcpStream,
    host: &str,
    path: &str,
    mime: &str,
    length: u64,
    open_reader: F,
    body_idle_timeout: Duration,
) -> Result<()>
where
    R: AsyncRead + AsyncSeek + Unpin,
    Fut: Future<Output = Result<R>>,
    F: FnOnce() -> Fut,
{
    let header = tokio::time::timeout(Duration::from_secs(10), async {
        let mut raw = Vec::with_capacity(1024);
        let mut buf = [0u8; 1024];
        let end = loop {
            if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            ensure!(raw.len() < 8192, "HTTP headers too large");
            let n = socket.read(&mut buf).await?;
            ensure!(n > 0, "unexpected EOF reading HTTP headers");
            raw.extend_from_slice(&buf[..n]);
        };
        ensure!(end + 4 <= 8192, "HTTP headers too large");
        ensure!(
            end + 4 == raw.len(),
            "HTTP pipelining or request body is not supported"
        );
        Ok::<_, anyhow::Error>(String::from_utf8(raw)?)
    })
    .await??;
    let mut lines = header.split("\r\n");
    let parts: Vec<_> = lines.next().unwrap_or("").split_whitespace().collect();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" {
        return error_response(&mut socket, "400 Bad Request", "").await;
    }
    let method = parts[0];
    if parts[1] != path {
        return error_response(&mut socket, "404 Not Found", "").await;
    }
    if !matches!(method, "GET" | "HEAD") {
        return error_response(
            &mut socket,
            "405 Method Not Allowed",
            "Allow: GET, HEAD\r\n",
        )
        .await;
    }
    let mut hosts = Vec::new();
    let mut ranges = Vec::new();
    for line in lines.take_while(|s| !s.is_empty()) {
        let Some((key, val)) = line.split_once(':') else {
            return error_response(&mut socket, "400 Bad Request", "").await;
        };
        if key.eq_ignore_ascii_case("host") {
            hosts.push(val.trim());
        }
        if key.eq_ignore_ascii_case("range") {
            ranges.push(val.trim());
        }
        if key.eq_ignore_ascii_case("transfer-encoding")
            || (key.eq_ignore_ascii_case("content-length") && val.trim() != "0")
        {
            return error_response(&mut socket, "400 Bad Request", "").await;
        }
    }
    if hosts != [host] {
        return error_response(&mut socket, "403 Forbidden", "").await;
    }
    let range = if ranges.len() > 1 {
        Err(anyhow!("duplicate range"))
    } else {
        parse_range(ranges.first().copied(), length)
    };
    let range = match range {
        Ok(r) => r,
        Err(_) => {
            return error_response(
                &mut socket,
                "416 Range Not Satisfiable",
                &format!("Content-Range: bytes */{length}\r\nAccept-Ranges: bytes\r\n"),
            )
            .await
        }
    };
    let (start, count, code, extra) = match range {
        Some((s, e)) => (
            s,
            e - s + 1,
            "206 Partial Content",
            format!("Content-Range: bytes {s}-{e}/{length}\r\n"),
        ),
        None => (0, length, "200 OK", String::new()),
    };
    socket.write_all(format!("HTTP/1.1 {code}\r\nContent-Length: {count}\r\nContent-Type: {mime}\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n{extra}\r\n").as_bytes()).await?;
    if method == "GET" {
        let mut reader = open_reader().await?;
        tokio::time::timeout(body_idle_timeout, reader.seek(SeekFrom::Start(start)))
            .await
            .context("torrent body seek idle timeout")??;
        copy_body(&mut reader.take(count), &mut socket, body_idle_timeout).await?;
    }
    socket.shutdown().await?;
    Ok(())
}

// Bound time without progress, NOT total response duration. Checking socket EOF
// would mistake a legitimate TCP write-half-close for a disconnected reader.
// A client gone while pieces are missing therefore releases its slot within
// this deadline; each successful source read / partial socket write resets it.
async fn copy_body<R, W>(reader: &mut R, writer: &mut W, idle: Duration) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = tokio::time::timeout(idle, reader.read(&mut buffer))
            .await
            .context("torrent body read idle timeout")??;
        if read == 0 {
            return Ok(());
        }
        let mut remaining = &buffer[..read];
        while !remaining.is_empty() {
            let written = tokio::time::timeout(idle, writer.write(remaining))
                .await
                .context("torrent body write idle timeout")??;
            ensure!(written > 0, "torrent body socket closed");
            remaining = &remaining[written..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_OVERRIDE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct InterfaceOverrideGuard;
    impl InterfaceOverrideGuard {
        fn set(ifaces: Vec<NetworkInterface>) -> Self {
            *TEST_INTERFACES_OVERRIDE.lock().unwrap() = Some(ifaces);
            Self
        }
        fn update(&self, ifaces: Vec<NetworkInterface>) {
            *TEST_INTERFACES_OVERRIDE.lock().unwrap() = Some(ifaces);
        }
    }
    impl Drop for InterfaceOverrideGuard {
        fn drop(&mut self) {
            *TEST_INTERFACES_OVERRIDE.lock().unwrap() = None;
        }
    }

    #[test]
    fn public_engine_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<TorrentEngine>();
    }
    #[test]
    fn list_interfaces_runs_cleanly() {
        let ifaces = list_interfaces();
        assert!(ifaces.iter().all(|i| i.name != "lo" && !i.name.is_empty()));
    }
    #[test]
    fn vpn_interface_classification() {
        for name in [
            "tun0",
            "tap0",
            "wg0",
            "utun3",
            "ppp0",
            "proton0",
            "mullvad-wg",
            "nordlynx",
            "WG-Test",
            "WireGuard Tunnel",
            "wintun0",
            "OpenVPN TAP-Windows6",
        ] {
            assert!(is_vpn_interface_name(name), "{name} should be vpn-like");
        }
        for name in [
            "eth0",
            "wlan0",
            "enp0s3",
            "docker0",
            "br-123",
            "tailscale0",
            "Tailscale",
        ] {
            assert!(!is_vpn_interface_name(name), "{name} should not be vpn-like");
        }
    }
    #[test]
    fn android_vpn_state_roundtrip() {
        set_android_vpn_state(true, Some("tun0".into()));
        assert_eq!(android_vpn_state(), (true, Some("tun0".into())));
        set_android_vpn_state(false, None);
        assert_eq!(android_vpn_state(), (false, None));
    }
    #[test]
    fn episode_hint_and_video_selection() {
        let files = vec![
            ("Show.S01E01.mkv".into(), 1000),
            ("Show.S01E02.1080p.mkv".into(), 900),
            ("Show.S01E02.srt".into(), 50_000),
            ("Show.S01E020.mkv".into(), 2000),
            ("Extras.iso".into(), 500_000),
        ];
        assert_eq!(
            select_download_file(&files, None, Some("S01E02")).unwrap(),
            1
        );
        assert_eq!(
            select_download_file(&files, None, Some("1x02")).unwrap(),
            1
        );
        assert_eq!(select_download_file(&files, None, None).unwrap(), 3);
        assert_eq!(
            select_download_file(&files, Some(0), Some("S01E02")).unwrap(),
            0
        );
    }
    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100).unwrap(), None);
        for (s, expected) in [
            ("bytes=0-9", (0, 9)),
            ("bytes=90-", (90, 99)),
            ("bytes=-10", (90, 99)),
            ("bytes=-200", (0, 99)),
            ("bytes=99-999", (99, 99)),
        ] {
            assert_eq!(parse_range(Some(s), 100).unwrap(), Some(expected));
        }
        for s in [
            "bytes=100-",
            "bytes=9-1",
            "bytes=-0",
            "bytes=0-1,4-5",
            "items=0-1",
            "bytes=18446744073709551616-",
        ] {
            assert!(parse_range(Some(s), 100).is_err(), "{s}");
        }
        assert!(parse_range(Some("bytes=0-"), 0).is_err());
    }
    #[test]
    fn original_indices() {
        let files = vec![
            ("small".into(), 2),
            ("largest".into(), 20),
            ("tie".into(), 20),
        ];
        assert_eq!(select_download_file(&files, None, None).unwrap(), 1);
        assert_eq!(select_download_file(&files, Some(0), None).unwrap(), 0);
        assert!(select_download_file(&files, Some(3), None).is_err());
    }
    #[test]
    fn tracker_mapping_and_validation() {
        let h = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
        let i = parse_input(&json!({
            "infoHash": h,
            "fileIdx": 7,
            "sources": [
                "tracker:https://example.org/a?x=1&y=2",
                "http://bare-tracker.example.org:8080/announce#frag",
                "tracker:wss://tracker.openwebtorrent.com",
                "tracker:ws://tracker.webtorrent.invalid:80/announce",
                "dht:wrong",
                "tracker:file:///tmp/x",
                "tracker:https://u:p@example.org/x"
            ]
        }))
        .unwrap();
        assert_eq!(i.hash, h.to_ascii_lowercase());
        assert_eq!(i.file_idx, Some(7));
        let u = url::Url::parse(&i.magnet).unwrap();
        let trs: Vec<String> = u
            .query_pairs()
            .filter(|(k, _)| k == "tr")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert!(trs.iter().any(|v| v == "https://example.org/a?x=1&y=2"));
        assert!(trs
            .iter()
            .any(|v| v == "http://bare-tracker.example.org:8080/announce"));
        for &pub_tr in PUBLIC_TRACKERS {
            assert!(
                trs.iter().any(|v| v == pub_tr),
                "missing public fallback tracker {pub_tr}"
            );
        }
        assert_eq!(
            i.ignored_sources,
            vec![
                "tracker:wss://tracker.openwebtorrent.com",
                "tracker:ws://tracker.webtorrent.invalid:80/announce",
                "dht:wrong",
                "tracker:file:///tmp/x",
                "tracker:https://u:p@example.org/x",
            ]
        );

        // Bare infoHash without sources still gets deduplicated public trackers.
        let bare = parse_input(&json!({"infoHash": h})).unwrap();
        let bare_u = url::Url::parse(&bare.magnet).unwrap();
        let bare_trs: Vec<String> = bare_u
            .query_pairs()
            .filter(|(k, _)| k == "tr")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(bare_trs.len(), PUBLIC_TRACKERS.len());

        // 32-char Base32 infoHash AND magnet:?xt=urn:btih:... in infoHash work cleanly.
        // 0x00..0x14 (all zeros) in Base32 is 32 'A's.
        let b32 = parse_input(&json!({"infoHash": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"})).unwrap();
        assert_eq!(b32.hash, "0000000000000000000000000000000000000000");
        let mag_in = parse_input(&json!({
            "infoHash": format!("magnet:?xt=urn:btih:{h}&tr=udp://tracker.opentrackr.org:1337/announce&tr=wss://bad.invalid")
        }))
        .unwrap();
        assert_eq!(mag_in.hash, h.to_ascii_lowercase());
        assert_eq!(mag_in.ignored_sources, vec!["wss://bad.invalid"]);

        // Up to 256 tracker sources accepted, 257 rejected.
        let many_sources: Vec<String> = (0..200)
            .map(|idx| format!("tracker:udp://tracker{idx}.example.org:1337/announce"))
            .collect();
        assert!(parse_input(&json!({"infoHash": h, "sources": many_sources})).is_ok());
        let too_many: Vec<String> = (0..257)
            .map(|idx| format!("tracker:udp://tracker{idx}.example.org:1337/announce"))
            .collect();
        assert!(parse_input(&json!({"infoHash": h, "sources": too_many})).is_err());

        for v in [
            json!({"infoHash": "bad"}),
            json!({"infoHash": h, "fileIdx": -1}),
            json!({"infoHash": h, "sources": "not-an-array"}),
        ] {
            assert!(parse_input(&v).is_err());
        }
    }

    #[test]
    fn defaults_and_stremio_video_id_episode_hint() {
        let cfg = TorrentConfig::default();
        assert_eq!(cfg.cache_budget_bytes, 64 * GIB);
        assert_eq!(cfg.free_space_reserve_bytes, GIB);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let files = vec![
            ("Show.S01E01.mkv".into(), 1000),
            ("Show.S01E02.mkv".into(), 950),
            ("Show.S01E03.mkv".into(), 1100),
        ];
        assert_eq!(
            select_download_file(&files, None, Some("tt0903747:1:2")).unwrap(),
            1
        );
    }
    #[test]
    fn metadata_paths() {
        for s in [
            "..",
            ".",
            "/tmp",
            "a/b",
            "a\\b",
            "C:",
            "a\0b",
            "",
            "CON",
            "con.mkv",
            "PRN",
            "AUX.srt",
            "NUL",
            "COM1.mp4",
            "LPT9.avi",
            "movie.mkv.",
            "movie.mkv ",
        ] {
            assert!(validate_components(&[s.into()]).is_err(), "{s} must fail");
        }
        assert!(validate_components(&["folder".into(), "movie.mkv".into()]).is_ok());
        assert!(validate_components(&[
            "Pioneer One: Season 1".into(),
            "Episode 01: Earthfall.mkv".into()
        ])
        .is_ok());
    }
    #[test]
    fn utf8_episode_hints_and_token_boundaries_never_panic() {
        assert!(!contains_token_boundary("aééé", "éé"));
        assert!(!contains_token_boundary("1éé2", "éé"));
        assert!(contains_token_boundary("アニメ 第01話 1080p.mkv", "第01話"));
        assert_eq!(parse_season_episode("Café.1×02.mkv"), None);
        assert_eq!(parse_season_episode("Café S01E02"), Some((1, 2)));
        assert_eq!(parse_season_episode("アニメ 1x03"), Some((1, 3)));
    }
    #[test]
    fn only_inactive_owned_cache_is_cleaned() {
        let temp = tempfile::tempdir().unwrap();
        let first = create_lease(temp.path(), CancellationToken::new(), None).unwrap();
        let first_path = first.dir.clone();
        let second = create_lease(temp.path(), CancellationToken::new(), None).unwrap();
        assert!(first_path.exists());
        let untouched = temp.path().join("torrent-v1/other");
        std::fs::create_dir(&untouched).unwrap();
        drop(first);
        let third = create_lease(temp.path(), CancellationToken::new(), None).unwrap();
        assert!(!first_path.exists());
        assert!(untouched.exists());
        assert!(second.dir.exists());
        assert!(third.dir.exists());
        assert!(remove_cache_dir(&untouched).is_err());
    }

    #[tokio::test]
    async fn killswitch_rejects_missing_and_link_local_only_interfaces_without_swarm() -> Result<()> {
        let _lock = TEST_OVERRIDE_LOCK.lock().await;
        let _guard = InterfaceOverrideGuard::set(vec![
            NetworkInterface {
                name: "eth0".into(),
                addrs: vec!["192.168.1.10".into()],
                is_up: true,
                is_vpn_like: false,
            },
            NetworkInterface {
                name: "wg-linklocal0".into(),
                addrs: vec!["fe80::1234".into()],
                is_up: true,
                is_vpn_like: true,
            },
        ]);
        let temp = tempfile::tempdir()?;
        let engine = TorrentEngine::new(temp.path().into());
        let stream = json!({"infoHash":"0123456789abcdef0123456789abcdef01234567"});

        engine.set_vpn_policy(VpnPolicy {
            killswitch: true,
            bind_interface: Some("wg-nonexistent0".into()),
        });
        assert!(engine.prepare(stream.clone()).await.is_err());
        let dest = temp.path().join("out.mkv");
        let cancel = CancellationToken::new();
        let keep = Arc::new(AtomicBool::new(false));
        let err = engine
            .download_resumable(
                "job-missing-iface",
                stream.clone(),
                &dest,
                None,
                Arc::new(|_, _| {}),
                cancel.clone(),
                keep.clone(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("wg-nonexistent0"), "{err}");

        // Link-local IPv6 (`fe80::`) alone must NOT satisfy VPN killswitch, and an IPv4+fe80:: VPN must use ipv4_only.
        engine.set_vpn_policy(VpnPolicy {
            killswitch: true,
            bind_interface: None,
        });
        let err = engine.prepare(stream.clone()).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "VPN killswitch blocked torrent: no active VPN tunnel found"
        );
        let err = engine
            .download_resumable(
                "job-no-vpn",
                stream,
                &dest,
                None,
                Arc::new(|_, _| {}),
                cancel,
                keep,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "VPN killswitch blocked torrent: no active VPN tunnel found"
        );
        assert!(!temp.path().join("torrent-v1").exists());

        _guard.update(vec![NetworkInterface {
            name: "wg0".into(),
            addrs: vec!["10.8.0.2".into(), "fe80::abcd".into()],
            is_up: true,
            is_vpn_like: true,
        }]);
        assert!(
            should_use_ipv4_only(Some("wg0")),
            "fe80:: link-local IPv6 must not enable IPv6 on IPv4-only VPNs"
        );
        Ok(())
    }

    async fn offline_fixture() -> Result<(tempfile::TempDir, Arc<Session>, Arc<ManagedTorrent>)> {
        let temp = tempfile::tempdir()?;
        let media = b"0123456789abcdef";
        let file = temp.path().join("fixture.mp4");
        std::fs::write(&file, media)?;
        let meta = librqbit::create_torrent(
            &file,
            librqbit::CreateTorrentOptions {
                piece_length: Some(16384),
                ..Default::default()
            },
            &librqbit::spawn_utils::BlockingSpawner::new(1),
        )
        .await?;
        let session = Session::new_with_opts(
            temp.path().into(),
            SessionOptions {
                dht: None,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await?;
        let torrent = session
            .add_torrent(
                AddTorrent::from_bytes(meta.as_bytes()?),
                Some(AddTorrentOptions {
                    overwrite: true,
                    disable_trackers: true,
                    output_folder: Some(temp.path().to_string_lossy().into()),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("fixture admission")?;
        torrent.wait_until_initialized().await?;
        Ok((temp, session, torrent))
    }

    #[tokio::test]
    async fn mid_session_vpn_drop_cancels_active_lease_under_one_second() -> Result<()> {
        let _lock = TEST_OVERRIDE_LOCK.lock().await;
        let guard = InterfaceOverrideGuard::set(vec![NetworkInterface {
            name: "wg-test0".into(),
            addrs: vec!["10.8.0.2".into()],
            is_up: true,
            is_vpn_like: true,
        }]);
        let (temp, session, torrent) = offline_fixture().await?;
        let engine = TorrentEngine::new(temp.path().into());
        let policy = VpnPolicy {
            killswitch: true,
            bind_interface: Some("wg-test0".into()),
        };
        engine.set_vpn_policy(policy.clone());
        let lease_token = engine.register_lease_token()?;
        let mut lease = create_lease(temp.path(), lease_token.clone(), engine.runtime_handle())?;
        lease.session = Some(session.clone());
        lease.watcher = Some(spawn_vpn_watcher(
            policy,
            Some("wg-test0".into()),
            lease_token.clone(),
            session.clone(),
        ));
        *engine.state.lock().await = Some(Active {
            lease,
            torrent,
            server: tokio::spawn(async {}),
            response: json!({"lease_id": "vpn-test"}),
        });

        assert!(!lease_token.is_cancelled());
        // Simulate VPN tunnel dropping mid-session.
        guard.update(Vec::new());
        tokio::time::timeout(Duration::from_millis(900), lease_token.cancelled())
            .await
            .context("lease token was not cancelled within <1s after VPN drop")?;
        assert!(lease_token.is_cancelled());
        engine.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn policy_change_from_non_tokio_thread_stops_and_removes_active_lease() -> Result<()> {
        let (temp, session, torrent) = offline_fixture().await?;
        let engine = Arc::new(TorrentEngine::new(temp.path().into()));
        let lease_token = engine.register_lease_token()?;
        let mut lease = create_lease(temp.path(), lease_token.clone(), engine.runtime_handle())?;
        lease.session = Some(session);
        let lease_dir = lease.dir.clone();
        *engine.state.lock().await = Some(Active {
            lease,
            torrent,
            server: tokio::spawn(async {}),
            response: json!({"lease_id": "policy-change-test"}),
        });
        assert!(lease_dir.exists());

        let eng_thread = engine.clone();
        std::thread::spawn(move || {
            eng_thread.set_vpn_policy(VpnPolicy {
                killswitch: true,
                bind_interface: Some("wg0".into()),
            });
        })
        .join()
        .unwrap();

        assert!(lease_token.is_cancelled());
        tokio::time::timeout(Duration::from_secs(3), async {
            while lease_dir.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("set_vpn_policy from non-Tokio thread must remove lease-* dir")?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resumable_preserves_partials_on_timeout_and_deletes_on_explicit_cancel() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let engine = TorrentEngine::with_config(
            temp.path().into(),
            TorrentConfig {
                metadata_timeout: Duration::from_millis(350),
                ..Default::default()
            },
        );
        let job_id = "movie|tt0133093|tt0133093";
        let res_dir = temp
            .path()
            .join("torrent-v1")
            .join(resumable_dir_name(job_id));

        // Seed fake partial piece inside resumable-<hash> before running with keep_partial = false.
        {
            let seed_lease = open_resumable_lease(
                temp.path(),
                job_id,
                CancellationToken::new(),
                CancellationToken::new(),
                Arc::new(AtomicBool::new(false)),
                engine.runtime_handle(),
            )?;
            std::fs::write(seed_lease.dir.join("partial.bin"), b"verified-pieces")?;
            // Active lock must reject concurrent cleanup_resumable!
            assert!(engine.cleanup_resumable(job_id).is_err());
        }
        assert!(res_dir.join("partial.bin").exists());

        // 1. Metadata timeout (non-user error) with keep_partial_on_stop = false MUST preserve partial.bin!
        let uncancelled_stop = CancellationToken::new();
        let err = engine
            .download_resumable(
                job_id,
                json!({
                    "infoHash": "0123456789abcdef0123456789abcdef01234567",
                    "sources": ["tracker:udp://127.0.0.1:9/announce"]
                }),
                &temp.path().join("nested/video.mkv"),
                None,
                Arc::new(|_, _| {}),
                uncancelled_stop,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("timeout") || err.to_string().contains("cancelled"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(res_dir.join("partial.bin"))?,
            b"verified-pieces",
            "timeout/VPN error must never wipe verified partial pieces"
        );

        // 2. Explicit user cancel (stop_token cancelled + keep_partial = false) MUST delete resumable-*!
        let user_cancel = CancellationToken::new();
        let user_cancel_trigger = user_cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            user_cancel_trigger.cancel();
        });
        let err = engine
            .download_resumable(
                job_id,
                json!({
                    "infoHash": "0123456789abcdef0123456789abcdef01234567",
                    "sources": ["tracker:udp://127.0.0.1:9/announce"]
                }),
                &temp.path().join("nested/video.mkv"),
                Some("S01E02"),
                Arc::new(|_, _| {}),
                user_cancel,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert!(
            !res_dir.exists(),
            "explicit user cancellation must delete resumable-* dir"
        );

        let src = temp.path().join("src.bin");
        let dst = temp.path().join("nested/dir/dst.bin");
        std::fs::write(&src, b"payload")?;
        move_or_copy_file(&src, &dst)?;
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst)?, b"payload");
        Ok(())
    }

    #[test]
    fn leak_probe_expiry_cancels_token_and_space_reservation_rolls_back() {
        let probe = StdMutex::new(LeakProbeInner {
            status: LeakProbeStatus {
                active: true,
                trackers_total: 3,
                trackers_ok: 2,
                ..Default::default()
            },
            expires_at: Some(Instant::now()),
            cancel: None,
            run_id: 7,
        });
        let tok = CancellationToken::new();
        probe.lock().unwrap().cancel = Some(tok.clone());
        finish_leak_probe_expired(&probe, 7);
        assert!(tok.is_cancelled(), "expired leak probe must cancel its token");
        assert!(!probe.lock().unwrap().status.active);

        let counter = Arc::new(AtomicU64::new(0));
        {
            counter.fetch_add(4096, Ordering::AcqRel);
            let _res = SpaceReservation {
                counter: counter.clone(),
                bytes: 4096,
            };
            assert_eq!(counter.load(Ordering::Acquire), 4096);
        }
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offline_verified_file_http_contract() -> Result<()> {
        let (_temp, session, torrent) = offline_fixture().await?;
        let media = b"0123456789abcdef";
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let host = listener.local_addr()?.to_string();
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve(
            listener,
            StreamEndpoint {
                host: host.clone(),
                path: "/stream/test-token".into(),
                mime: "video/mp4".into(),
                length: 16,
                index: 0,
                torrent,
            },
            cancel.clone(),
        ));
        async fn request(host: &str, request: String) -> Result<Vec<u8>> {
            let mut s = TcpStream::connect(host).await?;
            s.write_all(request.as_bytes()).await?;
            // A write-half-close is not a disconnected response reader.
            s.shutdown().await?;
            let mut b = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut b)).await??;
            Ok(b)
        }
        for (method, path, extra, authority, status, body) in [
            (
                "GET",
                "/stream/test-token",
                "",
                host.as_str(),
                "200 OK",
                media.as_slice(),
            ),
            (
                "GET",
                "/stream/test-token",
                "Range: bytes=2-5\r\n",
                host.as_str(),
                "206 Partial Content",
                b"2345".as_slice(),
            ),
            (
                "GET",
                "/stream/test-token",
                "Range: bytes=-3\r\n",
                host.as_str(),
                "206 Partial Content",
                b"def".as_slice(),
            ),
            (
                "HEAD",
                "/stream/test-token",
                "",
                host.as_str(),
                "200 OK",
                b"".as_slice(),
            ),
            (
                "GET",
                "/stream/test-token",
                "Range: bytes=16-\r\n",
                host.as_str(),
                "416 Range Not Satisfiable",
                b"".as_slice(),
            ),
            (
                "GET",
                "/stream/test-token",
                "",
                "evil.example",
                "403 Forbidden",
                b"".as_slice(),
            ),
            (
                "POST",
                "/stream/test-token",
                "",
                host.as_str(),
                "405 Method Not Allowed",
                b"".as_slice(),
            ),
            (
                "GET",
                "/stream/wrong",
                "",
                host.as_str(),
                "404 Not Found",
                b"".as_slice(),
            ),
        ] {
            let response = request(
                &host,
                format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\n{extra}\r\n"),
            )
            .await?;
            let split = response
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .context("headers")?;
            let headers = std::str::from_utf8(&response[..split])?;
            assert!(
                headers.starts_with(&format!("HTTP/1.1 {status}")),
                "{headers}"
            );
            assert_eq!(&response[split + 4..], body);
            if status.starts_with("206") {
                assert!(headers.contains("Content-Range: bytes"));
            }
        }
        cancel.cancel();
        server.await?;
        assert!(TcpStream::connect(&host).await.is_err());
        session.stop().await;
        Ok(())
    }
    #[tokio::test]
    async fn stop_during_replacement_shutdown_cancels_all_queued_prepares() -> Result<()> {
        let (temp, session, torrent) = offline_fixture().await?;
        let engine = TorrentEngine::new(temp.path().into());
        let old_token = engine.cancellation.lock().unwrap().token.child_token();
        let lease = create_lease(temp.path(), old_token.clone(), engine.runtime_handle())?;
        let (release, wait) = tokio::sync::oneshot::channel();
        // Hold old shutdown at its server join, without a swarm or wall-clock sleeps.
        *engine.state.lock().await = Some(Active {
            lease,
            torrent,
            server: tokio::spawn(async {
                let _ = wait.await;
            }),
            response: json!({}),
        });
        let input = json!({"infoHash":"0123456789abcdef0123456789abcdef01234567"});
        let mut replacement = Box::pin(engine.prepare(input.clone()));
        tokio::select! {
            biased;
            result = &mut replacement => panic!("shutdown did not wait: {result:?}"),
            _ = std::future::ready(()) => {},
        }
        assert!(old_token.is_cancelled());
        let mut queued = Box::pin(engine.prepare(input.clone()));
        tokio::select! {
            biased;
            result = &mut queued => panic!("queued prepare did not wait: {result:?}"),
            _ = std::future::ready(()) => {},
        }
        let mut first_stop = Box::pin(engine.stop());
        let mut second_stop = Box::pin(engine.stop());
        for stop in [&mut first_stop, &mut second_stop] {
            tokio::select! {
                biased;
                result = stop => panic!("stop did not wait: {result:?}"),
                _ = std::future::ready(()) => {},
            }
        }
        let mut during_stop = Box::pin(engine.prepare(input));
        tokio::select! {
            biased;
            result = &mut during_stop => panic!("prepare did not queue: {result:?}"),
            _ = std::future::ready(()) => {},
        }
        assert_eq!(engine.cancellation.lock().unwrap().stops, 2);
        // Aborting one overlapping stop must NOT reopen the epoch prematurely.
        drop(first_stop);
        assert_eq!(engine.cancellation.lock().unwrap().stops, 1);
        assert!(engine.cancellation.lock().unwrap().token.is_cancelled());
        release.send(()).unwrap();
        let results = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(replacement, queued, second_stop, during_stop)
        })
        .await?;
        for result in [results.0, results.1, results.3] {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("preparation cancelled"));
        }
        results.2?;
        assert!(!engine.cancellation.lock().unwrap().token.is_cancelled());
        assert_eq!(engine.cancellation.lock().unwrap().stops, 0);
        assert!(engine.state.lock().await.is_none());
        // No replacement filesystem lease or metadata resolver was started.
        assert!(std::fs::read_dir(temp.path().join("torrent-v1"))?
            .all(|e| e.unwrap().file_name() == ".manager.lock"));
        session.stop().await;
        Ok(())
    }

    struct StalledReader {
        started: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    }
    impl AsyncRead for StalledReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            std::task::Poll::Pending
        }
    }
    impl AsyncSeek for StalledReader {
        fn start_seek(self: std::pin::Pin<&mut Self>, _: SeekFrom) -> std::io::Result<()> {
            Ok(())
        }
        fn poll_complete(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<u64>> {
            std::task::Poll::Ready(Ok(0))
        }
    }
    /// An HTTP server whose every body read stalls, reporting each handler's
    /// start and its error (if any) on completion.
    struct StalledServer {
        host: String,
        cancel: CancellationToken,
        server: JoinHandle<()>,
        started_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
        done_rx: tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
    }
    async fn stalled_server() -> Result<StalledServer> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let host = listener.local_addr()?.to_string();
        let server_host = host.clone();
        let cancel = CancellationToken::new();
        let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(serve_connections(listener, cancel.clone(), move |socket| {
            let host = server_host.clone();
            let started = started_tx.clone();
            let done = done_tx.clone();
            async move {
                let result = handle_http_with_reader(
                    socket,
                    &host,
                    "/test",
                    "video/mp4",
                    16,
                    || async {
                        Ok(StalledReader {
                            started: Some(started),
                        })
                    },
                    BODY_IDLE_TIMEOUT,
                )
                .await;
                let _ = done.send(result.err().map(|e| e.to_string()));
            }
        }));
        Ok(StalledServer {
            host,
            cancel,
            server,
            started_rx,
            done_rx,
        })
    }
    /// Occupies every HTTP slot with a GET whose body is pending.
    async fn fill_slots(
        host: &str,
        started: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    ) -> Result<Vec<TcpStream>> {
        let mut clients = Vec::new();
        for _ in 0..MAX_HTTP_CLIENTS {
            let mut client = TcpStream::connect(host).await?;
            client
                .write_all(format!("GET /test HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
                .await?;
            tokio::time::timeout(Duration::from_secs(5), started.recv())
                .await?
                .context("mock reader not polled")?;
            clients.push(client);
        }
        Ok(clients)
    }
    #[tokio::test]
    async fn closed_clients_with_stalled_pieces_release_all_http_slots() -> Result<()> {
        let StalledServer {
            host,
            cancel,
            server,
            mut started_rx,
            mut done_rx,
        } = stalled_server().await?;
        let clients = fill_slots(&host, &mut started_rx).await?;
        // Every source is now deterministically pending, with all four slots held.
        // Advance only after socket setup: paused-time auto-advance and real IO
        // must not race each other during setup or subsequent HTTP assertions.
        tokio::time::pause();
        drop(clients);
        tokio::time::advance(BODY_IDLE_TIMEOUT).await;
        tokio::time::resume();
        for _ in 0..MAX_HTTP_CLIENTS {
            let error = tokio::time::timeout(Duration::from_secs(5), done_rx.recv())
                .await?
                .context("handler did not release slot")?
                .context("stalled body succeeded")?;
            assert!(error.contains("body read idle timeout"), "{error}");
        }
        // The same accept loop/cap now admits another request. HEAD + Range must
        // finish even though opening/reading the mocked payload would stall.
        let mut client = TcpStream::connect(&host).await?;
        client
            .write_all(
                format!("HEAD /test HTTP/1.1\r\nHost: {host}\r\nRange: bytes=2-5\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut response))
            .await??;
        assert!(response.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(response.contains("Content-Length: 4\r\n"));
        assert!(response.ends_with("\r\n\r\n"));
        assert!(tokio::time::timeout(Duration::from_secs(5), done_rx.recv())
            .await?
            .context("HEAD handler completion")?
            .is_none());
        assert!(started_rx.try_recv().is_err(), "HEAD opened the source");
        cancel.cancel();
        server.await?;
        Ok(())
    }

    #[tokio::test]
    async fn newest_request_replaces_oldest_stalled_handler() -> Result<()> {
        let StalledServer {
            host,
            cancel,
            server,
            mut started_rx,
            ..
        } = stalled_server().await?;
        // Abandoned seeks: every slot waits on a missing piece, clients still open.
        let mut clients = fill_slots(&host, &mut started_rx).await?;
        // Without eviction this request would be dropped until a 60 s idle timeout.
        let mut client = TcpStream::connect(&host).await?;
        client
            .write_all(
                format!("HEAD /test HTTP/1.1\r\nHost: {host}\r\nRange: bytes=0-3\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut response))
            .await??;
        assert!(
            response.starts_with("HTTP/1.1 206 Partial Content"),
            "{response}"
        );
        // The oldest stalled request was the one closed.
        let mut oldest = String::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            clients[0].read_to_string(&mut oldest),
        )
        .await??;
        assert!(oldest.starts_with("HTTP/1.1 200 OK"));
        cancel.cancel();
        server.await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn body_idle_timeout_bounds_both_stalled_reads_and_writes() {
        let mut reader = StalledReader { started: None };
        let mut writer = tokio::io::sink();
        let error = copy_body(&mut reader, &mut writer, BODY_IDLE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("body read idle timeout"));
        let (mut writer, _unread_client) = tokio::io::duplex(1);
        let mut reader = b"ab".as_slice();
        let error = copy_body(&mut reader, &mut writer, BODY_IDLE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("body write idle timeout"));
    }
    #[tokio::test(start_paused = true)]
    async fn body_idle_timeout_does_not_limit_total_stream_duration() -> Result<()> {
        let (mut source, mut reader) = tokio::io::duplex(1);
        let (mut writer, mut client) = tokio::io::duplex(1);
        let transfer =
            tokio::spawn(
                async move { copy_body(&mut reader, &mut writer, BODY_IDLE_TIMEOUT).await },
            );
        let start = tokio::time::Instant::now();
        for byte in b"0123456789" {
            source.write_all(&[*byte]).await?;
            let mut received = [0];
            client.read_exact(&mut received).await?;
            assert_eq!(received, [*byte]);
            tokio::time::advance(BODY_IDLE_TIMEOUT / 2).await;
        }
        assert!(start.elapsed() > BODY_IDLE_TIMEOUT);
        drop(source);
        transfer.await??;
        Ok(())
    }

    #[tokio::test]
    async fn unpolled_preparation_is_registered_before_stop() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let blocked = temp.path().join("blocked-cache");
        // Even a regression cannot create a network session: this is not a directory.
        std::fs::write(&blocked, b"no network")?;
        let engine = TorrentEngine::new(blocked);
        let future = engine.prepare(json!({"infoHash":"0123456789012345678901234567890123456789"}));
        engine.stop().await?;
        let error = tokio::time::timeout(Duration::from_secs(1), future)
            .await?
            .unwrap_err();
        assert!(error.to_string().contains("preparation cancelled"));
        Ok(())
    }
    #[tokio::test]
    async fn scoped_stop_does_not_cancel_newer_preparation_epoch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let blocked = temp.path().join("blocked-cache");
        std::fs::write(&blocked, b"no network")?;
        let engine = TorrentEngine::new(blocked);
        let next_ep = engine.prepare(json!({"infoHash":"0123456789012345678901234567890123456789"}));
        engine.stop_lease("retired-lease").await?;
        let error = tokio::time::timeout(Duration::from_secs(1), next_ep)
            .await?
            .unwrap_err();
        assert!(
            !format!("{error:#}").contains("preparation cancelled"),
            "scoped lease stop must never cancel the next episode's prepare: {error:#}"
        );
        let token = engine.cancellation.lock().unwrap().token.child_token();
        engine.stop().await?;
        assert!(token.is_cancelled());
        Ok(())
    }

    #[tokio::test]
    async fn idle_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let engine = TorrentEngine::new(temp.path().into());
        fn future_is_send<T: Send>(_: T) {}
        future_is_send(engine.prepare(json!({"infoHash":"bad"})));
        future_is_send(engine.stop());
        future_is_send(engine.stats());
        assert_eq!(engine.stats().await.unwrap()["active"], false);
        engine.stop().await.unwrap();
        assert!(engine.prepare(json!({"infoHash":"bad"})).await.is_err());
    }

    type UdpAnnounceLog = Arc<StdMutex<Vec<([u8; 20], u32, u16, u64)>>>;

    async fn spawn_mock_bep15_udp_tracker(
        peers_to_return: Vec<std::net::SocketAddrV4>,
        observed_announces: UdpAnnounceLog,
        cancel: CancellationToken,
    ) -> Result<(std::net::SocketAddr, JoinHandle<()>)> {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let local_addr = sock.local_addr()?;
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let conn_id: u64 = 0x1122_3344_5566_7788;
            loop {
                let recv = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    r = sock.recv_from(&mut buf) => r,
                };
                let Ok((len, peer)) = recv else { continue };
                if len < 16 {
                    continue;
                }
                let action = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let tid = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
                if action == 0 {
                    // Connect
                    let mut resp = [0u8; 16];
                    resp[0..4].copy_from_slice(&0u32.to_be_bytes());
                    resp[4..8].copy_from_slice(&tid.to_be_bytes());
                    resp[8..16].copy_from_slice(&conn_id.to_be_bytes());
                    let _ = sock.send_to(&resp, peer).await;
                } else if action == 1 && len >= 98 {
                    // Announce
                    let mut ih = [0u8; 20];
                    ih.copy_from_slice(&buf[16..36]);
                    let left = u64::from_be_bytes([
                        buf[64], buf[65], buf[66], buf[67], buf[68], buf[69], buf[70], buf[71],
                    ]);
                    let event = u32::from_be_bytes([buf[80], buf[81], buf[82], buf[83]]);
                    let port = u16::from_be_bytes([buf[96], buf[97]]);
                    observed_announces
                        .lock()
                        .unwrap()
                        .push((ih, event, port, left));

                    let mut resp = Vec::with_capacity(20 + peers_to_return.len() * 6);
                    resp.extend_from_slice(&1u32.to_be_bytes()); // ACTION_ANNOUNCE
                    resp.extend_from_slice(&tid.to_be_bytes());
                    resp.extend_from_slice(&60u32.to_be_bytes()); // interval
                    resp.extend_from_slice(&0u32.to_be_bytes()); // leechers
                    resp.extend_from_slice(&(peers_to_return.len() as u32).to_be_bytes()); // seeders
                    for p in &peers_to_return {
                        resp.extend_from_slice(&p.ip().octets());
                        resp.extend_from_slice(&p.port().to_be_bytes());
                    }
                    let _ = sock.send_to(&resp, peer).await;
                }
            }
        });
        Ok((local_addr, handle))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leak_probe_announces_http_and_udp_with_zero_disk_writes() -> Result<()> {
        let cache_dir = tempfile::tempdir()?;
        let engine = TorrentEngine::new(cache_dir.path().into());

        // Start mock HTTP tracker on 127.0.0.1
        let http_listener = TcpListener::bind("127.0.0.1:0").await?;
        let http_addr = http_listener.local_addr()?;
        let http_requests: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let http_requests_srv = http_requests.clone();
        let mock_cancel = CancellationToken::new();
        let mock_cancel_http = mock_cancel.clone();
        let http_srv = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    biased;
                    _ = mock_cancel_http.cancelled() => break,
                    a = http_listener.accept() => a,
                };
                let Ok((mut sock, _)) = accepted else { continue };
                let reqs = http_requests_srv.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    if let Ok(n) = sock.read(&mut buf).await {
                        let req_text = String::from_utf8_lossy(&buf[..n]);
                        if let Some(first_line) = req_text.lines().next() {
                            reqs.lock().unwrap().push(first_line.to_owned());
                        }
                        let body = b"d8:intervali60e5:peers0:ee";
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(hdr.as_bytes()).await;
                        let _ = sock.write_all(body).await;
                    }
                });
            }
        });

        // Start mock BEP-15 UDP tracker on 127.0.0.1
        let udp_announces = Arc::new(StdMutex::new(Vec::new()));
        let (udp_addr, udp_srv) =
            spawn_mock_bep15_udp_tracker(Vec::new(), udp_announces.clone(), mock_cancel.clone())
                .await?;

        let expected_hash_hex = "0123456789abcdef0123456789abcdef01234567";
        let mut expected_hash_bytes = [0u8; 20];
        hex::decode_to_slice(expected_hash_hex, &mut expected_hash_bytes)?;
        let magnet = format!(
            "magnet:?xt=urn:btih:{expected_hash_hex}&dn=ipleak.net&tr=http://{http_addr}/token123/announce&tr=udp://{udp_addr}/announce"
        );

        engine.start_leak_probe(&magnet)?;

        // Wait until both HTTP and UDP trackers have responded OK.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let st = engine.leak_probe_status();
                if st.trackers_ok == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("leak probe did not reach 2/2 trackers_ok")?;

        let st = engine.leak_probe_status();
        assert!(st.active);
        assert_eq!(st.trackers_total, 2);
        assert_eq!(st.trackers_contacted, 2);
        assert_eq!(st.trackers_ok, 2);
        assert!(st.remaining_secs > 0 && st.remaining_secs <= 60);
        assert!(st.error.is_none());

        // Verify HTTP announce query parameters and UDP BEP-15 announce fields.
        {
            let reqs = http_requests.lock().unwrap();
            assert!(!reqs.is_empty());
            assert!(reqs[0].starts_with("GET /token123/announce?info_hash="));
            assert!(reqs[0].contains("&port=6881&uploaded=0&downloaded=0&left=16384&compact=1&event=started"));
        }
        {
            let anns = udp_announces.lock().unwrap();
            assert!(!anns.is_empty());
            assert_eq!(anns[0], (expected_hash_bytes, 2, 6881, 16384));
        }

        // Strictly ZERO disk writes inside cache_dir!
        assert_eq!(std::fs::read_dir(cache_dir.path())?.count(), 0);

        engine.stop_leak_probe();
        let stopped = engine.leak_probe_status();
        assert!(!stopped.active);
        assert_eq!(stopped.remaining_secs, 0);
        assert_eq!(std::fs::read_dir(cache_dir.path())?.count(), 0);

        mock_cancel.cancel();
        let _ = http_srv.await;
        let _ = udp_srv.await;
        Ok(())
    }

    #[tokio::test]
    async fn leak_probe_enforces_vpn_killswitch_and_zero_disk_writes() -> Result<()> {
        let _lock = TEST_OVERRIDE_LOCK.lock().await;
        let _guard = InterfaceOverrideGuard::set(vec![NetworkInterface {
            name: "eth0".into(),
            addrs: vec!["192.168.1.20".into()],
            is_up: true,
            is_vpn_like: false,
        }]);
        let cache_dir = tempfile::tempdir()?;
        let engine = TorrentEngine::new(cache_dir.path().into());
        engine.set_vpn_policy(VpnPolicy {
            killswitch: true,
            bind_interface: None,
        });
        let err = engine
            .start_leak_probe(
                "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&tr=udp://127.0.0.1:9999/announce",
            )
            .unwrap_err();
        assert!(err.to_string().contains("VPN killswitch blocked torrent"));
        assert!(!engine.leak_probe_status().active);
        assert_eq!(std::fs::read_dir(cache_dir.path())?.count(), 0);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn loopback_p2p_swarm_prepare_range_and_resumable_pause_resume() -> Result<()> {
        let seed_temp = tempfile::tempdir()?;
        let pack_dir = seed_temp.path().join("SeasonPack");
        std::fs::create_dir_all(&pack_dir)?;

        let ep1_bytes = vec![0x11u8; 32 * 1024];
        let ep2_len = 512 * 1024usize; // 32 pieces of 16 KiB
        let ep2_bytes: Vec<u8> = (0..ep2_len)
            .map(|i| ((i.wrapping_mul(37).wrapping_add(13)) & 0xff) as u8)
            .collect();
        std::fs::write(pack_dir.join("Show.S01E01.mkv"), &ep1_bytes)?;
        std::fs::write(pack_dir.join("Show.S01E02.mkv"), &ep2_bytes)?;
        std::fs::write(pack_dir.join("Readme.txt"), b"release notes")?;

        let meta = librqbit::create_torrent(
            &pack_dir,
            librqbit::CreateTorrentOptions {
                piece_length: Some(16384),
                ..Default::default()
            },
            &librqbit::spawn_utils::BlockingSpawner::new(1),
        )
        .await?;
        let info_hash_hex = meta.info_hash().as_string();
        let torrent_bytes = meta.as_bytes()?;

        let seed_session = Session::new_with_opts(
            seed_temp.path().into(),
            SessionOptions {
                dht: None,
                disable_trackers: true,
                disable_local_service_discovery: true,
                ipv4_only: true,
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpAndUtp,
                    listen_addr: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                    enable_upnp_port_forwarding: false,
                    ipv4_only: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await?;
        let seed_listen = seed_session
            .listen_addr()
            .context("seeder listen_addr missing")?;
        let seed_v4 = match seed_listen {
            std::net::SocketAddr::V4(v4) => v4,
            other => bail!("expected IPv4 seeder listen addr, got {other}"),
        };

        let seed_handle = seed_session
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes),
                Some(AddTorrentOptions {
                    overwrite: true,
                    disable_trackers: true,
                    output_folder: Some(pack_dir.to_string_lossy().into_owned()),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("seed torrent handle")?;
        seed_handle.wait_until_initialized().await?;
        assert!(
            seed_handle.stats().finished,
            "seeder must have 100% verified pieces: {:?}",
            seed_handle.stats()
        );

        let tracker_cancel = CancellationToken::new();
        let tracker_announces = Arc::new(StdMutex::new(Vec::new()));
        let (udp_tracker_addr, udp_tracker_task) = spawn_mock_bep15_udp_tracker(
            vec![seed_v4],
            tracker_announces,
            tracker_cancel.clone(),
        )
        .await?;

        let client_cache = tempfile::tempdir()?;
        let engine = TorrentEngine::with_config(
            client_cache.path().into(),
            TorrentConfig {
                metadata_timeout: Duration::from_secs(20),
                ..Default::default()
            },
        );

        // Include a WebTorrent wss:// source alongside our local UDP tracker to prove multi-tracker resilience,
        // and omit fileIdx while passing episodeHint = "S01E02" to prove season-pack episode selection!
        let stream = json!({
            "infoHash": info_hash_hex,
            "episodeHint": "S01E02",
            "sources": [
                "tracker:wss://tracker.openwebtorrent.com",
                format!("tracker:udp://{udp_tracker_addr}/announce")
            ]
        });

        // 1. Test `prepare` + HTTP 206 Partial Content range read over real loopback P2P!
        let prepared = engine.prepare(stream.clone()).await?;
        assert_eq!(prepared["file_length"].as_u64(), Some(ep2_len as u64));
        assert!(prepared["file_name"]
            .as_str()
            .unwrap_or("")
            .ends_with("Show.S01E02.mkv"));

        let stream_url = url::Url::parse(prepared["url"].as_str().context("stream url")?)?;
        let authority = format!(
            "{}:{}",
            stream_url.host_str().unwrap(),
            stream_url.port().unwrap()
        );
        let mut http_sock = TcpStream::connect(&authority).await?;
        http_sock
            .write_all(
                format!(
                    "GET {} HTTP/1.1\r\nHost: {authority}\r\nRange: bytes=20000-20255\r\n\r\n",
                    stream_url.path()
                )
                .as_bytes(),
            )
            .await?;
        http_sock.shutdown().await?;
        let mut raw_resp = Vec::new();
        tokio::time::timeout(Duration::from_secs(15), http_sock.read_to_end(&mut raw_resp))
            .await??;
        let hdr_end = raw_resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .context("HTTP header terminator")?;
        let hdr_str = std::str::from_utf8(&raw_resp[..hdr_end])?;
        assert!(
            hdr_str.starts_with("HTTP/1.1 206 Partial Content"),
            "{hdr_str}"
        );
        assert_eq!(&raw_resp[hdr_end + 4..], &ep2_bytes[20000..=20255]);
        engine.stop().await?;

        // 2. Test `download_resumable`: Pause mid-flight -> verify resumable-* preserved -> Resume from verified pieces to completion!
        seed_session
            .ratelimits
            .set_upload_bps(std::num::NonZeroU32::new(128 * 1024));

        let out_dir = tempfile::tempdir()?;
        let dest_path = out_dir.path().join("Show.S01E02.mkv");
        let job_id = "series|tt0903747|ep2";
        let expected_resumable_dir = client_cache
            .path()
            .join("torrent-v1")
            .join(resumable_dir_name(job_id));

        let pause_token = CancellationToken::new();
        let pause_trigger = pause_token.clone();
        let keep_partial = Arc::new(AtomicBool::new(true));
        let paused_progress = Arc::new( std::sync::atomic::AtomicU64::new(0) );
        let paused_progress_cb = paused_progress.clone();

        let first_run = engine
            .download_resumable(
                job_id,
                stream.clone(),
                &dest_path,
                Some("S01E02"),
                Arc::new(move |done, _total| {
                    if done >= 64 * 1024 && done < ep2_len as u64 {
                        paused_progress_cb.store(done, Ordering::Relaxed);
                        pause_trigger.cancel();
                    }
                }),
                pause_token,
                keep_partial.clone(),
            )
            .await;
        let pause_err = first_run.unwrap_err();
        assert!(pause_err.to_string().contains("paused"), "{pause_err}");
        let bytes_before_pause = paused_progress.load(Ordering::Relaxed);
        assert!(
            bytes_before_pause >= 64 * 1024,
            "expected >= 64 KiB before pause, got {bytes_before_pause}"
        );
        assert!(!dest_path.exists());
        assert!(
            expected_resumable_dir.exists(),
            "paused download must preserve resumable-* dir"
        );

        // Verify `create_lease` sweep does NOT touch `resumable-*` directories!
        {
            let unrelated_lease =
                create_lease(client_cache.path(), CancellationToken::new(), None)?;
            drop(unrelated_lease);
            assert!(
                expected_resumable_dir.exists(),
                "create_lease sweep must never delete resumable-* dir"
            );
        }

        // Unthrottle seeder and resume with the same job_id!
        seed_session.ratelimits.set_upload_bps(None);
        let first_resumed_report = Arc::new(StdMutex::new(None::<u64>));
        let first_resumed_cb = first_resumed_report.clone();
        let resume_token = CancellationToken::new();
        let total_written = engine
            .download_resumable(
                job_id,
                stream.clone(),
                &dest_path,
                Some("S01E02"),
                Arc::new(move |done, _total| {
                    let mut guard = first_resumed_cb.lock().unwrap();
                    if guard.is_none() {
                        *guard = Some(done);
                    }
                }),
                resume_token,
                keep_partial,
            )
            .await?;

        assert_eq!(total_written, ep2_len as u64);
        let resumed_start = first_resumed_report
            .lock()
            .unwrap()
            .expect("resume progress called");
        assert!(
            resumed_start >= bytes_before_pause,
            "resume should start from verified pieces ({resumed_start} < {bytes_before_pause})"
        );
        assert_eq!(std::fs::read(&dest_path)?, ep2_bytes);
        assert!(
            !expected_resumable_dir.exists(),
            "completed download must clean up resumable-* dir"
        );

        // 3. Verify `cleanup_resumable(job_id)` removes an inactive resumable-* directory cleanly.
        {
            let dummy_lease = open_resumable_lease(
                client_cache.path(),
                "manual-cleanup-job",
                CancellationToken::new(),
                CancellationToken::new(),
                Arc::new(AtomicBool::new(true)),
                None,
            )?;
            let manual_dir = dummy_lease.dir.clone();
            drop(dummy_lease);
            assert!(manual_dir.exists());
            engine.cleanup_resumable("manual-cleanup-job")?;
            assert!(!manual_dir.exists());
        }

        tracker_cancel.cancel();
        let _ = udp_tracker_task.await;
        seed_session.stop().await;
        Ok(())
    }
}
