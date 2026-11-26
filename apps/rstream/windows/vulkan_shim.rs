//! Fallback `vulkan-1.dll` shim for Windows systems without a Vulkan GPU driver installed.
//! `libmpv-2.dll` hard-imports 10 Vulkan symbols even when RStream uses `gpu-api=d3d11`.
//! When `C:\Windows\System32\vulkan-1.dll` is present, all calls forward to the OS/driver loader;
//! otherwise they fail cleanly (`VK_ERROR_INCOMPATIBLE_DRIVER` / null) so D3D11 starts normally.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
const VK_ERROR_INCOMPATIBLE_DRIVER: i32 = -9;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, proc_name: *const u8) -> *const c_void;
}

fn sys_vulkan() -> usize {
    static MODULE: OnceLock<usize> = OnceLock::new();
    *MODULE.get_or_init(|| {
        const NAME: &[u16] = &[
            b'v' as u16,
            b'u' as u16,
            b'l' as u16,
            b'k' as u16,
            b'a' as u16,
            b'n' as u16,
            b'-' as u16,
            b'1' as u16,
            b'.' as u16,
            b'd' as u16,
            b'l' as u16,
            b'l' as u16,
            0,
        ];
        unsafe {
            LoadLibraryExW(
                NAME.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            ) as usize
        }
    })
}

unsafe fn resolve(sym: &[u8]) -> Option<*const c_void> {
    let m = sys_vulkan();
    if m == 0 {
        return None;
    }
    let p = GetProcAddress(m as *mut c_void, sym.as_ptr());
    (!p.is_null()).then_some(p)
}

#[no_mangle]
pub unsafe extern "system" fn vkGetInstanceProcAddr(
    instance: *mut c_void,
    name: *const c_char,
) -> *mut c_void {
    type FnT = unsafe extern "system" fn(*mut c_void, *const c_char) -> *mut c_void;
    match resolve(b"vkGetInstanceProcAddr\0") {
        Some(f) => std::mem::transmute::<*const c_void, FnT>(f)(instance, name),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "system" fn vkEnumeratePhysicalDevices(
    instance: *mut c_void,
    count: *mut u32,
    devs: *mut c_void,
) -> i32 {
    type FnT = unsafe extern "system" fn(*mut c_void, *mut u32, *mut c_void) -> i32;
    if let Some(f) = resolve(b"vkEnumeratePhysicalDevices\0") {
        return std::mem::transmute::<*const c_void, FnT>(f)(instance, count, devs);
    }
    if !count.is_null() {
        *count = 0;
    }
    VK_ERROR_INCOMPATIBLE_DRIVER
}

#[no_mangle]
pub unsafe extern "system" fn vkCreateDisplayPlaneSurfaceKHR(
    instance: *mut c_void,
    info: *const c_void,
    alloc: *const c_void,
    surface: *mut c_void,
) -> i32 {
    type FnT =
        unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut c_void) -> i32;
    match resolve(b"vkCreateDisplayPlaneSurfaceKHR\0") {
        Some(f) => std::mem::transmute::<*const c_void, FnT>(f)(instance, info, alloc, surface),
        None => VK_ERROR_INCOMPATIBLE_DRIVER,
    }
}

#[no_mangle]
pub unsafe extern "system" fn vkGetDisplayModePropertiesKHR(
    phys: *mut c_void,
    disp: u64,
    count: *mut u32,
    props: *mut c_void,
) -> i32 {
    type FnT = unsafe extern "system" fn(*mut c_void, u64, *mut u32, *mut c_void) -> i32;
    match resolve(b"vkGetDisplayModePropertiesKHR\0") {
        Some(f) => std::mem::transmute::<*const c_void, FnT>(f)(phys, disp, count, props),
        None => VK_ERROR_INCOMPATIBLE_DRIVER,
    }
}

#[no_mangle]
pub unsafe extern "system" fn vkGetDisplayPlaneSupportedDisplaysKHR(
    phys: *mut c_void,
    idx: u32,
    count: *mut u32,
    disps: *mut c_void,
) -> i32 {
    type FnT = unsafe extern "system" fn(*mut c_void, u32, *mut u32, *mut c_void) -> i32;
    match resolve(b"vkGetDisplayPlaneSupportedDisplaysKHR\0") {
        Some(f) => std::mem::transmute::<*const c_void, FnT>(f)(phys, idx, count, disps),
        None => VK_ERROR_INCOMPATIBLE_DRIVER,
    }
}

#[no_mangle]
pub unsafe extern "system" fn vkGetPhysicalDeviceDisplayPlanePropertiesKHR(
    phys: *mut c_void,
    count: *mut u32,
    props: *mut c_void,
) -> i32 {
    type FnT = unsafe extern "system" fn(*mut c_void, *mut u32, *mut c_void) -> i32;
    match resolve(b"vkGetPhysicalDeviceDisplayPlanePropertiesKHR\0") {
        Some(f) => std::mem::transmute::<*const c_void, FnT>(f)(phys, count, props),
        None => VK_ERROR_INCOMPATIBLE_DRIVER,
    }
}

#[no_mangle]
pub unsafe extern "system" fn vkGetPhysicalDeviceDisplayPropertiesKHR(
    phys: *mut c_void,
    count: *mut u32,
    props: *mut c_void,
) -> i32 {
    type FnT = unsafe extern "system" fn(*mut c_void, *mut u32, *mut c_void) -> i32;
    match resolve(b"vkGetPhysicalDeviceDisplayPropertiesKHR\0") {
        Some(f) => std::mem::transmute::<*const c_void, FnT>(f)(phys, count, props),
        None => VK_ERROR_INCOMPATIBLE_DRIVER,
    }
}

#[no_mangle]
pub unsafe extern "system" fn vkGetPhysicalDeviceProperties(phys: *mut c_void, props: *mut c_void) {
    type FnT = unsafe extern "system" fn(*mut c_void, *mut c_void);
    if let Some(f) = resolve(b"vkGetPhysicalDeviceProperties\0") {
        std::mem::transmute::<*const c_void, FnT>(f)(phys, props);
    }
}

#[no_mangle]
pub unsafe extern "system" fn vkGetPhysicalDeviceProperties2(
    phys: *mut c_void,
    props: *mut c_void,
) {
    type FnT = unsafe extern "system" fn(*mut c_void, *mut c_void);
    if let Some(f) = resolve(b"vkGetPhysicalDeviceProperties2\0") {
        std::mem::transmute::<*const c_void, FnT>(f)(phys, props);
    }
}

#[no_mangle]
pub unsafe extern "system" fn vkGetPhysicalDeviceQueueFamilyProperties2(
    phys: *mut c_void,
    count: *mut u32,
    props: *mut c_void,
) {
    type FnT = unsafe extern "system" fn(*mut c_void, *mut u32, *mut c_void);
    if let Some(f) = resolve(b"vkGetPhysicalDeviceQueueFamilyProperties2\0") {
        std::mem::transmute::<*const c_void, FnT>(f)(phys, count, props);
        return;
    }
    if !count.is_null() {
        *count = 0;
    }
}
