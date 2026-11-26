#![cfg_attr(windows, windows_subsystem = "windows")]
//! Self-extracting single-file portable launcher for Windows (`RStream-<ver>-windows-x86_64.exe`).
//! Reads an appended zlib-compressed bundle (`RSTRSFX1` trailer), extracts once into
//! `%LOCALAPPDATA%\rstream\runtime\<hash16>\`, prunes older cached runtimes, and spawns `rstream.exe`.

use flate2::read::ZlibDecoder;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

const MAGIC: &[u8; 8] = b"RSTRSFX1";
const TRAILER_LEN: u64 = 8 + 32 + 8;
const MAX_EXTRACTED_BYTES: u64 = 1024 * 1024 * 1024;

struct Trailer {
    payload_offset: u64,
    payload_len: u64,
    sha256: [u8; 32],
}

fn main() {
    if let Err(err) = run() {
        report_fatal(&err);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let self_exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let cache_root = runtime_cache_root();
    let main_bin = ensure_extracted(&self_exe, &cache_root)?;

    let forwarded: Vec<_> = std::env::args_os().skip(1).collect();
    let mut cmd = Command::new(&main_bin);
    cmd.args(&forwarded);

    if forwarded.is_empty() {
        cmd.spawn()
            .map_err(|e| format!("failed to launch {}: {e}", main_bin.display()))?;
        Ok(())
    } else {
        let status = cmd
            .status()
            .map_err(|e| format!("failed to run {}: {e}", main_bin.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn runtime_cache_root() -> PathBuf {
    if let Some(custom) = std::env::var_os("RSTREAM_PORTABLE_CACHE") {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        if !local.is_empty() {
            return PathBuf::from(local).join("rstream").join("runtime");
        }
    }
    std::env::temp_dir().join("rstream-runtime")
}

fn read_trailer(file: &mut File) -> Result<Trailer, String> {
    let total_len = file
        .metadata()
        .map_err(|e| format!("stat failed: {e}"))?
        .len();
    if total_len < TRAILER_LEN {
        return Err("missing portable payload trailer (unpacked stub binary)".into());
    }
    file.seek(SeekFrom::End(-(TRAILER_LEN as i64)))
        .map_err(|e| format!("seek trailer failed: {e}"))?;
    let mut buf = [0u8; TRAILER_LEN as usize];
    file.read_exact(&mut buf)
        .map_err(|e| format!("read trailer failed: {e}"))?;

    if &buf[40..48] != MAGIC {
        return Err("portable payload magic mismatch (unpacked stub binary)".into());
    }
    let payload_len = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let trailer_and_payload = payload_len
        .checked_add(TRAILER_LEN)
        .ok_or_else(|| "corrupt portable payload length".to_string())?;
    if payload_len == 0 || trailer_and_payload > total_len {
        return Err("corrupt portable payload length".into());
    }
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(&buf[8..40]);
    Ok(Trailer {
        payload_offset: total_len - trailer_and_payload,
        payload_len,
        sha256,
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn validate_entry_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.contains('\0')
    {
        return Err(format!("unsafe archive entry name: {name:?}"));
    }
    Ok(())
}

fn read_valid_ready_exe(slot_dir: &Path, full_hex: &str) -> Option<PathBuf> {
    let ready_marker = slot_dir.join(".ready");
    let meta = fs::symlink_metadata(&ready_marker).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return None;
    }
    let marker = fs::read_to_string(&ready_marker).ok()?;
    let mut lines = marker.lines();
    let recorded_hash = lines.next().unwrap_or_default().trim();
    let recorded_exe = lines.next().unwrap_or("rstream.exe").trim();
    if recorded_hash != full_hex || validate_entry_name(recorded_exe).is_err() {
        return None;
    }
    let exe_path = slot_dir.join(recorded_exe);
    let exe_meta = fs::symlink_metadata(&exe_path).ok()?;
    (exe_meta.is_file() && !exe_meta.file_type().is_symlink()).then_some(exe_path)
}

fn ensure_extracted(archive_path: &Path, cache_root: &Path) -> Result<PathBuf, String> {
    let mut file =
        File::open(archive_path).map_err(|e| format!("open {}: {e}", archive_path.display()))?;
    let trailer = read_trailer(&mut file)?;
    let full_hex = hex_encode(&trailer.sha256);
    let slot_id = &full_hex[..16];
    let slot_dir = cache_root.join(slot_id);

    if let Some(exe_path) = read_valid_ready_exe(&slot_dir, &full_hex) {
        return Ok(exe_path);
    }

    fs::create_dir_all(cache_root)
        .map_err(|e| format!("create cache dir {}: {e}", cache_root.display()))?;

    // Verify compressed payload integrity before unpacking.
    file.seek(SeekFrom::Start(trailer.payload_offset))
        .map_err(|e| format!("seek payload failed: {e}"))?;
    let mut hasher = Sha256::new();
    let mut reader = (&mut file).take(trailer.payload_len);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| format!("read payload failed: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    if digest != trailer.sha256 {
        return Err("portable payload SHA-256 mismatch (corrupted download)".into());
    }

    let staging = cache_root.join(format!(
        ".staging-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("create staging dir: {e}"))?;

    let unpack_res = (|| -> Result<String, String> {
        file.seek(SeekFrom::Start(trailer.payload_offset))
            .map_err(|e| format!("seek payload failed: {e}"))?;
        let bounded = (&mut file).take(trailer.payload_len);
        let mut z = ZlibDecoder::new(bounded);

        let mut u32_buf = [0u8; 4];
        z.read_exact(&mut u32_buf)
            .map_err(|e| format!("read entry count: {e}"))?;
        let count = u32::from_le_bytes(u32_buf);
        if count == 0 || count > 128 {
            return Err(format!("invalid portable entry count: {count}"));
        }

        let mut total_extracted: u64 = 0;
        let mut primary_exe: Option<String> = None;
        for _ in 0..count {
            z.read_exact(&mut u32_buf)
                .map_err(|e| format!("read name len: {e}"))?;
            let name_len = u32::from_le_bytes(u32_buf) as usize;
            if name_len == 0 || name_len > 255 {
                return Err(format!("invalid entry name length: {name_len}"));
            }
            let mut name_bytes = vec![0u8; name_len];
            z.read_exact(&mut name_bytes)
                .map_err(|e| format!("read name: {e}"))?;
            let name =
                String::from_utf8(name_bytes).map_err(|_| "non-UTF8 entry name".to_string())?;
            validate_entry_name(&name)?;

            let mut u64_buf = [0u8; 8];
            z.read_exact(&mut u64_buf)
                .map_err(|e| format!("read file size for {name}: {e}"))?;
            let size = u64::from_le_bytes(u64_buf);
            total_extracted = total_extracted
                .checked_add(size)
                .filter(|&total| total <= MAX_EXTRACTED_BYTES)
                .ok_or_else(|| "portable archive exceeds 1 GiB unpacked limit".to_string())?;

            let dest = staging.join(&name);
            let mut out = File::create(&dest).map_err(|e| format!("create {name}: {e}"))?;
            let mut entry_reader = (&mut z).take(size);
            let copied =
                io::copy(&mut entry_reader, &mut out).map_err(|e| format!("unpack {name}: {e}"))?;
            if copied != size {
                return Err(format!("truncated entry {name} ({copied} != {size})"));
            }
            #[cfg(unix)]
            if name == "rstream" || name.ends_with(".exe") {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o755));
            }
            if primary_exe.is_none() || name.eq_ignore_ascii_case("rstream.exe") {
                primary_exe = Some(name);
            }
        }
        let exe_name = primary_exe.ok_or_else(|| "archive contained no executable".to_string())?;
        fs::write(staging.join(".ready"), format!("{full_hex}\n{exe_name}\n"))
            .map_err(|e| format!("write .ready: {e}"))?;
        Ok(exe_name)
    })();

    let exe_name = match unpack_res {
        Ok(name) => name,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // Re-check `.ready` so a concurrent launcher that finished first is never deleted.
    if let Some(existing_exe) = read_valid_ready_exe(&slot_dir, &full_hex) {
        let _ = fs::remove_dir_all(&staging);
        prune_old_runtimes(cache_root, slot_id);
        return Ok(existing_exe);
    }
    if slot_dir.exists() {
        let _ = fs::remove_dir_all(&slot_dir);
    }
    if let Err(e) = fs::rename(&staging, &slot_dir) {
        let _ = fs::remove_dir_all(&staging);
        if let Some(existing_exe) = read_valid_ready_exe(&slot_dir, &full_hex) {
            return Ok(existing_exe);
        }
        return Err(format!("activate runtime dir failed: {e}"));
    }

    prune_old_runtimes(cache_root, slot_id);
    Ok(slot_dir.join(exe_name))
}

fn is_hex_slot_name(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn prune_old_runtimes(cache_root: &Path, keep_slot: &str) {
    let Ok(entries) = fs::read_dir(cache_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str == keep_slot || !is_hex_slot_name(name_str) {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if fs::symlink_metadata(path.join(".ready"))
            .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
        {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn report_fatal(msg: &str) {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        extern "system" {
            fn MessageBoxW(hwnd: usize, text: *const u16, caption: *const u16, utype: u32) -> i32;
        }
        let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
        let title: Vec<u16> = "RStream Portable Error"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            MessageBoxW(0, text.as_ptr(), title.as_ptr(), 0x10);
        }
    }
    #[cfg(not(windows))]
    eprintln!("rstream-portable-sfx: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn make_sfx_archive_with_sizes(path: &Path, entries: &[(&str, u64, &[u8])]) {
        let mut raw_stream = Vec::new();
        raw_stream.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (name, declared_size, data) in entries {
            raw_stream.extend_from_slice(&(name.len() as u32).to_le_bytes());
            raw_stream.extend_from_slice(name.as_bytes());
            raw_stream.extend_from_slice(&declared_size.to_le_bytes());
            raw_stream.extend_from_slice(data);
        }
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&raw_stream).unwrap();
        let compressed = enc.finish().unwrap();
        let sha: [u8; 32] = Sha256::digest(&compressed).into();

        let mut out = Vec::new();
        out.extend_from_slice(b"MZ-FAKE-PE-STUB-HEADER");
        out.extend_from_slice(&compressed);
        out.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
        out.extend_from_slice(&sha);
        out.extend_from_slice(MAGIC);
        fs::write(path, out).unwrap();
    }

    fn make_sfx_archive(path: &Path, entries: &[(&str, &[u8])]) {
        let sized: Vec<_> = entries
            .iter()
            .map(|(n, d)| (*n, d.len() as u64, *d))
            .collect();
        make_sfx_archive_with_sizes(path, &sized);
    }

    #[test]
    fn extracts_reuses_cache_and_prunes_only_ready_hex_slots() {
        let dir = std::env::temp_dir().join(format!("rstream-sfx-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let cache = dir.join("cache");
        // Old valid runtime slot (16-hex + .ready) -> must be pruned.
        fs::create_dir_all(cache.join("0011223344556677")).unwrap();
        fs::write(cache.join("0011223344556677/.ready"), "old\nrstream.exe\n").unwrap();
        // Unrelated user directory & 16-hex dir without .ready -> must NOT be pruned.
        fs::create_dir_all(cache.join("user-documents")).unwrap();
        fs::create_dir_all(cache.join("8899aabbccddeeff")).unwrap();
        #[cfg(unix)]
        {
            let target = dir.join("external-target");
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join(".ready"), "ext\nrstream.exe\n").unwrap();
            std::os::unix::fs::symlink(&target, cache.join("deadbeefdeadbeef")).unwrap();
        }

        let sfx = dir.join("RStream-portable.exe");
        make_sfx_archive(
            &sfx,
            &[
                ("rstream.exe", b"binary-payload"),
                ("libmpv-2.dll", b"mpv-dll"),
                ("librclone.dll", b"rclone-dll"),
            ],
        );

        let exe1 = ensure_extracted(&sfx, &cache).unwrap();
        assert_eq!(fs::read(&exe1).unwrap(), b"binary-payload");
        assert_eq!(
            fs::read(exe1.parent().unwrap().join("libmpv-2.dll")).unwrap(),
            b"mpv-dll"
        );
        assert_eq!(
            fs::read(exe1.parent().unwrap().join("librclone.dll")).unwrap(),
            b"rclone-dll"
        );
        assert!(
            !cache.join("0011223344556677").exists(),
            "stale ready runtime slot must be pruned"
        );
        assert!(
            cache.join("user-documents").exists(),
            "non-hex directory must never be pruned"
        );
        assert!(
            cache.join("8899aabbccddeeff").exists(),
            "hex directory without .ready must never be pruned"
        );
        #[cfg(unix)]
        assert!(
            dir.join("external-target/.ready").exists(),
            "symlinked 16-hex slot must never be followed or pruned"
        );

        // Second invocation hits cached .ready marker.
        let exe2 = ensure_extracted(&sfx, &cache).unwrap();
        assert_eq!(exe1, exe2);

        // Tampered recorded_exe traversal inside .ready is rejected and safely re-extracted.
        let slot_dir = exe1.parent().unwrap();
        let ready_text = fs::read_to_string(slot_dir.join(".ready")).unwrap();
        let recorded_hash = ready_text.lines().next().unwrap();
        fs::write(
            slot_dir.join(".ready"),
            format!("{recorded_hash}\n../evil.exe\n"),
        )
        .unwrap();
        let exe3 = ensure_extracted(&sfx, &cache).unwrap();
        assert_eq!(exe3, exe1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_tampered_payload_overflow_trailer_and_oversized_bundle() {
        assert!(validate_entry_name("../evil.dll").is_err());
        assert!(validate_entry_name("sub\\evil.dll").is_err());
        assert!(validate_entry_name("C:evil.exe").is_err());
        assert!(validate_entry_name("rstream.exe").is_ok());

        let dir = std::env::temp_dir().join(format!("rstream-sfx-tamper-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let sfx = dir.join("tampered.exe");
        make_sfx_archive(&sfx, &[("rstream.exe", b"clean")]);

        // Overflowing payload_len (`u64::MAX`) must fail cleanly without panicking.
        let mut overflow_bytes = fs::read(&sfx).unwrap();
        let trailer_start = overflow_bytes.len() - TRAILER_LEN as usize;
        overflow_bytes[trailer_start..trailer_start + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let overflow_sfx = dir.join("overflow.exe");
        fs::write(&overflow_sfx, &overflow_bytes).unwrap();
        let mut f = File::open(&overflow_sfx).unwrap();
        assert!(read_trailer(&mut f).is_err());

        // Oversized (> 1 GiB) declared extraction budget is rejected before disk exhaustion.
        let bomb_sfx = dir.join("bomb.exe");
        make_sfx_archive_with_sizes(&bomb_sfx, &[("rstream.exe", MAX_EXTRACTED_BYTES + 1, b"x")]);
        let bomb_err = ensure_extracted(&bomb_sfx, &dir.join("cache-bomb")).unwrap_err();
        assert!(bomb_err.contains("1 GiB"), "unexpected error: {bomb_err}");

        // Corrupted compressed byte fails SHA-256 check.
        let mut bytes = fs::read(&sfx).unwrap();
        bytes[25] ^= 0xff;
        fs::write(&sfx, bytes).unwrap();

        let err = ensure_extracted(&sfx, &dir.join("cache")).unwrap_err();
        assert!(err.contains("SHA-256"), "unexpected error: {err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
