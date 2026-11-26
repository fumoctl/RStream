#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/vendor/mpv-android"
base_url="https://github.com/mpv-android/mpv-android/releases/download/2026-09-17"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

mkdir -p "$out"

for abi in x86_64 arm64-v8a armeabi-v7a; do
  apk="$tmpdir/app-default-${abi}-release.apk"
  echo "Downloading mpv-android (${abi})..."
  curl -fSL -o "$apk" "${base_url}/app-default-${abi}-release.apk"
  mkdir -p "$out/$abi"
  unzip -joq "$apk" "lib/${abi}/*.so" -x "lib/${abi}/libplayer.so" -d "$out/$abi"
  if [ ! -f "$out/cacert.pem" ]; then
    unzip -joq "$apk" "assets/cacert.pem" -d "$out"
  fi
done
