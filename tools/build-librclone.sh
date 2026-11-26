#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="$root/tools/librclone"
out="$root/vendor/librclone"
target="${1:-default}"

cd "$src"

build_linux_arch() {
  local rust_arch="$1" go_arch="$2" cc_override="${3:-}"
  mkdir -p "$out/linux-${rust_arch}"
  echo "Building linux-${rust_arch} librclone.so..."
  if [ -n "$cc_override" ]; then
    CGO_ENABLED=1 GOOS=linux GOARCH="$go_arch" CC="$cc_override" \
      go build -buildmode=c-shared -trimpath -o "$out/linux-${rust_arch}/librclone.so" .
  else
    CGO_ENABLED=1 GOOS=linux GOARCH="$go_arch" \
      go build -buildmode=c-shared -trimpath -o "$out/linux-${rust_arch}/librclone.so" .
  fi
}

build_linux() {
  case "$(uname -m)" in
    x86_64|amd64) build_linux_arch "x86_64" "amd64" ;;
    aarch64|arm64) build_linux_arch "aarch64" "arm64" ;;
    *)
      echo "Unsupported host Linux arch: $(uname -m)" >&2
      exit 1
      ;;
  esac
}

build_android() {
  local ndk="${ANDROID_NDK_HOME:?ANDROID_NDK_HOME must be set for Android builds}"
  local ndk_bin=""
  for cand in "$ndk"/toolchains/llvm/prebuilt/*/bin; do
    if [ -d "$cand" ]; then
      ndk_bin="$cand"
      break
    fi
  done
  [ -n "$ndk_bin" ] || { echo "NDK llvm bin not found under $ndk" >&2; exit 1; }

  mkdir -p "$out/android-x86_64" "$out/android-arm64-v8a"

  echo "Building android-x86_64 librclone.so..."
  CGO_ENABLED=1 GOOS=android GOARCH=amd64 \
    CC="$ndk_bin/x86_64-linux-android26-clang" \
    CGO_LDFLAGS="-Wl,-soname,librclone.so -Wl,-z,max-page-size=16384" \
    go build -buildmode=c-shared -trimpath -o "$out/android-x86_64/librclone.so" .

  echo "Building android-arm64-v8a librclone.so..."
  CGO_ENABLED=1 GOOS=android GOARCH=arm64 \
    CC="$ndk_bin/aarch64-linux-android26-clang" \
    CGO_LDFLAGS="-Wl,-soname,librclone.so -Wl,-z,max-page-size=16384" \
    go build -buildmode=c-shared -trimpath -o "$out/android-arm64-v8a/librclone.so" .
}

build_windows() {
  mkdir -p "$out/windows-x86_64"
  local cc="${CC_WINDOWS:-}"
  if [ -z "$cc" ]; then
    if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
      cc="x86_64-w64-mingw32-gcc"
    else
      cc="gcc"
    fi
  fi
  echo "Building windows-x86_64 librclone.dll (CC=$cc)..."
  CGO_ENABLED=1 GOOS=windows GOARCH=amd64 CC="$cc" \
    go build -buildmode=c-shared -trimpath -o "$out/windows-x86_64/librclone.dll" .
}

case "$target" in
  linux) build_linux ;;
  linux-x86_64) build_linux_arch "x86_64" "amd64" ;;
  linux-aarch64)
    if [ "$(uname -m)" = "aarch64" ] || [ "$(uname -m)" = "arm64" ]; then
      build_linux_arch "aarch64" "arm64"
    else
      build_linux_arch "aarch64" "arm64" "${CC_LINUX_AARCH64:-aarch64-linux-gnu-gcc}"
    fi
    ;;
  android) build_android ;;
  windows) build_windows ;;
  default)
    build_linux
    build_android
    ;;
  all)
    build_linux
    build_android
    build_windows
    ;;
  *)
    echo "Usage: $0 [linux|linux-x86_64|linux-aarch64|android|windows|all]" >&2
    exit 1
    ;;
esac
