//! What the screen Veylo is on can show, read from the system each time it is asked (a window
//! can move to another monitor, and Windows HDR can be switched on or off at any time):
//! Android's display HDR types, Windows' DXGI output for the window's monitor, the EDR headroom of the
//! Mac's screen. Linux draws
//! through GTK, which can't hand HDR to the compositor, so its picture is always SDR.
use crate::engine::Screen;

/// The screen's capabilities, or None where they can't be read.
pub fn screen() -> Option<Screen> { imp::screen() }

/// One line for Settings and the player's details: what this screen shows, and what to do
/// about it when HDR could reach it but doesn't.
/// `hdr_output`: the Windows setting (Always SDR tone-maps even on an HDR screen).
pub fn summary(screen: Option<Screen>, hdr_output: bool) -> (String, String) {
    let peak = |s: &Screen| if s.peak_nits > 0 { format!(", up to {} nits", s.peak_nits) } else { String::new() };
    match screen {
        Some(s) if s.hdr10 || s.hlg || s.hdr10_plus || s.dolby_vision => {
            let mut kinds: Vec<&str> = Vec::new();
            if s.dolby_vision { kinds.push("Dolby Vision"); }
            if s.hdr10_plus { kinds.push("HDR10+"); }
            if s.hdr10 { kinds.push("HDR10"); }
            if s.hlg { kinds.push("HLG"); }
            let about = if cfg!(target_os = "windows") && !hdr_output { "HDR output is set to Always SDR: HDR videos are tone-mapped." }
                else { "HDR videos play in HDR." };
            (format!("HDR: {}{}", kinds.join(", "), peak(&s)), about.into())
        }
        Some(_) if cfg!(target_os = "windows") => ("SDR".into(),
            "HDR videos are tone-mapped to SDR. If this screen supports HDR, turn on \u{201c}Use HDR\u{201d} in Windows Settings \u{203a} Display, then check again.".into()),
        Some(_) => ("SDR".into(), "This screen doesn\u{2019}t declare HDR: HDR videos are tone-mapped to SDR.".into()),
        None if cfg!(target_os = "linux") => ("SDR".into(), "On Linux the picture is drawn in SDR: HDR videos are tone-mapped.".into()),
        None => ("Unknown".into(), "The screen couldn\u{2019}t be read: HDR videos are tone-mapped to SDR.".into()),
    }
}

#[cfg(target_os = "android")]
mod imp {
    pub fn screen() -> Option<super::Screen> { crate::android::screen().ok() }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::Screen;
    use windows::core::Interface;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};

    /// The DXGI output showing Veylo's window: Windows HDR is on for it when its colour space is
    /// PQ/BT.2020 (what "Use HDR" switches); its peak luminance comes with it.
    pub fn screen() -> Option<Screen> {
        // The shell's window, from its picture bridge (each shell has its own video.rs).
        let hwnd = HWND(crate::video::hwnd()? as *mut core::ffi::c_void);
        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
        for a in 0.. {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else { break };
            for o in 0.. {
                let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else { break };
                let Ok(output) = output.cast::<IDXGIOutput6>() else { continue };
                let Ok(desc) = (unsafe { output.GetDesc1() }) else { continue };
                if desc.Monitor != monitor { continue; }
                let hdr = desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                // HLG videos reach it as HDR10 too (mpv converts them): the screen itself takes HDR10.
                return Some(Screen { hdr10: hdr, peak_nits: if hdr { desc.MaxLuminance.round() as u32 } else { 0 }, ..Default::default() });
            }
        }
        None
    }
}

/// macOS: EDR. A screen that can show values above SDR white (headroom over 1.0, an XDR or HDR
/// display) gets HDR as PQ through the video layer (video.rs); its nits aren't public, so none are named.
#[cfg(target_os = "macos")]
mod imp {
    pub fn screen() -> Option<super::Screen> {
        let headroom = crate::video::potential_headroom();
        (headroom > 0.0).then(|| super::Screen { hdr10: headroom > 1.0, hlg: headroom > 1.0, ..Default::default() })
    }
}

#[cfg(not(any(target_os = "android", target_os = "windows", target_os = "macos")))]
mod imp {
    pub fn screen() -> Option<super::Screen> { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_names_what_reaches_the_screen() {
        let hdr = Screen { hdr10: true, peak_nits: 1000, ..Default::default() };
        assert_eq!(summary(Some(hdr), true).0, "HDR: HDR10, up to 1000 nits");
        let tv = Screen { hdr10: true, hdr10_plus: true, dolby_vision: true, ..Default::default() };
        assert_eq!(summary(Some(tv), true).0, "HDR: Dolby Vision, HDR10+, HDR10");
        assert_eq!(summary(Some(Screen::default()), true).0, "SDR");
    }
}
