# Third-Party Licenses & Upstream Attributions

This document provides upstream attribution, bundled component license notices, and corresponding-source references for **RStream**, which is licensed under the **GNU Affero General Public License v3.0 or later (`AGPL-3.0-or-later`)** (see [`LICENSE`](LICENSE)).

---

## 1. Upstream Veylo Attribution (MIT License)

RStream is forked from the MIT-licensed **Veylo** project. The original MIT copyright and permission notice for upstream Veylo code is preserved below:

```text
MIT License

Copyright (c) 2026 Veylo contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

---

## 2. Bundled & Linked Native and Go Components

### rclone / `librclone` (MIT License)
- **Module / Version:** `github.com/rclone/rclone v1.73.3`
- **Corresponding Source:** <https://github.com/rclone/rclone/releases/tag/v1.73.3>
- **Build Wrapper in This Repository:** [`tools/librclone/main.go`](tools/librclone/main.go) and [`tools/librclone/go.mod`](tools/librclone/go.mod) (`module rstream/librclone`, `go 1.26`).

### `mpv` / `libmpv` (GPLv2+ / LGPLv2.1+) and FFmpeg (GPLv2+ / GPLv3+)
When distributed together with RStream in binary releases, `libmpv` and FFmpeg are combined with RStream's `AGPL-3.0-or-later` codebase in accordance with GPLv3 / AGPLv3 §13.

- **Android Prebuilts (`mpv-android`):**
  - **License:** MIT (Java/Kotlin wrapper) + bundled native `libmpv.so` and FFmpeg shared libraries (GPLv2+ / GPLv3+ / LGPLv2.1+) and Mozilla CA certificate bundle (`cacert.pem`, MPL-2.0).
  - **Release:** `2026-09-17` — <https://github.com/mpv-android/mpv-android/releases/tag/2026-09-17>
  - **Fetch Script:** [`tools/fetch-mpv-android.sh`](tools/fetch-mpv-android.sh) (downloads `app-default-x86_64-release.apk`, `app-default-arm64-v8a-release.apk`, and `app-default-armeabi-v7a-release.apk` from `https://github.com/mpv-android/mpv-android/releases/download/2026-09-17`).
- **Windows Prebuilt (`libmpv-2.dll`):**
  - **License:** GPLv2+ / GPLv3+ (`libmpv-2.dll`, `mpv` + FFmpeg).
  - **Release:** `shinchiro/mpv-winbuild-cmake` release `20261111` — <https://github.com/shinchiro/mpv-winbuild-cmake/releases/tag/20261111>
  - **Asset:** `mpv-dev-x86_64-20261111-git-bcde6b93cc.7z` (`https://github.com/shinchiro/mpv-winbuild-cmake/releases/download/20261111/mpv-dev-x86_64-20261111-git-bcde6b93cc.7z`)
  - **SHA-256:** `273c079b11ea5575822f972756f4c9a71149a5c10130353abeb8586738938452`
  - **Fetch Script:** [`apps/rstream/windows/fetch-mpv.ps1`](apps/rstream/windows/fetch-mpv.ps1)
- **Linux AppImage:**
  - Bundles `libmpv`, FFmpeg, GTK3, WebKitGTK (`webkit2gtk-4.1`), and supporting system libraries from the build host (Nix / Ubuntu 24.04 upstream distribution packages) via [`tools/package-appimage.sh`](tools/package-appimage.sh).

---

## 3. Key Rust & Go Workspace Dependencies

RStream links and embeds open-source libraries across its Rust Cargo workspace and Go `librclone` module:

- **Rust Workspace (permissive MIT / Apache-2.0 / BSD / MPL-2.0 / Zlib licenses unless noted):**
  - `dioxus` (`=0.7.10`) — cross-platform UI framework
  - `librqbit` (`=9.1.0`) — BitTorrent engine
  - `rusqlite` (`0.32`, bundled SQLite) — embedded database
  - `reqwest` (`0.12`) & `rustls` (via `rustls-tls`) — HTTP/TLS client stack
  - `tokio` (`1`) & `tokio-util` (`0.7`) — asynchronous runtime
  - `libmpv2` (`7.0.1`) / `libmpv2-sys` (`4.0.1`) — Rust bindings to `libmpv`
  - `flate2` (`1`), `sha2` (`0.10`), `serde` (`1`), `serde_json` (`1`), `url` (`2`), `anyhow` (`1`), `regex` (`1`)
  - See [`Cargo.lock`](Cargo.lock) and the workspace `Cargo.toml` manifests for the complete list of direct and transitive Rust crates and their exact versions.
- **Go `librclone` Module:**
  - `github.com/rclone/rclone v1.73.3` and its Go dependency graph (AWS SDK, Google Cloud/API, Azure SDK, OpenTelemetry, `golang.org/x/*`, etc.).
  - See [`tools/librclone/go.mod`](tools/librclone/go.mod) for the complete list of direct and indirect Go modules and exact pinned versions.
