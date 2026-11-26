//! Android: hand our Kotlin bridges (video surface, Back, VPN monitor) and native libs
//! (libmpv + FFmpeg + librclone) to the Gradle project dx generates.
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=android/kotlin");
    println!("cargo:rerun-if-env-changed=WRY_ANDROID_KOTLIN_FILES_OUT_DIR");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "windows" {
        windows();
        stage_desktop_librclone("windows");
        return;
    }
    if os == "macos" {
        macos();
        stage_desktop_librclone("macos");
        return;
    }
    if os != "android" {
        if os == "linux" {
            link_linux_appindicator_rpath();
        }
        stage_desktop_librclone(&os);
        return;
    }

    let abi = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x86_64",
        Ok("aarch64") => "arm64-v8a",
        Ok("arm") => "armeabi-v7a",
        _ => return,
    };

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_mpv = manifest_dir.join("../../vendor/mpv-android");
    let mpv_libs = vendor_mpv.join(abi);
    assert!(
        mpv_libs.join("libmpv.so").is_file(),
        "libmpv for Android ({abi}) is missing at {}; run tools/fetch-mpv-android.sh",
        mpv_libs.display()
    );
    println!("cargo:rustc-link-search=native={}", mpv_libs.display());

    let Ok(out) = std::env::var("WRY_ANDROID_KOTLIN_FILES_OUT_DIR") else { return };
    let out = Path::new(&out);
    std::fs::create_dir_all(out).expect("kotlin output dir");
    for entry in std::fs::read_dir("android/kotlin").expect("android/kotlin") {
        let path = entry.expect("kotlin entry").path();
        if path.extension().is_some_and(|e| e == "kt" || e == "pro") {
            let dest = out.join(path.file_name().unwrap());
            std::fs::copy(&path, &dest).expect("copy kotlin/proguard file");
            println!("cargo:rerun-if-changed={}", dest.display());
        }
    }

    // Launcher icon and label: overlay our res tree on the generated one.
    println!("cargo:rerun-if-changed=android/res");
    if let Some(main_dir) = out.ancestors().nth(4) {
        fn overlay(from: &Path, to: &Path) {
            std::fs::create_dir_all(to).expect("res dir");
            for entry in std::fs::read_dir(from).expect("android/res") {
                let path = entry.expect("res entry").path();
                let dest = to.join(path.file_name().unwrap());
                if path.is_dir() {
                    overlay(&path, &dest);
                    continue;
                }
                std::fs::copy(&path, &dest).expect("copy res file");
                println!("cargo:rerun-if-changed={}", dest.display());
            }
        }
        overlay(Path::new("android/res"), &main_dir.join("res"));
        tv_launcher(&main_dir.join("AndroidManifest.xml"));
        pin_installed_build_tools(main_dir);
    }

    // Package libmpv + FFmpeg + librclone into jniLibs/<abi>/ and cacert.pem into assets/.
    println!("cargo:rerun-if-changed=../../vendor/mpv-android");
    println!("cargo:rerun-if-changed=../../vendor/librclone");
    let main_dir = out.ancestors().nth(4).expect("android project layout");
    let libs = main_dir.join("jniLibs").join(abi);
    std::fs::create_dir_all(&libs).expect("jniLibs dir");
    for entry in std::fs::read_dir(&mpv_libs).expect("vendor/mpv-android libraries") {
        let path = entry.expect("mpv entry").path();
        if path.file_name().is_some_and(|n| n == "libplayer.so") {
            continue;
        }
        if path.extension().is_some_and(|e| e == "so") {
            let dest = libs.join(path.file_name().unwrap());
            std::fs::copy(&path, &dest).expect("copy mpv library");
            println!("cargo:rerun-if-changed={}", dest.display());
        }
    }

    let rclone_so = manifest_dir
        .join("../../vendor/librclone")
        .join(format!("android-{abi}"))
        .join("librclone.so");
    assert!(
        rclone_so.is_file(),
        "librclone for Android ({abi}) is missing at {}; run tools/build-librclone.sh android",
        rclone_so.display()
    );
    let dest = libs.join("librclone.so");
    std::fs::copy(&rclone_so, &dest).expect("copy librclone.so for Android");
    println!("cargo:rerun-if-changed={}", dest.display());

    let assets = main_dir.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");
    let cacert = vendor_mpv.join("cacert.pem");
    assert!(
        cacert.is_file(),
        "cacert.pem is missing at {}; run tools/fetch-mpv-android.sh",
        cacert.display()
    );
    let dest_ca = assets.join("cacert.pem");
    std::fs::copy(&cacert, &dest_ca).expect("copy cacert.pem");
    println!("cargo:rerun-if-changed={}", dest_ca.display());
}

fn pin_installed_build_tools(main_dir: &Path) {
    let Some(app_dir) = main_dir.parent().and_then(Path::parent) else { return };
    let gradle = app_dir.join("build.gradle.kts");
    let Ok(raw) = std::fs::read_to_string(&gradle) else { return };
    let text = raw.replace("targetSdk = 35", "targetSdk = 36");
    if text.contains("buildToolsVersion") {
        if text != raw {
            let _ = std::fs::write(&gradle, text);
        }
        return;
    }
    let sdk = std::env::var("ANDROID_HOME").or_else(|_| std::env::var("ANDROID_SDK_ROOT")).ok();
    let Some(sdk) = sdk else {
        if text != raw {
            let _ = std::fs::write(&gradle, text);
        }
        return;
    };
    let bt_dir = Path::new(&sdk).join("build-tools");
    let Ok(entries) = std::fs::read_dir(&bt_dir) else { return };
    let mut versions: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    versions.sort_by(|a, b| {
        let nums = |s: &str| -> Vec<u64> {
            s.split(|c: char| !c.is_ascii_digit())
                .filter(|p| !p.is_empty())
                .filter_map(|p| p.parse().ok())
                .collect()
        };
        nums(a).cmp(&nums(b)).then_with(|| a.cmp(b))
    });
    let Some(ver) = versions.last() else { return };
    let mut out = String::with_capacity(text.len() + 64);
    let mut patched = false;
    for line in text.lines() {
        out.push_str(line);
        out.push('\n');
        if !patched && line.trim_start().starts_with("compileSdk") {
            out.push_str(&format!("    buildToolsVersion = \"{ver}\"\n"));
            patched = true;
        }
    }
    if patched || text != raw {
        let _ = std::fs::write(&gradle, out);
    }
}

fn link_linux_appindicator_rpath() {
    let Ok(out) = std::process::Command::new("pkg-config")
        .args(["--libs-only-L", "ayatana-appindicator3-0.1"])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for tok in text.split_whitespace() {
        if let Some(dir) = tok.strip_prefix("-L") {
            if !dir.is_empty() && Path::new(dir).is_dir() {
                println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
            }
        }
    }
}

fn stage_desktop_librclone(os: &str) {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let lib_name = match os {
        "windows" => "librclone.dll",
        "macos" => "librclone.dylib",
        _ => "librclone.so",
    };
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src = manifest_dir
        .join("../../vendor/librclone")
        .join(format!("{os}-{arch}"))
        .join(lib_name);
    println!("cargo:rerun-if-changed={}", src.display());
    if !src.is_file() {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    if let Some(profile) = out
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "build"))
        .and_then(Path::parent)
    {
        for to in [profile.to_path_buf(), profile.join("deps")] {
            let dest = to.join(lib_name);
            let stale = std::fs::metadata(&dest).and_then(|m| m.modified()).ok()
                < std::fs::metadata(&src).and_then(|m| m.modified()).ok();
            if stale && std::fs::create_dir_all(&to).is_ok() {
                let _ = std::fs::copy(&src, &dest);
            }
        }
    }
}

/// Android TV: the TV home screen lists only apps whose activity is in LEANBACK_LAUNCHER.
fn tv_launcher(manifest: &Path) {
    let Ok(xml) = std::fs::read_to_string(manifest) else { return };
    let mut patched = xml.clone();
    if !patched.contains("android:banner=") {
        patched = patched.replacen("<application ", "<application android:banner=\"@drawable/tv_banner\" ", 1);
    }
    if !patched.contains("LEANBACK_LAUNCHER") {
        patched = patched.replacen(
            "<category android:name=\"android.intent.category.LAUNCHER\" />",
            "<category android:name=\"android.intent.category.LAUNCHER\" />\n                <category android:name=\"android.intent.category.LEANBACK_LAUNCHER\" />",
            1,
        );
    }
    if !patched.contains("supportsPictureInPicture") {
        patched = patched.replacen("<activity ", "<activity android:supportsPictureInPicture=\"true\" ", 1);
    }
    if !patched.contains("android:launchMode=") {
        patched = patched.replacen("<activity ", "<activity android:launchMode=\"singleTask\" ", 1);
    }
    for perm in [
        "android.permission.FOREGROUND_SERVICE",
        "android.permission.FOREGROUND_SERVICE_DATA_SYNC",
        "android.permission.POST_NOTIFICATIONS",
        "android.permission.ACCESS_WIFI_STATE",
        "android.permission.WAKE_LOCK",
    ] {
        if !patched.contains(perm) {
            patched = patched.replacen(
                "<application ",
                &format!("    <uses-permission android:name=\"{perm}\" />\n    <application "),
                1,
            );
        }
    }
    let inet = "<uses-permission android:name=\"android.permission.INTERNET\" />";
    if patched.matches(inet).count() > 1 {
        if let Some(second) = patched.rfind(inet) {
            let end = patched[second..]
                .find('\n')
                .map(|i| second + i + 1)
                .unwrap_or(second + inet.len());
            patched.replace_range(second..end, "");
        }
    }
    if !patched.contains("dev.rstream.app.RStreamOAuthService") {
        patched = patched.replacen(
            "</application>",
            "        <service android:name=\"dev.rstream.app.RStreamOAuthService\" android:exported=\"false\" android:foregroundServiceType=\"shortService\" />\n    </application>",
            1,
        );
    }
    if !patched.contains("dev.rstream.app.RStreamTransferService") {
        patched = patched.replacen(
            "</application>",
            "        <service android:name=\"dev.rstream.app.RStreamTransferService\" android:exported=\"false\" android:foregroundServiceType=\"dataSync\" />\n    </application>",
            1,
        );
    }
    if patched != xml {
        std::fs::write(manifest, patched).expect("patch AndroidManifest.xml");
    }
    println!("cargo:rerun-if-changed={}", manifest.display());
}

fn build_windows_vulkan_shim(out: &Path) -> PathBuf {
    println!("cargo:rerun-if-changed=windows/vulkan_shim.rs");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let shim_src = manifest_dir.join("windows/vulkan_shim.rs");
    let shim_dll = out.join("vulkan-1.dll");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let target = std::env::var("TARGET").expect("TARGET env var");
    let mut cmd = std::process::Command::new(rustc);
    cmd.arg("--crate-type=cdylib")
        .arg("--edition=2021")
        .arg("-Copt-level=s")
        .arg("-Cstrip=symbols")
        .arg("-Dwarnings")
        .arg("--target")
        .arg(&target);
    let mut has_linker = false;
    if let Ok(encoded) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        for flag in encoded.split('\x1f').filter(|s| !s.is_empty()) {
            if flag.contains("linker=") {
                has_linker = true;
            }
            cmd.arg(flag);
        }
    }
    if !has_linker {
        let linker_var = format!(
            "CARGO_TARGET_{}_LINKER",
            target.replace('-', "_").to_ascii_uppercase()
        );
        if let Ok(linker) = std::env::var(&linker_var) {
            if !linker.trim().is_empty() {
                cmd.arg(format!("-Clinker={linker}"));
            }
        }
    }
    cmd.arg(&shim_src).arg("-o").arg(&shim_dll);
    let status = cmd.status().expect("invoke rustc for vulkan-1.dll shim");
    assert!(status.success(), "failed to compile windows/vulkan_shim.rs");
    shim_dll
}

fn windows() {
    println!("cargo:rerun-if-changed=windows/rstream.rc");
    println!("cargo:rerun-if-changed=windows/rstream.ico");
    embed_resource::compile("windows/rstream.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("compile the Windows icon resource");
    println!("cargo:rerun-if-env-changed=VEYLO_MPV_DIR");
    println!("cargo:rerun-if-changed=windows/mpv");
    let dir = std::env::var_os("VEYLO_MPV_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("windows/mpv"));
    let dll = dir.join("libmpv-2.dll");
    assert!(dll.is_file(), "libmpv for Windows is missing: run windows/fetch-mpv.ps1 (or set VEYLO_MPV_DIR)");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let vk_shim = build_windows_vulkan_shim(&out);
    if !dir.join("mpv.lib").is_file() && dir.join("libmpv.dll.a").is_file() {
        std::fs::copy(dir.join("libmpv.dll.a"), out.join("mpv.lib")).expect("copy the mpv import library");
        println!("cargo:rustc-link-search=native={}", out.display());
    }
    println!("cargo:rustc-link-search=native={}", dir.display());
    if let Some(profile) = out
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "build"))
        .and_then(Path::parent)
    {
        for to in [profile.to_path_buf(), profile.join("deps")] {
            for (src, name) in [(&dll, "libmpv-2.dll"), (&vk_shim, "vulkan-1.dll")] {
                let dest = to.join(name);
                let stale = std::fs::metadata(&dest).and_then(|m| m.modified()).ok()
                    < std::fs::metadata(src).and_then(|m| m.modified()).ok();
                if stale && std::fs::create_dir_all(&to).is_ok() {
                    std::fs::copy(src, &dest)
                        .unwrap_or_else(|e| panic!("copy {name} beside the executable: {e}"));
                }
            }
        }
    }
}

fn macos() {
    println!("cargo:rerun-if-env-changed=VEYLO_MPV_DIR");
    println!("cargo:rerun-if-env-changed=HOMEBREW_PREFIX");
    let dirs: Vec<PathBuf> = match std::env::var_os("VEYLO_MPV_DIR") {
        Some(dir) => vec![dir.into()],
        None => std::env::var_os("HOMEBREW_PREFIX")
            .map(|p| PathBuf::from(p).join("lib"))
            .into_iter()
            .chain(["/opt/homebrew/lib", "/usr/local/lib"].map(PathBuf::from))
            .collect(),
    };
    let found = dirs.iter().find(|d| d.join("libmpv.dylib").exists() || d.join("libmpv.2.dylib").exists());
    let dir = found.unwrap_or_else(|| panic!("libmpv for macOS is missing: run `brew install mpv` (or set VEYLO_MPV_DIR)"));
    println!("cargo:rustc-link-search=native={}", dir.display());
}
