# rstream: Dioxus shell prototype

Home, Picks, Search and the title page (episodes, sources, playback) in Rust + CSS with [Dioxus](https://dioxuslabs.com) 0.7,
running on `crates/core` directly. Desktop (Linux, Windows) and Android.

```sh
cargo run            # 1200×768 window, the tablet's landscape size in dp
cargo test           # search ranking, pick filters, history, artwork accent
```

- Own Cargo workspace and lockfile on purpose: the Android packaging guard watches
  the root workspace, so this crate never makes `librustmedia.so` look stale.
- Data lives in `$XDG_DATA_HOME/rstream` (override with `RSTREAM_DATA`), never the
  device library. First run loads the bundled Cinemeta and Kitsu catalogs.
- Linux needs `webkit2gtk-4.1`, `xdotool` (libxdo) and `mpv` (libmpv).
- Windows needs WebView2 (built into Windows 10/11) and `libmpv-2.dll` beside the exe: `windows/fetch-mpv.ps1`
  fetches a pinned build and `build.rs` links and copies it. Data lives in `%LOCALAPPDATA%\rstream`.
  The app icon is embedded from `windows/rstream.ico` (`branding/rstream-icon.png`); release builds have no console window.
- Keys: Ctrl+K or `/` search, Esc closes, L like, N not for me, ←/→ browse picks.
- Look: smoked glass lit by the title in front (an ambient blurred copy of its
  artwork). Icon-first controls; every icon button has a tooltip and screen-reader label.
  `backdrop-filter` is an enhancement only: WebKitGTK doesn't render it, so overlays
  carry their own blurred artwork.
- Player: one Rust player on every OS, libmpv (`src/engine.rs`) with Dioxus controls (`src/player_ui.rs`);
  only the picture's bridge differs (`src/video.rs`, desktop paths in `crates/video-surface`): Android a SurfaceView under the WebView
  (`android/kotlin/VeyloVideo.kt`, MediaCodec), Linux a GL area or Wayland subsurface (VAAPI),
  Windows mpv's child window under WebView2 (D3D11). Progress is recorded through the core.
- Sources: direct links play; torrents (P2P consent) and external links are shown, not played.
- Dev options: `RSTREAM_PLAY_DEMO=1` (plays the test stream at launch; `=bars` plays colour bars for
  `rstream --check-picture <screenshot.png>`), `RSTREAM_SKIP_PICTURE_PATH=wayland-subsurface` (try the next picture path), `RSTREAM_TAB=home|library|settings`, `RSTREAM_SEARCH=<query>`,
  `RSTREAM_OPEN=<library title>`. In a title sheet, P plays the best source.

## Android

```sh
# ARM64 Linux hosts: ~/Android/host-tools/ndk-shim mirrors NDK r28 with wrappers around the
# system clang/lld (the NDK's own compilers are x86-only); x86-64 hosts use the real NDK.
export ANDROID_HOME=~/Android/Sdk JAVA_HOME=~/Android/jdk17 ANDROID_NDK_HOME=~/Android/host-tools/ndk-shim
export QEMU_LD_PREFIX=~/Android/host-tools/x86_64-root   # runs Gradle's x86 aapt2 under qemu
dx build --android --target aarch64-linux-android
adb -s "$VEYLO_ADB_SERIAL" install -r target/dx/rstream/debug/android/app/app/build/outputs/apk/debug/app-debug.apk
```

Installs as `dev.rstream.app`, beside the Kotlin app, with its own private store. Run
`android/fetch-mpv.sh` once first: the player links that libmpv. The Kotlin stdlib is pinned
because dx 0.7.10 compiles Kotlin 2.0.20. A panic is written to `files/last-panic.txt`
(some Android builds hide app logs).

- Not here yet: Library, Settings, subtitles and audio-track choice, P2P with consent.
