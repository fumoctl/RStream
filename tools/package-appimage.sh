#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${RSTREAM_VERSION:-$(git -C "$root" describe --tags --exact-match 2>/dev/null || echo "v0.1.0")}"
dist_dir="$root/dist"
cache_dir="$root/target/appimage-tools"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

case "$(uname -m)" in
  x86_64|amd64)
    arch="x86_64"
    ldconfig_pat="x86-64"
    gnu_triple="x86_64-linux-gnu"
    ld_so_name="ld-linux-x86-64.so.2"
    ;;
  aarch64|arm64)
    arch="aarch64"
    ldconfig_pat="AArch64"
    gnu_triple="aarch64-linux-gnu"
    ld_so_name="ld-linux-aarch64.so.1"
    ;;
  *)
    echo "Unsupported Linux architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

mkdir -p "$dist_dir" "$cache_dir"

rclone_so="$root/vendor/librclone/linux-${arch}/librclone.so"
if [ ! -f "$rclone_so" ]; then
  "$root/tools/build-librclone.sh" "linux-${arch}"
fi

if [ "${SKIP_CARGO_BUILD:-0}" != "1" ]; then
  cargo build --release -p rstream --manifest-path "$root/Cargo.toml"
fi

bin_path="$root/target/release/rstream"
[ -f "$bin_path" ] || { echo "Missing $bin_path" >&2; exit 1; }
[ -f "$rclone_so" ] || { echo "Missing $rclone_so" >&2; exit 1; }

fetch_tool() {
  local name="$1" url="$2"
  if [ ! -x "$cache_dir/$name" ]; then
    echo "Downloading $name..."
    curl -fSL --retry 3 -o "$cache_dir/$name.tmp" "$url"
    chmod +x "$cache_dir/$name.tmp"
    mv "$cache_dir/$name.tmp" "$cache_dir/$name"
  fi
}

appimagetool_bin="appimagetool-${arch}.AppImage"
fetch_tool "$appimagetool_bin" \
  "https://github.com/AppImage/appimagetool/releases/download/continuous/${appimagetool_bin}"

appdir="$work_dir/AppDir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" \
  "$appdir/usr/share/icons/hicolor/512x512/apps" "$appdir/usr/share/icons/hicolor/scalable/apps" \
  "$appdir/usr/share/licenses/rstream"

cp "$root/LICENSE" "$root/THIRD_PARTY_LICENSES.md" "$appdir/usr/share/licenses/rstream/"

cp "$root/branding/rstream-icon.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/dev.rstream.app.svg"

icon_512="$appdir/usr/share/icons/hicolor/512x512/apps/dev.rstream.app.png"
if command -v rsvg-convert >/dev/null 2>&1; then
  rsvg-convert -w 512 -h 512 "$root/branding/rstream-icon.svg" -o "$icon_512"
elif command -v magick >/dev/null 2>&1; then
  magick "$root/branding/rstream-icon.png" -resize 512x512 "$icon_512"
elif command -v convert >/dev/null 2>&1; then
  convert "$root/branding/rstream-icon.png" -resize 512x512 "$icon_512"
elif command -v ffmpeg >/dev/null 2>&1; then
  ffmpeg -loglevel error -y -i "$root/branding/rstream-icon.png" -vf scale=512:512 "$icon_512"
else
  cp "$root/branding/rstream-icon.png" "$icon_512"
fi
cp "$icon_512" "$appdir/dev.rstream.app.png"
ln -sf "dev.rstream.app.png" "$appdir/.DirIcon"

desktop_file="$appdir/usr/share/applications/dev.rstream.app.desktop"
cat > "$desktop_file" <<'EOF'
[Desktop Entry]
Type=Application
Name=RStream
Comment=Cross-platform media player and library
Exec=rstream
Icon=dev.rstream.app
Terminal=false
Categories=AudioVideo;Video;Player;
StartupWMClass=rstream
EOF
cp "$desktop_file" "$appdir/dev.rstream.app.desktop"

find_shared_lib() {
  local pkg="$1" soname="$2"
  local dir=""
  dir="$(pkg-config --variable=libdir "$pkg" 2>/dev/null || true)"
  if [ -n "$dir" ] && [ -f "$dir/$soname" ]; then
    echo "$dir/$soname"
    return
  fi
  ldconfig -p 2>/dev/null | awk -v n="$soname" -v pat="$ldconfig_pat" '$1 == n && $0 ~ pat { print $NF; exit }' || true
}

appind_so="$(find_shared_lib ayatana-appindicator3-0.1 libayatana-appindicator3.so.1)"
dbusmenu_so="$(find_shared_lib dbusmenu-gtk3-0.4 libdbusmenu-gtk3.so.4)"

# Keep isolated helper directories in usr/rstream-libs so NixOS appimage-run and child host binaries
# (xdg-open browser for OAuth, /bin/sh) never inherit incompatible glibc/GIO modules.
privlib="$appdir/usr/rstream-libs"
mkdir -p "$privlib"

if [ -d /nix/store ]; then
  echo "Detected Nix store: assembling self-contained ${arch} ELF closure..."
  cp "$bin_path" "$appdir/usr/bin/rstream.bin"
  chmod 0755 "$appdir/usr/bin/rstream.bin"

  wk_prefix="$(pkg-config --variable=prefix webkit2gtk-4.1)"
  mkdir -p "$privlib/webkit2gtk-4.1"
  if [ -d "$wk_prefix/lib/webkit2gtk-4.1/injected-bundle" ]; then
    mkdir -p "$privlib/webkit2gtk-4.1/injected-bundle"
    cp -L "$wk_prefix/lib/webkit2gtk-4.1/injected-bundle/"*.so "$privlib/webkit2gtk-4.1/injected-bundle/"
  fi
  for proc in WebKitWebProcess WebKitNetworkProcess WebKitGPUProcess; do
    for cand in "$wk_prefix/libexec/webkit2gtk-4.1/$proc" "$wk_prefix/lib/webkit2gtk-4.1/$proc"; do
      if [ -f "$cand" ]; then
        cp -L "$cand" "$privlib/webkit2gtk-4.1/${proc}.bin"
        chmod 0755 "$privlib/webkit2gtk-4.1/${proc}.bin"
        cat > "$privlib/webkit2gtk-4.1/$proc" <<EOF
#!/bin/sh
HERE="\$(CDPATH= cd -- "\$(dirname -- "\$0")/.." && pwd)"
lib_path="\$HERE"
if [ -d /run/opengl-driver/lib ]; then
  lib_path="\$lib_path:/run/opengl-driver/lib"
fi
exec "\$HERE/$ld_so_name" --library-path "\$lib_path" "\$0.bin" "\$@"
EOF
        chmod 0755 "$privlib/webkit2gtk-4.1/$proc"
        break
      fi
    done
  done

  mkdir -p "$privlib/gio/modules"
  IFS=':' read -ra gio_dirs <<< "${GIO_EXTRA_MODULES:-}"
  for gdir in "${gio_dirs[@]}"; do
    if [ -n "$gdir" ] && [ -d "$gdir" ]; then
      for gmod in libgiognutls.so libgioopenssl.so libgioenvironmentproxy.so libdconfsettings.so; do
        [ -f "$gdir/$gmod" ] && cp -L "$gdir/$gmod" "$privlib/gio/modules/" || true
      done
    fi
  done

  pixbuf_moddir="$(pkg-config --variable=gdk_pixbuf_moduledir gdk-pixbuf-2.0 2>/dev/null || true)"
  if [ -n "$pixbuf_moddir" ] && [ -d "$pixbuf_moddir" ]; then
    mkdir -p "$privlib/gdk-pixbuf-2.0/loaders"
    cp -L "$pixbuf_moddir/"*.so "$privlib/gdk-pixbuf-2.0/loaders/" 2>/dev/null || true
    pixbuf_query="$(pkg-config --variable=gdk_pixbuf_query_loaders gdk-pixbuf-2.0 2>/dev/null || command -v gdk-pixbuf-query-loaders || true)"
    if [ -n "$pixbuf_query" ] && [ -x "$pixbuf_query" ]; then
      escaped_appdir="$(printf '%s\n' "$appdir" | sed 's/[][\\.*^$|]/\\&/g')"
      "$pixbuf_query" "$privlib/gdk-pixbuf-2.0/loaders/"*.so \
        | sed "s|$escaped_appdir|@APPDIR@|g" > "$privlib/gdk-pixbuf-2.0/loaders.cache.in"
    fi
  fi

  mkdir -p "$appdir/usr/share/glib-2.0/schemas"
  gtk_prefix="$(pkg-config --variable=prefix gtk+-3.0 2>/dev/null || true)"
  for sroot in "$gtk_prefix" /nix/store/*gsettings-desktop-schemas*; do
    if [ -n "$sroot" ] && [ -d "$sroot/share/gsettings-schemas" ]; then
      find "$sroot/share/gsettings-schemas" -name '*.xml' -exec cp -Lf {} "$appdir/usr/share/glib-2.0/schemas/" \;
    fi
  done
  if command -v glib-compile-schemas >/dev/null 2>&1 && ls "$appdir/usr/share/glib-2.0/schemas/"*.xml >/dev/null 2>&1; then
    chmod -R u+w "$appdir/usr/share/glib-2.0/schemas"
    glib-compile-schemas "$appdir/usr/share/glib-2.0/schemas"
  fi

  seeds=("$appdir/usr/bin/rstream.bin")
  [ -n "$appind_so" ] && [ -f "$appind_so" ] && cp -Lf "$appind_so" "$privlib/" && seeds+=("$appind_so")
  [ -n "$dbusmenu_so" ] && [ -f "$dbusmenu_so" ] && cp -Lf "$dbusmenu_so" "$privlib/" && seeds+=("$dbusmenu_so")
  while IFS= read -r -d '' f; do seeds+=("$f"); done < <(
    find "$privlib/webkit2gtk-4.1" "$privlib/gio/modules" "$privlib/gdk-pixbuf-2.0" \
      -type f \( -name '*.bin' -o -name '*.so*' \) -print0 2>/dev/null || true
  )

  interp="$(patchelf --print-interpreter "$bin_path")"
  cp -Lf "$interp" "$privlib/$ld_so_name"

  declare -A seen=()
  queue=("${seeds[@]}")
  while [ "${#queue[@]}" -gt 0 ]; do
    cur="${queue[0]}"
    queue=("${queue[@]:1}")
    while read -r path; do
      [ -n "$path" ] && [ -f "$path" ] || continue
      base="$(basename "$path")"
      case "$base" in
        libEGL_mesa.so*|libGLX_mesa.so*|libEGL_nvidia.so*|libGLX_nvidia.so*|libvulkan_*.so*|ld-linux-*.so*)
          continue
          ;;
      esac
      if [ -z "${seen[$base]:-}" ]; then
        seen[$base]=1
        cp -Lf "$path" "$privlib/$base"
        queue+=("$path")
      fi
    done < <(ldd "$cur" 2>/dev/null | awk '/=> \// { print $3 } /^\t\// { print $1 }')
  done

  chmod -R u+w "$appdir"

  # Relocate hardcoded /nix/store/.../webkit2gtk-4.1 paths inside libwebkit2gtk-4.1.so.0 to ./rstream-libs/webkit2gtk-4.1.
  if [ -f "$privlib/libwebkit2gtk-4.1.so.0" ]; then
    python3 - "$privlib/libwebkit2gtk-4.1.so.0" "$wk_prefix" <<'PY'
import pathlib, sys
so_path = pathlib.Path(sys.argv[1])
prefix = sys.argv[2].encode()
data = so_path.read_bytes()
target_rel = b"rstream-libs/webkit2gtk-4.1"
for sub in (b"/libexec/webkit2gtk-4.1", b"/lib/webkit2gtk-4.1"):
    old = prefix + sub
    if old in data and len(old) >= len(target_rel) + 2:
        pad = len(old) - len(target_rel) - 1
        new = b"." + (b"/" * pad) + target_rel
        assert len(new) == len(old)
        data = data.replace(old, new)
so_path.write_bytes(data)
PY
  fi

  patchelf --set-rpath '$ORIGIN/../rstream-libs' "$appdir/usr/bin/rstream.bin"
  for f in "$privlib/webkit2gtk-4.1/"*.bin; do
    [ -f "$f" ] && patchelf --set-rpath '$ORIGIN/..' "$f"
  done
  for f in "$privlib/webkit2gtk-4.1/injected-bundle/"*.so "$privlib/gio/modules/"*.so "$privlib/gdk-pixbuf-2.0/loaders/"*.so; do
    [ -f "$f" ] && patchelf --set-rpath '$ORIGIN/../..:$ORIGIN/..' "$f" || true
  done
  for f in "$privlib/"*.so*; do
    [ -f "$f" ] || continue
    base="$(basename "$f")"
    case "$base" in
      ld-linux-*.so*|libc.so*|libm.so*|libdl.so*|libpthread.so*|librt.so*|libresolv.so*)
        continue
        ;;
    esac
    patchelf --set-rpath '$ORIGIN' "$f" 2>/dev/null || true
  done

  cat > "$appdir/usr/bin/rstream" <<EOF
#!/bin/sh
HERE="\$(CDPATH= cd -- "\$(dirname -- "\$0")/.." && pwd)"
lib_path="\$HERE/rstream-libs"
if [ -d /run/opengl-driver/lib ]; then
  lib_path="\$lib_path:/run/opengl-driver/lib"
fi
exec "\$HERE/rstream-libs/$ld_so_name" --library-path "\$lib_path" "\$HERE/bin/rstream.bin" "\$@"
EOF
  chmod 0755 "$appdir/usr/bin/rstream"
else
  linuxdeploy_bin="linuxdeploy-${arch}.AppImage"
  fetch_tool "$linuxdeploy_bin" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/${linuxdeploy_bin}"
  fetch_tool "linuxdeploy-plugin-gtk.sh" \
    "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh"

  cp "$bin_path" "$appdir/usr/bin/rstream"
  chmod 0755 "$appdir/usr/bin/rstream"

  webkit_libdir="$(pkg-config --variable=libdir webkit2gtk-4.1 2>/dev/null || echo "/usr/lib/${gnu_triple}")"

  extra_libs=()
  [ -n "$appind_so" ] && [ -f "$appind_so" ] && extra_libs+=(--library "$appind_so")
  [ -n "$dbusmenu_so" ] && [ -f "$dbusmenu_so" ] && extra_libs+=(--library "$dbusmenu_so")
  for gmod in libgiognutls.so libgioopenssl.so libgioenvironmentproxy.so; do
    if [ -f "$webkit_libdir/gio/modules/$gmod" ]; then
      extra_libs+=(--library "$webkit_libdir/gio/modules/$gmod")
    fi
  done

  export PATH="$cache_dir:$PATH"
  export APPIMAGE_EXTRACT_AND_RUN=1
  export DEPLOY_GTK_VERSION=3
  export NO_STRIP=true

  "$cache_dir/$linuxdeploy_bin" \
    --appdir "$appdir" \
    --executable "$appdir/usr/bin/rstream" \
    --desktop-file "$desktop_file" \
    --icon-file "$icon_512" \
    --plugin gtk \
    "${extra_libs[@]}"

  for wk_sub in "webkit2gtk-4.1" "${gnu_triple}/webkit2gtk-4.1"; do
    for base in "$webkit_libdir" "/usr/lib" "/usr/libexec"; do
      if [ -d "$base/$wk_sub" ] && [ -f "$base/$wk_sub/WebKitWebProcess" ]; then
        mkdir -p "$privlib/webkit2gtk-4.1"
        cp -a "$base/$wk_sub/." "$privlib/webkit2gtk-4.1/"
        for proc in WebKitWebProcess WebKitNetworkProcess WebKitGPUProcess; do
          if [ -f "$privlib/webkit2gtk-4.1/$proc" ] && command -v patchelf >/dev/null 2>&1; then
            patchelf --set-rpath '$ORIGIN/../../lib:$ORIGIN/..' "$privlib/webkit2gtk-4.1/$proc" || true
          fi
        done
        break 2
      fi
    done
  done

  # Relocate release WebKitGTK's compiled-in /usr/lib/<gnu_triple>/webkit2gtk-4.1 path (ENABLE_DEVELOPER_MODE=OFF
  # ignores WEBKIT_EXEC_PATH) to ./lib/<gnu_triple>/webkit2gtk-4.1 relative to $HERE/usr.
  mkdir -p "$appdir/usr/lib/${gnu_triple}"
  ln -sfn "../../rstream-libs/webkit2gtk-4.1" "$appdir/usr/lib/${gnu_triple}/webkit2gtk-4.1"
  for wk_so in "$appdir/usr/lib/libwebkit2gtk-4.1.so"*; do
    [ -f "$wk_so" ] && [ ! -L "$wk_so" ] || continue
    python3 - "$wk_so" "$gnu_triple" <<'PY'
import pathlib, sys
so_path = pathlib.Path(sys.argv[1])
triple = sys.argv[2].encode()
data = so_path.read_bytes()
for old, new in [
    (b"/usr/lib/" + triple + b"/webkit2gtk-4.1", b"././/lib/" + triple + b"/webkit2gtk-4.1"),
    (b"/usr/libexec/webkit2gtk-4.1", b"././/libexec/webkit2gtk-4.1"),
]:
    assert len(old) == len(new)
    if old in data:
        data = data.replace(old, new)
so_path.write_bytes(data)
PY
  done
  mkdir -p "$appdir/usr/libexec"
  ln -sfn "../rstream-libs/webkit2gtk-4.1" "$appdir/usr/libexec/webkit2gtk-4.1"

  mkdir -p "$privlib/gio/modules"
  for gmod in libgiognutls.so libgioopenssl.so libgioenvironmentproxy.so; do
    if [ -f "$webkit_libdir/gio/modules/$gmod" ]; then
      cp -Lf "$webkit_libdir/gio/modules/$gmod" "$privlib/gio/modules/$gmod"
      patchelf --set-rpath '$ORIGIN/../../lib:$ORIGIN/..' "$privlib/gio/modules/$gmod" 2>/dev/null || true
    fi
  done
fi

# Copy Go c-shared librclone.so AFTER patchelf/linuxdeploy so Go ELF sections stay untouched.
cp "$rclone_so" "$privlib/librclone.so"
chmod 0755 "$privlib/librclone.so"

rm -f "$appdir/AppRun"
cat > "$appdir/AppRun" <<EOF
#!/bin/sh
set -e
HERE="\$(CDPATH= cd -- "\$(dirname -- "\$0")" && pwd)"

# Strip any AppImage wrapper LD_LIBRARY_PATH pointing inside \$HERE so child host tools (/bin/sh, xdg-open) stay clean.
if [ -n "\${LD_LIBRARY_PATH:-}" ]; then
  clean_ld=""
  OLD_IFS="\$IFS"
  IFS=':'
  for p in \$LD_LIBRARY_PATH; do
    case "\$p" in
      "\$HERE"*|"") ;;
      *) clean_ld="\${clean_ld:+\$clean_ld:}\$p" ;;
    esac
  done
  IFS="\$OLD_IFS"
  if [ -n "\$clean_ld" ]; then
    export LD_LIBRARY_PATH="\$clean_ld"
  else
    unset LD_LIBRARY_PATH
  fi
fi

if [ ! -f "\$HERE/usr/rstream-libs/$ld_so_name" ] && [ -d "\$HERE/usr/lib" ]; then
  ld_path="\$HERE/usr/lib"
  if [ -d /run/opengl-driver/lib ]; then
    ld_path="\$ld_path:/run/opengl-driver/lib"
  fi
  export LD_LIBRARY_PATH="\$ld_path\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
fi

if [ -d /run/opengl-driver/share/glvnd/egl_vendor.d ] && [ -z "\${__EGL_VENDOR_LIBRARY_DIRS:-}" ]; then
  export __EGL_VENDOR_LIBRARY_DIRS="/run/opengl-driver/share/glvnd/egl_vendor.d:/usr/share/glvnd/egl_vendor.d"
fi
if [ -d /run/opengl-driver/share/vulkan/icd.d ] && [ -z "\${VK_ADD_DRIVER_FILES:-}" ]; then
  export VK_ADD_DRIVER_FILES="/run/opengl-driver/share/vulkan/icd.d"
fi
if [ -d /run/opengl-driver/lib/dri ] && [ -z "\${LIBVA_DRIVERS_PATH:-}" ]; then
  export LIBVA_DRIVERS_PATH="/run/opengl-driver/lib/dri"
fi

export RSTREAM_LIBRCLONE_PATH="\$HERE/usr/rstream-libs/librclone.so"
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1

if [ -d "\$HERE/usr/rstream-libs/webkit2gtk-4.1" ]; then
  export WEBKIT_EXEC_PATH="\$HERE/usr/rstream-libs/webkit2gtk-4.1"
  if [ -d "\$HERE/usr/rstream-libs/webkit2gtk-4.1/injected-bundle" ]; then
    export WEBKIT_INJECTED_BUNDLE_PATH="\$HERE/usr/rstream-libs/webkit2gtk-4.1/injected-bundle"
  fi
fi

# Isolate GIO modules so bundled GLib never dlopens incompatible host modules (e.g. NixOS gvfs).
unset GIO_EXTRA_MODULES
if [ -d "\$HERE/usr/rstream-libs/gio/modules" ]; then
  export GIO_MODULE_DIR="\$HERE/usr/rstream-libs/gio/modules"
fi

if [ -d "\$HERE/usr/share/glib-2.0/schemas" ]; then
  export GSETTINGS_SCHEMA_DIR="\$HERE/usr/share/glib-2.0/schemas\${GSETTINGS_SCHEMA_DIR:+:\$GSETTINGS_SCHEMA_DIR}"
fi

if [ -f /etc/fonts/fonts.conf ] && [ -z "\${FONTCONFIG_FILE:-}" ]; then
  export FONTCONFIG_FILE=/etc/fonts/fonts.conf
fi
if [ -d /etc/fonts ] && [ -z "\${FONTCONFIG_PATH:-}" ]; then
  export FONTCONFIG_PATH=/etc/fonts
fi

if [ -f "\$HERE/usr/rstream-libs/gdk-pixbuf-2.0/loaders.cache.in" ]; then
  pixbuf_cache="\${XDG_RUNTIME_DIR:-/tmp}/rstream-pixbuf-\$\$.cache"
  escaped_here="\$(printf '%s\n' "\$HERE" | sed 's/[\\\\&|]/\\\\&/g')"
  sed "s|@APPDIR@|\$escaped_here|g" "\$HERE/usr/rstream-libs/gdk-pixbuf-2.0/loaders.cache.in" > "\$pixbuf_cache"
  export GDK_PIXBUF_MODULE_FILE="\$pixbuf_cache"
elif [ -d "\$HERE/usr/lib/gdk-pixbuf-2.0" ]; then
  cache_file="\$(find "\$HERE/usr/lib/gdk-pixbuf-2.0" -name 'loaders.cache' | head -n 1 || true)"
  if [ -n "\$cache_file" ]; then
    export GDK_PIXBUF_MODULE_FILE="\$cache_file"
  fi
fi

export RSTREAM_LAUNCH_CWD="\$PWD"
cd "\$HERE/usr"
exec "\$HERE/usr/bin/rstream" "\$@"
EOF
chmod 0755 "$appdir/AppRun"

out_file="$dist_dir/RStream-${version}-${arch}.AppImage"
rm -f "$out_file"
if command -v appimage-run >/dev/null 2>&1; then
  env -u LD_LIBRARY_PATH ARCH="$arch" VERSION="$version" APPIMAGE_EXTRACT_AND_RUN=1 \
    appimage-run "$cache_dir/$appimagetool_bin" "$appdir" "$out_file"
else
  env -u LD_LIBRARY_PATH ARCH="$arch" VERSION="$version" APPIMAGE_EXTRACT_AND_RUN=1 \
    "$cache_dir/$appimagetool_bin" "$appdir" "$out_file"
fi
chmod 0755 "$out_file"
echo "Built Linux AppImage: $out_file ($(du -h "$out_file" | cut -f1))"
