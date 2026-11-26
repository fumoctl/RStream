# AGENTS.md — RStream Architecture, Conventions & Maintenance Guide

RStream is a cross-platform media client (Linux, Windows, macOS, Android & Android TV) built in **Rust** with **Dioxus 0.7**, **libmpv**, **librqbit**, **SQLite (`rusqlite`)**, and an embedded Go **librclone** shared library.

---

## 1. Workspace Layout

| Path | Purpose |
| --- | --- |
| `apps/rstream` | Main Dioxus application (`dev.rstream.app`), UI components, `libmpv` player engine, JNI/desktop bridges, downloads, and cloud sync. |
| `crates/core` (`rustmedia-core`) | Stremio addon protocol client, manifest validation, catalog/meta/stream caching, and `core.sqlite3` (`media` + `playback` state & sync primitives). |
| `crates/torrent` (`stremio-torrent`) | Hardened BitTorrent streaming & resumable file downloader (`librqbit = "=9.1.0"`), loopback HTTP range server, VPN killswitch, interface binding, and RAM-only leak probe. |
| `crates/rclone` (`rstream-rclone`) | Runtime `dlopen`/`LoadLibraryW` wrapper around `librclone` JSON-RPC (`config/*`, `operations/*`, `core/stats`, loopback HTTP stream serving, `crypt` setup, and browser OAuth callback wiring). |
| `crates/video-surface` (`veylo-video-surface`) | Native desktop video surfaces underneath the WebView (Linux `GtkGLArea` & Wayland subsurface, Windows child `HWND`, macOS CoreAnimation OpenGL layer with EDR). |
| `crates/mpv-config` (`rstream-mpv-config`) | Drop-in `mpv.conf`, `input.conf`, and `shaders/*.glsl` parser/whitelist sanitizer. |
| `crates/media-proxy` (`rustmedia-media-proxy`) | Local loopback HTTP proxy injecting per-source `behaviorHints.proxyHeaders` safely. |
| `crates/picture-check` (`veylo-picture-check`) | Y4M colour-bar generator and screenshot verifier used by `--self-test` / `--check-picture`. |
| `tools/librclone` & `tools/build-librclone.sh` | Custom CGO wrapper (`main.go`) exporting `RcloneInitialize`, `RcloneRPC`, `RcloneFreeString`, and `RcloneSetOpenURL`; outputs to `vendor/librclone/<platform>/` (`linux`, `android`, `windows`). |
| `crates/portable-sfx` & `tools/pack-windows-portable.py` | Single-file zero-install Windows self-extracting `.exe` stub (`RSTRSFX1` trailer, `rstream.ico`, cached unpack to `%LOCALAPPDATA%\rstream\runtime\<hash16>\`). |
| `tools/package-appimage.sh` & `tools/package-android-apk.sh` | Self-contained Linux (`x86_64` + `aarch64`) AppImage packager and 16 KB page-aligned signed Universal Android APK (`arm64-v8a` + `x86_64`) packager, wired to `.github/workflows/release.yml` on `v*` tags. |
| `tools/fetch-mpv-android.sh` | Fetches prebuilt `libmpv.so` + FFmpeg `.so`s + `cacert.pem` into `vendor/mpv-android/{arm64-v8a,armeabi-v7a,x86_64}`. |

---

## 2. Toolchain, Build & Verification Commands

The repository uses a Nix flake (`flake.nix`) wired through `direnv` (`.envrc`).

> **Nix tip:** Run commands via `direnv exec . <cmd>` **without** setting `NIXPKGS_ALLOW_UNSUPPORTED_SYSTEM=1` so `nix-direnv` reuses its cached devShell cleanly.

### Standard Verification Suite
```bash
direnv exec . bash -c 'set -euo pipefail
cargo clippy --workspace --all-targets -- -D warnings
RUSTFLAGS="-D warnings" cargo check --target aarch64-linux-android -p rstream
RUSTFLAGS="-D warnings" cargo check --target x86_64-linux-android -p rstream
cargo test --workspace
'
```

### Desktop Builds
```bash
direnv exec . cargo build -p rstream              # target/debug/rstream
direnv exec . cargo build --release -p rstream    # target/release/rstream
```
- `apps/rstream/build.rs` automatically stages `vendor/librclone/<os>-<arch>/librclone.{so,dll,dylib}` next to the built binary and embeds Nix `libayatana-appindicator` + `libdbusmenu-gtk3` `DT_RUNPATH` entries on Linux so system-tray `dlopen` works outside `direnv`.
- Windows builds expect `libmpv-2.dll` in `apps/rstream/windows/mpv` (or `VEYLO_MPV_DIR`).

### Android APK Builds (`targetSdk = 36`, 16 KB Page-Aligned)
Build **both** ABIs in sequence so Dioxus's generated Gradle project bundles a universal APK (`x86_64` emulator + `arm64-v8a` phones/TVs):
```bash
direnv exec . bash -c '
cd apps/rstream
dx build --platform android --target x86_64-linux-android
dx build --platform android --target aarch64-linux-android
'
# Output APK:
# target/dx/rstream/debug/android/app/app/build/outputs/apk/debug/app-debug.apk
```
- **Rebuilding `librclone`:** Run `direnv exec . ./tools/build-librclone.sh` whenever `tools/librclone/main.go` changes. Verify 16 KB ELF page alignment (`0x4000`) required by Android 15/16 (`targetSdk = 36`) on `arm64-v8a`:
  ```bash
  direnv exec . llvm-readelf -l vendor/librclone/android-arm64-v8a/librclone.so | rg LOAD
  ```

### Developer CLI Flags & Environment Variables
- `RSTREAM_DATA=/path/to/dir`: Override the app data folder (defaults to `$XDG_DATA_HOME/rstream` on Linux, `%LOCALAPPDATA%\rstream` on Windows, `~/Library/Application Support/rstream` on macOS, `Context.filesDir` on Android).
- `RSTREAM_TAB=home|library|settings`, `RSTREAM_SEARCH=<query>`, `RSTREAM_PLAY_DEMO=1|hdr|bars`.
- `rstream --sources <movie|series> <video_id> [season_ep_count]`: Prints ranked sources (never URLs).
- `rstream --addons`: Runs startup addon checks and prints versions.
- `rstream --self-test` / `rstream --check-picture <png>`: Headless playback & colour-bar verification.

---

## 3. Key Architectural Decisions

### 3.1 Cinemeta Canonical ID Backbone
- **Cinemeta (`tt...` IMDb IDs)** is the canonical identity backbone (`apps/rstream/src/backend/remote_db.rs`).
- Whenever any catalog, search result, download, or history item carries a valid IMDb ID (`tt` + digits in `id`, `imdb_id`, or `imdbId`), RStream normalizes its identity to `tt...` and strips `addon_url` from download keys (`<kind>|<canon_media_id>|<canon_video_id>`) and remote sync payloads.
- **Why:** Installing or switching third-party stream addons never fragments library progress, `bingeGroup` locks, or saved downloads, and secret-bearing addon manifest URLs never touch cloud storage. Non-IMDb IDs (`kitsu:...` standalone anime entries) fall back cleanly without breaking.

### 3.2 Human-Readable Cloud Storage + Append-Only CRDT Sync (`remote_db.rs`)
- **Visible media files** on Rclone remotes use a clean, human-browsable (Jellyfin/Plex-compatible) hierarchy:
  - Movies: `RStream/Movies/<A-Z|#>/<Title (Year)>/<Title (Year)>.<ext>`
  - Series: `RStream/Series/<A-Z|#>/<Show (Year)>/Season 01/<Show> - S01E01.<ext>` (3-part `kitsu:<id>:<ep>` IDs map to `Episode <ep>` rather than treating the Kitsu ID as a season number).
- **Two separated sync namespaces** share the same conflict-free storage engine (works identically over plain remotes and zero-knowledge `rclone crypt` remotes):
  1. **`RStream/.db/` (Shared Media Index):** Tracks downloaded files (`upsert` / `delete` tombstones) on that remote so multiple users or devices sharing a cloud drive see all cloud-saved streams immediately without scanning media folders.
  2. **`RStream/.profiles/<profile>/` (Personal User State):** Syncs Library bookmarks (`saved`), `watched` marks, `liked` feedback, per-episode/movie `position_ms` & `watched_ms` (via `Core` `sync_changes` / `sync_apply`), series `bingeGroup` locks, and per-title audio/subtitle `TrackPref`. Devices sharing a profile name (`"default"`) sync seamlessly; friends sharing one media remote can pick distinct profile names so watch histories stay isolated.
- **Write-amplification & conflict avoidance:**
  - Devices **never** overwrite a shared SQLite file on the remote. Each device writes immutable HLC-stamped (`<ms:015>-<cnt:06>-<device_id>`) JSONL batches under `events/<device_id>/<hlc>.jsonl`.
  - **2-tier compaction:** `>= 40` event files compact into `deltas/delta-<hlc>.jsonl`; `>= 15` deltas compact into `snapshots/base-<hlc>.jsonl` (carrying `"covered"` metadata so peers skip deleted source files).
  - **Checkpoint-only pushes:** Local `core.sqlite3` updates continuously during playback, whereas remote `.profiles/` uploads run only at checkpoints (playback finish/close, bookmark/watched toggles, window close-to-tray, Android pause/resume, and a coarse 5-minute playback heartbeat).

### 3.3 Source Selection, Saved Streams & `bingeGroup` Play-Next (`sources.rs`, `title.rs`)
- `autoplay_with_lock` resolves streams in strict priority order:
  1. **Downloaded copies (`SavedSource`)**: Local device files first (`SAVED`), then Rclone remote copies (`CLOUD`), surfaced both in automatic Play and at the top of `SourceSheet`.
  2. **`StreamMatcherLock` (`binge_locks` table)**: Persisted whenever a series episode starts normally (`at.is_none()`, never overwritten by mid-episode `another` fallback). Matches Episode $N+1$ by:
     - Exact `behaviorHints.bingeGroup`
     - Same season-pack `infoHash` (or synthetic pack reuse when no sibling group release exists)
     - Weighted release-group (`[EncoderA]`, `-GroupB`), quality, codec, dual-audio/language, and `per_episode_bytes` similarity (supports split-part season packs like `01~11` -> `12~22`).
     - Locked matches set `audio_known: true` so `language_check` never rejects Japanese/subbed anime episodes when the global audio preference is English.
  3. **Standard quality & language probe** (`play_order` Direct HTTP probe -> `torrent_order` seeder-tier P2P fallback).

### 3.4 Dual `TorrentEngine` & Scoped Lease Teardown (`crates/torrent`, `backend/mod.rs`)
- `Backend` owns **two** independent `stremio_torrent::TorrentEngine` instances sharing the same `VpnPolicy`:
  - `torrent` (`torrent-cache/`): Interactive player streaming.
  - `download_torrent` (`torrent-downloads/`): Resumable background downloads (`downloads.rs`).
- **CRITICAL INVARIANT — `stop_lease` vs. `stop`:**
  - `TorrentEngine::stop()` (`backend::stop_torrent()`) increments the global cancellation epoch and aborts all in-flight `prepare()` calls. Call it **only** when the user explicitly cancels the pre-playback `Finding`/`Connecting` overlay (`cancel_finding`).
  - Once `prepare_torrent` returns `(url, lease_id)`, playback teardown (`PlayerEvent::Finished` / `Failed` or stale ticket drop) **must** call `stop_torrent_lease_wait(lease_id)` / `stop_torrent_lease(lease_id)` (`TorrentEngine::stop_lease`). Calling unscoped `stop_torrent()` on episode finish races **Play Next** and causes Episode 2 to fail with `"torrent preparation cancelled"`.
- **Parallel downloads & split upload pool (`downloads.rs`):**
  - `MAX_PARALLEL_DOWNLOADS` (`1..=6`, default `3`) dynamically gates download slots via `Notify` (waking queued jobs immediately when raised in Settings).
  - For `SaveTarget::Remote`, once a file finishes staging locally (`stage-<hash>.mkv`), the worker **drops its download slot** before acquiring `RCLONE_UPLOAD_SEMAPHORE`, allowing the next episode to download while `librclone` uploads to the cloud.

---

## 4. Platform-Specific Maintenance Notes & Gotchas

### 4.1 Dioxus 0.7 Reactivity & Async Lifetime Rules
- **Never read component-scoped `Signal`s inside `dioxus::core::spawn_forever`:** If the component unmounts (e.g., user switches tabs while an OAuth or check task is awaiting), reading a dropped component signal panics with `__copy_value_hoisted`. Use component-scoped `spawn(...)` inside Settings/UI components (offloading blocking/async work via `backend::on_runtime`), and reserve `spawn_forever` strictly for tasks reading `ScopeId::ROOT` signals on `Shell` (such as `start_with` / `follow_playback_events` outliving `SourceSheet` closing).
- **Desktop System Tray (`main.rs`):**
  - `muda::MenuEvent::set_event_handler` uses a global `OnceCell`. In `dioxus-desktop = "=0.7.10"`, `set_menubar_receiver()` claims that `OnceCell` for `UserWindowEvent::MudaMenuEvent` before `set_tray_icon_receiver()` runs, so `use_tray_menu_event_handler` never fires. Always attach tray menu handlers to **`use_muda_event_handler`** (keeping `use_tray_menu_event_handler` wired as well for forward compatibility).
  - On Linux GTK3/Wayland, `tao`'s `win.set_visible(true)` is asynchronous and causes an immediate `win.set_focus()` to be dropped while `gtk_window.get_visible()` is still `false`. Always restore via `restore_desktop_window()` (`gtk_win.show_all()`, `deiconify()`, `present()`, and `webview.queue_draw()`).
  - `init_desktop_tray()` uses `.build().ok()` (never `.expect(...)`) and gates `WindowCloseBehaviour::WindowHides` on `TRAY_AVAILABLE` so headless Xvfb CI (`--self-test`) or trayless WMs close normally instead of stranding an invisible window.

### 4.2 Linux Wayland, WebKitGTK & `libmpv` Rendering
- **No CSS `backdrop-filter`:** WebKitGTK 2.52–2.54 breaks alpha compositing over `GtkGLArea` when `backdrop-filter` is present. Smoked-glass surfaces use opaque/translucent layered fills and pre-blurred artwork instead.
- **WebKitGTK DMA-BUF partial damage:** `video.rs` (`disable_webkit_partial_damage`) disables WebKitGTK's `SwapChainDamageTracker` partial-damage flags so transparent GBM FBOs composite the full HUD every frame instead of erasing controls outside `.scrubber`.
- **Multi-GPU Wayland laptops:** `select_connected_drm_render_node()` in `main.rs` pins `WEBKIT_WEB_RENDER_DEVICE_FILE` and `WEBKIT_DMABUF_RENDERER_DRM_DEVICE` to the connected display's `/dev/dri/renderD*` node when multiple GPUs exist, falling back to `WEBKIT_DISABLE_DMABUF_RENDERER=1` only when zero render nodes exist (software Xvfb).

### 4.3 Android (`targetSdk = 36`) Services, OAuth & `librclone`
- **Never unload `librclone`:** The Go runtime inside `librclone.{so,dll,dylib}` cannot be safely unloaded/reloaded in-process; `crates/rclone` loads it once per process (`OnceLock`).
- **Android SSL roots in Go:** Android does not have `/etc/ssl/certs`. `tools/librclone/main.go` populates `SSL_CERT_DIR` with `/apex/com.android.conscrypt/cacerts:/system/etc/security/cacerts` so Go's `crypto/x509` verifies HTTPS providers on Android 14–16.
- **OAuth loopback protection (`RStreamOAuthService` in `RStreamBack.kt`):** Android 14+ and OEM freezers (ColorOS/OxygenOS Hans, MIUI) freeze backgrounded apps within seconds, killing `127.0.0.1:53682` during browser sign-in (`ERR_CONNECTION_TIMED_OUT`). `RStreamBack.openUrl` starts `RStreamOAuthService` (`foregroundServiceType="shortService"` + 3-minute `PARTIAL_WAKE_LOCK`) and launches an in-task Custom Tab before handing off to the browser, then `finishOAuth` stops the service and returns RStream to the foreground as soon as the token arrives.
- **Background downloads & DB sync (`RStreamTransferService` in `RStreamBack.kt`):** Uses `foregroundServiceType="dataSync"` with `PARTIAL_WAKE_LOCK` + low-latency `WifiLock` and caches `applicationContext` at startup so Rust background workers can start/update/stop the service even after `MainActivity` is destroyed. Android 15/16 caps `dataSync` foreground services at 6 hours per 24h; `onTimeout` invokes `pause_all_downloads()` (`keep_partial = true`) and shuts down cleanly so Android never throws `RemoteServiceException`.
- **Gradle / Kotlin pinning:** Keep `org.jetbrains.kotlin:kotlin-stdlib:2.0.21!!` in `apps/rstream/Dioxus.toml`; `dx 0.7` uses Kotlin 2.0.x, which fails metadata checks if transitive dependencies pull Kotlin 2.2+.

---

## 5. Security & Privacy Invariants

1. **Zero Secret Leakage in Logs or Sync:** Never log addon manifest URLs, resolved stream URLs, provider tokens, TMDB keys, or Rclone passwords/tokens. `store::redact_diagnostic` scrubs URLs and credentials from `dev-log.txt` and `last-panic.txt`. `remote_db::scrub_loopback_urls` and `sanitized_media_json` strip `streams`, `addon_url`, and `127.0.0.1` URLs before writing to Rclone remotes.
2. **Strict File Permissions:** Sensitive local files (`settings.json`, `rclone.conf`, `downloads.sqlite`, `downloads-queue.json`, `rstream-history-export.json`) must be written with `0600` permissions (`write_private_file` / `enforce_config_permissions`), and app data/torrent cache directories use `0700` on Unix.
3. **Cross-Origin Header Isolation:** `sources.rs` forwards `behaviorHints.proxyHeaders.request` (e.g. `Authorization`) **only** to same-origin redirects, never across origins.
4. **Rclone `crypt` Companion Remotes:** Encrypted remotes created by RStream wrap a `<name>-raw` backing remote inside `<name>` (`type = crypt`). `filter_visible_rclone_remotes` hides `<name>-raw` in UI pickers so users never accidentally write unencrypted files to the backing remote, and `delete_remote` cleans up both sections together.

---

## 6. Pre-Release Checklist

- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes with 0 warnings.
- [ ] `RUSTFLAGS="-D warnings" cargo check --target aarch64-linux-android -p rstream` and `x86_64-linux-android` pass with 0 warnings.
- [ ] `cargo test --workspace` passes across all workspace crates (`rustmedia-core`, `stremio-torrent`, `rstream-rclone`, `rstream-mpv-config`, `veylo-video-surface`, `veylo-picture-check`, `rstream`).
- [ ] `cargo build --release -p rstream` succeeds and `vendor/librclone` is staged alongside the release binary.
- [ ] Universal Android APK (`x86_64` + `aarch64`) builds cleanly (`./tools/package-android-apk.sh`), `aapt2 dump xmltree` confirms `targetSdkVersion=36` + `RStreamOAuthService` + `RStreamTransferService`, and `arm64-v8a` `.so` files maintain `0x4000` (16 KB) ELF `LOAD` alignment.
- [ ] Tagging `vX.Y.Z` triggers `.github/workflows/release.yml` to publish `RStream-vX.Y.Z-x86_64.AppImage`, `RStream-vX.Y.Z-aarch64.AppImage`, `RStream-vX.Y.Z-universal.apk` (configure `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD` secrets for persistent upgrade signing), `RStream-vX.Y.Z-windows-x86_64.exe`, and `SHA256SUMS.txt`.
