#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${RSTREAM_VERSION:-$(git -C "$root" describe --tags --exact-match 2>/dev/null || echo "v0.1.0")}"
semver="${version#v}"
dist_dir="$root/dist"
work_dir="$(mktemp -d)"
chmod 0700 "$work_dir"
trap 'rm -rf "$work_dir"' EXIT

mkdir -p "$dist_dir"

if [[ "${GITHUB_REF:-}" == refs/tags/v* ]] && [ -z "${ANDROID_KEYSTORE_BASE64:-}" ] && [ -z "${ANDROID_KEYSTORE_PATH:-}" ]; then
  echo "Error: Tagged release (${GITHUB_REF}) requires ANDROID_KEYSTORE_BASE64 or ANDROID_KEYSTORE_PATH for persistent upgrade signing." >&2
  exit 1
fi

sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:?ANDROID_HOME or ANDROID_SDK_ROOT must be set}}"
ndk="${ANDROID_NDK_HOME:-$(ls -1d "$sdk"/ndk/* 2>/dev/null | sort -V | tail -n 1)}"
[ -n "$ndk" ] && [ -d "$ndk" ] || { echo "ANDROID_NDK_HOME not found under $sdk/ndk" >&2; exit 1; }
export ANDROID_NDK_HOME="$ndk"

ndk_bin=""
for cand in "$ndk"/toolchains/llvm/prebuilt/*/bin; do
  if [ -d "$cand" ]; then
    ndk_bin="$cand"
    break
  fi
done
[ -n "$ndk_bin" ] || { echo "NDK llvm bin not found under $ndk" >&2; exit 1; }

export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$ndk_bin/x86_64-linux-android26-clang"
export CC_x86_64_linux_android="$ndk_bin/x86_64-linux-android26-clang"
export CXX_x86_64_linux_android="$ndk_bin/x86_64-linux-android26-clang++"
export AR_x86_64_linux_android="$ndk_bin/llvm-ar"

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$ndk_bin/aarch64-linux-android26-clang"
export CC_aarch64_linux_android="$ndk_bin/aarch64-linux-android26-clang"
export CXX_aarch64_linux_android="$ndk_bin/aarch64-linux-android26-clang++"
export AR_aarch64_linux_android="$ndk_bin/llvm-ar"

bt_dir="$(ls -1d "$sdk"/build-tools/* | sort -V | tail -n 1)"
zipalign="$bt_dir/zipalign"
apksigner="$bt_dir/apksigner"
[ -x "$zipalign" ] || { echo "zipalign not found at $zipalign" >&2; exit 1; }
[ -x "$apksigner" ] || { echo "apksigner not found at $apksigner" >&2; exit 1; }

if [ ! -f "$root/vendor/mpv-android/arm64-v8a/libmpv.so" ] || [ ! -f "$root/vendor/mpv-android/x86_64/libmpv.so" ]; then
  "$root/tools/fetch-mpv-android.sh"
fi

if [ ! -f "$root/vendor/librclone/android-arm64-v8a/librclone.so" ] || [ ! -f "$root/vendor/librclone/android-x86_64/librclone.so" ]; then
  "$root/tools/build-librclone.sh" android
fi

# Compute monotonic integer versionCode from X.Y.Z (e.g. 0.1.0 -> 100, fallback 1).
version_code="$(python3 - "$semver" <<'PY'
import re, sys
m = re.match(r"^(\d+)\.(\d+)\.(\d+)", sys.argv[1])
if m:
    maj, mi, pa = (int(x) for x in m.groups())
    print(max(1, maj * 10000 + mi * 100 + pa))
else:
    print(1)
PY
)"

# shortcut: Dioxus 0.7.10 (manganis) supports 64-bit Android only; add armv7-linux-androideabi when upstream drops the 64-bit guard.
pushd "$root/apps/rstream" >/dev/null
dx build --platform android --release --target x86_64-linux-android
dx build --platform android --release --target aarch64-linux-android
popd >/dev/null

gradle_root="$root/target/dx/rstream/release/android/app"
gradle_file="$gradle_root/app/build.gradle.kts"
[ -f "$gradle_file" ] || { echo "Expected Gradle project at $gradle_file" >&2; exit 1; }

python3 - "$gradle_file" "$version_code" "$semver" <<'PY'
import pathlib, re, sys
p = pathlib.Path(sys.argv[1])
code, name = sys.argv[2], sys.argv[3]
txt = p.read_text()
txt = re.sub(r"versionCode\s*=\s*\d+", f"versionCode = {code}", txt)
txt = re.sub(r'versionName\s*=\s*"[^"]*"', f'versionName = "{name}"', txt)
p.write_text(txt)
PY

mkdir -p "$gradle_root/app/src/main/assets"
cp "$root/LICENSE" "$root/THIRD_PARTY_LICENSES.md" "$gradle_root/app/src/main/assets/"

pushd "$gradle_root" >/dev/null
chmod +x ./gradlew
./gradlew assembleRelease --no-daemon
popd >/dev/null

unsigned_apk="$(find "$gradle_root/app/build/outputs/apk/release" -maxdepth 1 -name '*.apk' | head -n 1)"
[ -n "$unsigned_apk" ] && [ -f "$unsigned_apk" ] || { echo "No release APK produced under $gradle_root" >&2; exit 1; }

# Verify Universal APK bundles both 64-bit ABIs + libmpv + librclone + cacert.pem + licenses.
apk_contents="$(unzip -l "$unsigned_apk")"
for abi in arm64-v8a x86_64; do
  for lib in libmain.so libmpv.so librclone.so; do
    if ! grep -q "lib/${abi}/${lib}" <<<"$apk_contents"; then
      echo "Universal APK check failed: missing lib/${abi}/${lib}" >&2
      exit 1
    fi
  done
done
grep -q "assets/cacert.pem" <<<"$apk_contents" || { echo "Universal APK missing assets/cacert.pem" >&2; exit 1; }
grep -q "assets/LICENSE" <<<"$apk_contents" || { echo "Universal APK missing assets/LICENSE" >&2; exit 1; }
grep -q "assets/THIRD_PARTY_LICENSES.md" <<<"$apk_contents" || { echo "Universal APK missing assets/THIRD_PARTY_LICENSES.md" >&2; exit 1; }

aligned_apk="$work_dir/app-aligned.apk"
"$zipalign" -P 16 -f 4 "$unsigned_apk" "$aligned_apk"

keystore="$work_dir/release.jks"
if [ -n "${ANDROID_KEYSTORE_BASE64:-}" ]; then
  printf '%s' "$ANDROID_KEYSTORE_BASE64" | base64 -d > "$keystore"
  chmod 0600 "$keystore"
  ks_pass="${ANDROID_KEYSTORE_PASSWORD:?ANDROID_KEYSTORE_PASSWORD is required when ANDROID_KEYSTORE_BASE64 is set}"
  key_alias="${ANDROID_KEY_ALIAS:?ANDROID_KEY_ALIAS is required when ANDROID_KEYSTORE_BASE64 is set}"
  key_pass="${ANDROID_KEY_PASSWORD:-$ks_pass}"
elif [ -n "${ANDROID_KEYSTORE_PATH:-}" ]; then
  cp "$ANDROID_KEYSTORE_PATH" "$keystore"
  chmod 0600 "$keystore"
  ks_pass="${ANDROID_KEYSTORE_PASSWORD:?ANDROID_KEYSTORE_PASSWORD is required}"
  key_alias="${ANDROID_KEY_ALIAS:?ANDROID_KEY_ALIAS is required}"
  key_pass="${ANDROID_KEY_PASSWORD:-$ks_pass}"
else
  if [[ "${GITHUB_REF:-}" == refs/tags/v* ]]; then
    echo "Error: Tagged release (${GITHUB_REF}) requires a persistent signing keystore." >&2
    exit 1
  fi
  echo "Notice: ANDROID_KEYSTORE_BASE64 not set; generating ephemeral release signing key."
  ks_pass="rstream-release-ephemeral"
  key_alias="rstream"
  key_pass="$ks_pass"
  keytool -genkeypair -noprompt \
    -keystore "$keystore" \
    -storepass "$ks_pass" \
    -keypass "$key_pass" \
    -alias "$key_alias" \
    -keyalg RSA -keysize 2048 -validity 10000 \
    -dname "CN=RStream,O=RStream,C=US" >/dev/null 2>&1
  chmod 0600 "$keystore"
fi

out_apk="$dist_dir/RStream-${version}-universal.apk"
"$apksigner" sign \
  --ks "$keystore" \
  --ks-key-alias "$key_alias" \
  --ks-pass "pass:${ks_pass}" \
  --key-pass "pass:${key_pass}" \
  --out "$out_apk" \
  "$aligned_apk"

"$apksigner" verify --verbose "$out_apk" >/dev/null
"$zipalign" -c -P 16 4 "$out_apk"
echo "Built signed 16KB-aligned Universal Android APK: $out_apk"
