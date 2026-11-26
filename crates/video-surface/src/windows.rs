//! Windows: mpv gets the app window (`wid`) and opens its own child window filling it; we keep that
//! child at the bottom of the window's stack, under WebView2's, and the app makes the page transparent
//! while playing, so its controls float over the picture as elsewhere.
use crate::{Host, Path, Player};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

type Hwnd = isize;
#[link(name = "user32")]
extern "system" {
    fn EnumChildWindows(parent: Hwnd, f: unsafe extern "system" fn(Hwnd, isize) -> i32, data: isize) -> i32;
    fn GetClassNameW(hwnd: Hwnd, name: *mut u16, len: i32) -> i32;
    fn GetParent(hwnd: Hwnd) -> Hwnd;
    fn GetWindow(hwnd: Hwnd, cmd: u32) -> Hwnd;
    fn IsWindow(hwnd: Hwnd) -> i32;
    fn SetWindowPos(hwnd: Hwnd, after: Hwnd, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
}
const HWND_BOTTOM: Hwnd = 1;
const GW_HWNDLAST: u32 = 1;
const SWP_NOSIZE: u32 = 0x1;
const SWP_NOMOVE: u32 = 0x2;
const SWP_NOACTIVATE: u32 = 0x10;
const SWP_NOOWNERZORDER: u32 = 0x200;
const SWP_ASYNCWINDOWPOS: u32 = 0x4000;

/// Bumped on every open and close: the keeper thread of an old session stops.
static SESSION: AtomicU64 = AtomicU64::new(0);

pub fn candidates() -> &'static [Path] { crate::fallback::order("windows", false) }

/// mpv's own child window ("mpv" class) directly under the app window, if it exists yet.
fn mpv_window(top: Hwnd) -> Option<Hwnd> {
    unsafe extern "system" fn visit(hwnd: Hwnd, data: isize) -> i32 {
        const MPV: [u16; 3] = [b'm' as u16, b'p' as u16, b'v' as u16];
        let found = &mut *(data as *mut (Hwnd, Hwnd));
        let mut name = [0u16; 16];
        let n = GetClassNameW(hwnd, name.as_mut_ptr(), name.len() as i32).max(0) as usize;
        if GetParent(hwnd) == found.0 && name[..n] == MPV { found.1 = hwnd; return 0; }
        1
    }
    let mut found: (Hwnd, Hwnd) = (top, 0);
    unsafe { EnumChildWindows(top, visit, &mut found as *mut _ as isize); }
    (found.1 != 0).then_some(found.1)
}

pub fn open(_path: Path, host: &Host, player: &Arc<dyn Player>) -> Result<(), String> {
    let top = host.hwnd;
    if top == 0 { return Err("no window".into()); }
    player.mpv().set_property("wid", top as i64).map_err(|e| format!("wid: {e}"))?;
    let session = SESSION.fetch_add(1, Ordering::SeqCst) + 1;
    // mpv creates its window on its own thread once the file opens, on top of the stack: keep
    // sending it to the bottom (the WebView may also be re-stacked while it lives).
    std::thread::Builder::new().name("veylo-video".into()).spawn(move || {
        let mut cached: Option<Hwnd> = None;
        while SESSION.load(Ordering::SeqCst) == session {
            let video = cached.filter(|&w| unsafe { IsWindow(w) != 0 }).or_else(|| {
                let found = mpv_window(top);
                cached = found;
                found
            });
            let mut interval_ms = 50u64;
            if let Some(video) = video {
                if unsafe { GetWindow(video, GW_HWNDLAST) } != video {
                    unsafe { SetWindowPos(video, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS); }
                } else {
                    // Already behind WebView2: back off polling to save wakeups during long playback.
                    interval_ms = 500;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
    }).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn close(_path: Path) { SESSION.fetch_add(1, Ordering::SeqCst); }

pub fn layout(_player: &dyn Player, _hdr_output: bool) {}
