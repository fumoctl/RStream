//! `rstream --self-test [--report <file>]`: checks that RStream's store, libmpv, and FFmpeg work
//! on this machine without a window.
use crate::backend;
use anyhow::{anyhow, ensure, Context, Result};
use libmpv2::{events::Event, mpv_end_file_reason, Mpv};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) fn resolve_launch_path(path: &str) -> PathBuf {
    resolve_launch_path_with(path, std::env::var_os("RSTREAM_LAUNCH_CWD").as_deref())
}

fn resolve_launch_path_with(path: &str, launch_cwd: Option<&std::ffi::OsStr>) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_relative() {
        if let Some(cwd) = launch_cwd.filter(|s| !s.is_empty()) {
            return Path::new(cwd).join(p);
        }
    }
    p
}

/// `rstream --check-picture <screenshot.png> [--report <file>]`: verifies that a screenshot taken
/// while the colour-bar clip plays (`RSTREAM_PLAY_DEMO=bars`) shows its six colour bars.
pub fn check_picture(path: &str, report: Option<&str>) -> i32 {
    let resolved = resolve_launch_path(path);
    let result = image::open(&resolved)
        .map_err(|e| format!("can't read {}: {e}", resolved.display()))
        .and_then(|image| {
            let image = image.to_rgb8();
            let (w, h) = image.dimensions();
            veylo_picture_check::find_bars(image.as_raw(), w as usize, h as usize, 3)
        });
    let (line, code) = match result {
        Ok(found) => (format!("PASS  picture    {}", found.summary()), 0),
        Err(e) => (format!("FAIL  picture    {e}"), 1),
    };
    println!("{line}");
    if let Some(report) = report {
        let _ = std::fs::write(resolve_launch_path(report), format!("{line}\n"));
    }
    code
}

pub fn run(args: &[String]) -> i32 {
    let report = args.iter().position(|a| a == "--report").and_then(|i| args.get(i + 1)).map(|p| resolve_launch_path(p));
    let data = std::env::temp_dir().join(format!("rstream-self-test-{}", std::process::id()));
    std::env::set_var("RSTREAM_DATA", &data);

    let mut lines = vec![format!("RStream {} self-test on {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH)];
    println!("{}", lines[0]);
    let mut failed = 0;
    let mut check = |name: &str, result: Result<String>| {
        let line = match result {
            Ok(detail) => format!("PASS  {name:<10} {detail}"),
            Err(e) => { failed += 1; format!("FAIL  {name:<10} {e:#}") }
        };
        println!("{line}");
        lines.push(line);
    };
    check("store", store());
    check("player", player(&data));
    check("hdr", hdr(&data));
    check("mpv-config", mpv_config(&data));
    check("rclone", rclone_crypt(&data));
    check("torrent", torrent_killswitch(&data));
    let _ = std::fs::remove_dir_all(&data);

    let summary = if failed == 0 { "All checks passed.".to_owned() } else { format!("{failed} check(s) failed.") };
    println!("{summary}");
    lines.push(summary);
    if let Some(path) = report { let _ = std::fs::write(path, lines.join("\n") + "\n"); }
    if failed == 0 { 0 } else { 1 }
}

/// The library store (SQLite) and settings open, write and read back.
fn store() -> Result<String> {
    backend::init()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let media = backend::Media::from_json(&serde_json::json!({
        "id": "tt0000001", "type": "movie", "name": "Self-test film", "addon_url": "https://self-test.invalid/manifest.json"
    }), "").context("test title")?;
    let saved = runtime.block_on(async {
        backend::set_saved(&media, true).await?;
        backend::library().await
    })?;
    ensure!(saved.iter().any(|m| m.id == media.id && m.saved), "a saved title didn't come back from the library");
    let before = backend::load_settings();
    let flipped = !before.better_posters;
    backend::update_settings(|s| s.better_posters = flipped);
    let read_back = backend::load_settings().better_posters;
    backend::save_settings(&before);
    ensure!(read_back == flipped, "settings didn't read back");
    Ok("library saved and read back; settings round-trip".into())
}

const CLIP: &[u8] = include_bytes!("../assets/selftest.mp4");

fn player(data: &std::path::Path) -> Result<String> {
    std::fs::create_dir_all(data)?;
    let clip = data.join("selftest.mp4");
    std::fs::write(&clip, CLIP)?;
    let shader = data.join("selftest-pass.glsl");
    std::fs::write(&shader, "//!HOOK LUMA\n//!BIND HOOKED\nvec4 hook() { return HOOKED_tex(HOOKED_pos); }\n")?;
    crate::engine::ensure_c_numeric_locale();
    let mpv = Mpv::with_initializer(|o| {
        o.set_property("config", false)?;
        o.set_property("terminal", false)?;
        o.set_property("vo", "null")?;
        o.set_property("ao", "null")?;
        o.set_property("idle", true)?;
        o.set_property("glsl-shaders", shader.to_string_lossy().as_ref())?;
        Ok(())
    }).map_err(|e| anyhow!("mpv couldn't start: {e}"))?;
    let loaded_shaders: String = mpv.get_property("glsl-shaders").unwrap_or_default();
    ensure!(loaded_shaders.contains("selftest-pass.glsl"), "mpv glsl-shaders did not retain test shader");
    let version: String = mpv.get_property("mpv-version").unwrap_or_default();
    let ffmpeg: String = mpv.get_property("ffmpeg-version").unwrap_or_default();
    mpv.command("loadfile", &[clip.to_string_lossy().as_ref(), "replace"]).map_err(|e| anyhow!("loadfile: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut size, mut codecs) = ((0, 0), (String::new(), String::new()));
    loop {
        ensure!(Instant::now() < deadline, "the test clip didn't finish in 30 s");
        match mpv.wait_event(0.5) {
            Some(Ok(Event::FileLoaded)) => {
                codecs = (mpv.get_property("current-tracks/video/codec").unwrap_or_default(), mpv.get_property("current-tracks/audio/codec").unwrap_or_default());
            }
            Some(Ok(Event::VideoReconfig)) => {
                let w = mpv.get_property::<i64>("video-params/w").unwrap_or(0);
                if w > 0 { size = (w, mpv.get_property::<i64>("video-params/h").unwrap_or(0)); }
            }
            Some(Ok(Event::EndFile(reason))) => {
                ensure!(reason == mpv_end_file_reason::Eof, "the test clip ended early (reason {reason})");
                break;
            }
            Some(Err(e)) => return Err(anyhow!("mpv: {e}")),
            _ => {}
        }
    }
    ensure!(codecs == ("h264".into(), "aac".into()), "tracks are {codecs:?}, expected h264 and aac");
    ensure!(size == (320, 240), "decoded picture is {}x{}, expected 320x240", size.0, size.1);
    #[cfg(target_os = "macos")]
    let picture = format!("; {}", crate::video::check(&mpv).map_err(|e| anyhow!("video layer: {e}"))?);
    #[cfg(not(target_os = "macos"))]
    let picture = String::new();
    Ok(format!("{version} (FFmpeg {ffmpeg}) played H.264 {}x{} and AAC to the end{picture}", size.0, size.1))
}

pub const HDR_CLIP: &[u8] = include_bytes!("../assets/selftest-hdr.mp4");

fn hdr(data: &std::path::Path) -> Result<String> {
    std::fs::create_dir_all(data)?;
    let clip = data.join("selftest-hdr.mp4");
    std::fs::write(&clip, HDR_CLIP)?;
    crate::engine::ensure_c_numeric_locale();
    let mpv = Mpv::with_initializer(|o| {
        for (k, v) in [("config", "no"), ("terminal", "no"), ("vo", "null"), ("ao", "null"), ("keep-open", "yes")] { o.set_property(k, v)?; }
        Ok(())
    }).map_err(|e| anyhow!("mpv couldn't start: {e}"))?;
    mpv.command("loadfile", &[clip.to_string_lossy().as_ref()]).map_err(|e| anyhow!("loadfile: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let params = loop {
        ensure!(Instant::now() < deadline, "the HDR clip didn't start in 20 s");
        if let Some(Ok(Event::VideoReconfig)) = mpv.wait_event(0.5) {
            let p = |n: &str| mpv.get_property::<String>(&format!("video-params/{n}")).unwrap_or_default();
            if !p("gamma").is_empty() { break (p("gamma"), p("primaries"), p("pixelformat")); }
        }
    };
    ensure!(params.0 == "pq" && params.1 == "bt.2020", "the HDR10 clip read as {}/{}", params.0, params.1);
    #[cfg(target_os = "macos")]
    return crate::video::hdr_check(&clip.to_string_lossy()).map_err(|e| anyhow!("{e}"));
    #[cfg(not(target_os = "macos"))]
    Ok(format!("HDR10 recognised: PQ, BT.2020, {}", params.2))
}

fn mpv_config(data: &std::path::Path) -> Result<String> {
    let dir = rstream_mpv_config::MpvConfigDir::open_or_seed(data.join("mpv-selftest"))?;
    let shader = dir.shaders_dir().join("test.glsl");
    std::fs::write(&shader, "//!HOOK LUMA\n//!DESC Self-test pass\nvec4 hook() { return vec4(0.0); }\n")?;
    dir.write_mpv_conf("profile=high-quality\nglsl-shader=~~/shaders/test.glsl\nvo=invalid-ignored\n")?;
    dir.write_input_conf("Ctrl+t show-text \"selftest\" # Self-test OSD\n")?;
    let parsed = dir.parse_mpv_conf()?;
    ensure!(parsed.has_shaders, "mpv.conf did not detect active glsl-shader");
    ensure!(parsed.options.iter().all(|(k, _)| k != "vo"), "unsafe vo option should be skipped");
    let binds = dir.parse_input_conf()?;
    ensure!(binds.iter().any(|b| b.bind.ctrl && b.bind.key == "t"), "input.conf did not parse Ctrl+t");
    Ok(format!("{} mpv option(s), {} shader(s), {} binding(s)", parsed.options.len(), dir.list_shaders().len(), binds.len()))
}

fn rclone_crypt(data: &std::path::Path) -> Result<String> {
    let backing = data.join("rclone-backing");
    std::fs::create_dir_all(&backing)?;
    rstream_rclone::init(&data.join("rclone.conf"))?;
    rstream_rclone::create_remote("st_local", "local", serde_json::json!({}))?;
    let target = format!("st_local:{}", backing.to_string_lossy());
    rstream_rclone::create_crypt_remote("st_crypt", &target, "selftest-passphrase", Some("selftest-salt"))?;
    let sample = data.join("rclone-sample.bin");
    let payload = b"RStream crypt roundtrip payload 0123456789abcdef";
    std::fs::write(&sample, payload)?;
    let jobid = rstream_rclone::copy_local_to_remote(&sample, "st_crypt:", "selftest.bin")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        ensure!(Instant::now() < deadline, "rclone crypt upload timed out");
        let prog = rstream_rclone::job_progress(jobid)?;
        if prog.finished {
            ensure!(prog.success, "rclone upload failed: {:?}", prog.error);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let url = rstream_rclone::file_stream_url("st_crypt:", "selftest.bin")?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let decrypted = rt.block_on(async { reqwest::get(&url).await?.error_for_status()?.bytes().await })?;
    ensure!(decrypted.as_ref() == payload, "rclone crypt decrypted bytes mismatch");
    Ok(format!("librclone crypt roundtrip verified ({} bytes)", decrypted.len()))
}

fn torrent_killswitch(data: &std::path::Path) -> Result<String> {
    let engine = stremio_torrent::TorrentEngine::new(data.join("torrent-selftest"));
    engine.set_vpn_policy(stremio_torrent::VpnPolicy {
        killswitch: true,
        bind_interface: Some("rstream_missing_vpn0".into()),
    });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let res = rt.block_on(engine.prepare(serde_json::json!({
        "infoHash": "0123456789abcdef0123456789abcdef01234567"
    })));
    let err = match res {
        Ok(_) => anyhow::bail!("VPN killswitch failed to block torrent on missing interface"),
        Err(e) => format!("{e:#}"),
    };
    ensure!(err.to_lowercase().contains("killswitch") || err.to_lowercase().contains("blocked"), "unexpected killswitch error: {err}");
    Ok(format!("blocked P2P when VPN missing ({err})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_paths_against_launch_cwd() {
        assert_eq!(
            resolve_launch_path_with("bars.png", Some(std::ffi::OsStr::new("/tmp/caller"))),
            PathBuf::from("/tmp/caller/bars.png")
        );
        assert_eq!(
            resolve_launch_path_with("/abs/bars.png", Some(std::ffi::OsStr::new("/tmp/caller"))),
            PathBuf::from("/abs/bars.png")
        );
        assert_eq!(resolve_launch_path_with("bars.png", None), PathBuf::from("bars.png"));
    }
}
