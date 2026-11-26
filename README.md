<p align="center">
  <img src="branding/rstream-icon.svg" width="112" height="112" alt="RStream logo" />
</p>

<h1 align="center">RStream</h1>

<p align="center">
  Fast, native cross-platform media player & library for <strong>Linux</strong>, <strong>Windows</strong>, <strong>Android</strong>, and <strong>Android TV</strong>.<br />
  Built in Rust with Dioxus, <code>libmpv</code>, <code>librqbit</code>, SQLite, and embedded <code>librclone</code>.
</p>

---

> [!WARNING]
> **VPN killswitch and interface-binding support is still experimental.**

---

## Features

- **Native `libmpv` Playback Everywhere**: Hardware-accelerated playback across Linux (Wayland & X11 VAAPI), Windows (D3D11), macOS (CoreAnimation EDR), and Android / Android TV (MediaCodec `SurfaceView` + full D-pad / Leanback TV navigation), with custom `mpv.conf` & GLSL shader support.
- **Stremio-Compatible Addon Protocol (Unaffiliated) & Smart `bingeGroup` Play-Next**: Uses Cinemeta (`tt...`) as a canonical identity backbone so switching or combining compatible addons never fragments your watch progress or downloads. Series playback automatically locks onto matching release groups, season packs, quality, and audio preferences for seamless next-episode transitions.
- **Offline & Cloud Downloads (`librclone`)**: Resumable parallel downloads to local storage or directly to 70+ cloud storage providers via embedded `librclone` — including zero-knowledge client-side **`rclone crypt`** encryption. Downloaded titles appear automatically as top-priority `SAVED` / `CLOUD` playback sources in a clean, Jellyfin/Plex-compatible folder layout (unaffiliated).
- **Serverless Cross-Device Sync**: Sync your Library bookmarks, watched history, playback positions, `bingeGroup` locks, and per-title audio/subtitle preferences across desktop, phone, and TV over your own Rclone remote (`RStream/.profiles/<profile>/`) using conflict-free append-only HLC logs — no central account server required.
- **Background Transfers**: Minimize-to-tray support on desktop and an Android `dataSync` foreground notification service keep queued downloads and cloud sync running cleanly in the background.

---

## Downloads

Self-contained, zero-dependency packages are built automatically on every release tag (`v*`):

| Platform | Package | Notes |
| --- | --- | --- |
| **Linux (`x86_64`)** | `RStream-vX.Y.Z-x86_64.AppImage` | Self-contained (`chmod +x` and run) |
| **Linux (`ARM64` / `aarch64`)** | `RStream-vX.Y.Z-aarch64.AppImage` | Self-contained (`chmod +x` and run) |
| **Windows (`x86_64`)** | `RStream-vX.Y.Z-windows-x86_64.exe` | Single-file portable self-extracting `.exe` (static CRT; zero installer or VC++ redist needed) |
| **Android & Android TV** | `RStream-vX.Y.Z-universal.apk` | Signed 16 KB page-aligned Universal APK (`arm64-v8a` + `x86_64`, `minSdk 26` / `targetSdk 36`) |

Grab the latest binaries and `SHA256SUMS.txt` from the [**Releases**](../../releases) page.

---

## Building from Source

The repository includes a reproducible Nix development environment ([`flake.nix`](flake.nix) + `.envrc`), or can be built with standard Rust (`>= 1.85`), Go (`>= 1.26`), and platform SDKs.

### Nix (`direnv`)
```bash
direnv allow
direnv exec . ./tools/build-librclone.sh linux
direnv exec . cargo run --release -p rstream
```

### Packaging Release Artifacts Locally
```bash
# Linux AppImage (x86_64 or aarch64)
./tools/package-appimage.sh

# Android Universal APK (requires Android SDK 36 + NDK 28 + dioxus-cli 0.7.10)
./tools/package-android-apk.sh

# Windows single-file portable .exe
cargo build --release -p rstream -p rstream-portable-sfx
python3 tools/pack-windows-portable.py \
  target/release/rstream-portable-sfx.exe \
  dist/RStream-v0.1.0-windows-x86_64.exe \
  target/release/rstream.exe \
  apps/rstream/windows/mpv/libmpv-2.dll \
  vendor/librclone/windows-x86_64/librclone.dll \
  target/release/vulkan-1.dll
```

For architectural details, storage/CRDT sync design, and platform notes, see [**AGENTS.md**](AGENTS.md).

---

## Contributing

**Pull requests are welcome!** Bug fixes, platform polish, VPN/network hardening, accessibility improvements, and documentation updates are all appreciated.

Before opening a PR, please run the verification suite:
```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

Licensed under the **GNU Affero General Public License v3.0 (`AGPL-3.0-or-later`)** — see [`LICENSE`](LICENSE). Third-party notices and upstream MIT attribution for Veylo are listed in [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).
