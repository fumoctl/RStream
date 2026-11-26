{
  description = "RStream development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, fenix }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            config = {
              allowUnfree = true;
              android_sdk.accept_license = true;
            };
          };
        in f system pkgs);
    in
    {
      packages = forAllSystems (system: pkgs:
        let
          abi = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64-v8a" else "x86_64";
          android = pkgs.androidenv.composeAndroidPackages {
            platformVersions = [ "34" "35" "36" ];
            buildToolsVersions = [ "35.0.0" "36.0.0" ];
            includeEmulator = true;
            includeSystemImages = true;
            systemImageTypes = [ "google_apis" ];
            abiVersions = [ abi ];
            includeNDK = true;
            ndkVersions = [ "28.2.13676358" ];
          };
          sdk = android.androidsdk;
          androidSdkRoot = "${sdk}/libexec/android-sdk";
          emuLibPath = pkgs.lib.makeLibraryPath [ pkgs.vulkan-loader pkgs.libGL ];
          android-emulator = pkgs.writeShellApplication {
            name = "android-emulator";
            runtimeInputs = [ sdk pkgs.jdk17 ];
            text = ''
              export ANDROID_HOME="${androidSdkRoot}"
              export ANDROID_SDK_ROOT="${androidSdkRoot}"
              export ANDROID_NDK_HOME="${androidSdkRoot}/ndk/28.2.13676358"
              export JAVA_HOME="${pkgs.jdk17}"
              export ANDROID_AVD_HOME="''${XDG_CACHE_HOME:-$HOME/.cache}/nix-avd/rstream"
              export ANDROID_USER_HOME="$ANDROID_AVD_HOME/user-home"
              export LD_LIBRARY_PATH="${emuLibPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

              mkdir -p "$ANDROID_AVD_HOME" "$ANDROID_USER_HOME"
              if [ ! -f "$ANDROID_AVD_HOME/rstream.ini" ]; then
                avdmanager create avd --force \
                  --name "rstream" \
                  --package "system-images;android-34;google_apis;${abi}" \
                  --device "pixel_6" <<< "no"
                printf 'hw.keyboard=yes\ndisk.dataPartition.size=4096M\n' >> "$ANDROID_AVD_HOME/rstream.avd/config.ini"
              fi

              adb start-server >/dev/null 2>&1 || true

              args=(-avd "rstream" -gpu "auto")
              if [ "''${SIDEMU_ACTIVE:-}" = "1" ]; then
                args+=(-no-window -no-audio -no-boot-anim)
              fi
              exec emulator "''${args[@]}" "$@"
            '';
          };
        in
        {
          inherit android-emulator sdk;
        });

      apps = forAllSystems (system: _: {
        android-emulator = {
          type = "app";
          program = "${self.packages.${system}.android-emulator}/bin/android-emulator";
        };
      });

      devShells = forAllSystems (system: pkgs:
        let
          sdk = self.packages.${system}.sdk;
          androidSdkRoot = "${sdk}/libexec/android-sdk";
          ndkHome = "${androidSdkRoot}/ndk/28.2.13676358";
          ndkBin = "${ndkHome}/toolchains/llvm/prebuilt/linux-x86_64/bin";

          fenixPkgs = fenix.packages.${system};
          rustToolchain = fenixPkgs.combine [
            (fenixPkgs.stable.withComponents [
              "cargo"
              "clippy"
              "rust-src"
              "rustc"
              "rustfmt"
              "rust-std"
            ])
            fenixPkgs.targets.x86_64-linux-android.stable.rust-std
            fenixPkgs.targets.aarch64-linux-android.stable.rust-std
            fenixPkgs.targets.armv7-linux-androideabi.stable.rust-std
          ];

          runtimeLibPath = pkgs.lib.makeLibraryPath [
            pkgs.mpv-unwrapped
            pkgs.wayland
            pkgs.libGL
            pkgs.libglvnd
            pkgs.vulkan-loader
            pkgs.gtk3
            pkgs.webkitgtk_4_1
            pkgs.glib
          ];
        in
        {
          default = pkgs.mkShell {
            packages = [
              rustToolchain
              sdk
              pkgs.jdk17
              pkgs.go
              pkgs.dioxus-cli
              pkgs.pkg-config
              pkgs.clang
              pkgs.lld
              pkgs.llvm
              pkgs.cmake
              pkgs.unzip
              pkgs.zip
              pkgs.curl
              pkgs.jq
              pkgs.sqlite
              pkgs.mpv-unwrapped
              pkgs.webkitgtk_4_1
              pkgs.gtk3
              pkgs.glib-networking
              pkgs.libsoup_3
              pkgs.xdotool
              pkgs.libayatana-appindicator
              pkgs.wayland
              pkgs.libGL
              pkgs.libglvnd
              pkgs.vulkan-loader
              pkgs.xvfb-run
              pkgs.imagemagick
              pkgs.adw-gtk3
              pkgs.dconf
            ];

            ANDROID_HOME = androidSdkRoot;
            ANDROID_SDK_ROOT = androidSdkRoot;
            ANDROID_NDK_HOME = ndkHome;
            JAVA_HOME = "${pkgs.jdk17}";
            ORG_GRADLE_PROJECT_android_aapt2FromMavenOverride = "${androidSdkRoot}/build-tools/36.0.0/aapt2";
            GIO_EXTRA_MODULES = "${pkgs.glib-networking}/lib/gio/modules:${pkgs.dconf.lib}/lib/gio/modules";
            WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS = "1";

            CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER = "${ndkBin}/x86_64-linux-android26-clang";
            CC_x86_64_linux_android = "${ndkBin}/x86_64-linux-android26-clang";
            CXX_x86_64_linux_android = "${ndkBin}/x86_64-linux-android26-clang++";
            AR_x86_64_linux_android = "${ndkBin}/llvm-ar";

            CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = "${ndkBin}/aarch64-linux-android26-clang";
            CC_aarch64_linux_android = "${ndkBin}/aarch64-linux-android26-clang";
            CXX_aarch64_linux_android = "${ndkBin}/aarch64-linux-android26-clang++";
            AR_aarch64_linux_android = "${ndkBin}/llvm-ar";

            CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER = "${ndkBin}/armv7a-linux-androideabi26-clang";
            CC_armv7_linux_androideabi = "${ndkBin}/armv7a-linux-androideabi26-clang";
            CXX_armv7_linux_androideabi = "${ndkBin}/armv7a-linux-androideabi26-clang++";
            AR_armv7_linux_androideabi = "${ndkBin}/llvm-ar";

            shellHook = ''
              _rstream_ld="${runtimeLibPath}"
              if [ -d /run/opengl-driver/lib ]; then
                _rstream_ld="/run/opengl-driver/lib:$_rstream_ld"
              fi
              export LD_LIBRARY_PATH="$_rstream_ld''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              unset _rstream_ld
              if [ -d /run/opengl-driver/lib/dri ]; then
                export LIBVA_DRIVERS_PATH="/run/opengl-driver/lib/dri"
              fi
              if [ -z "''${GTK_DATA_PREFIX:-}" ]; then
                if [ -d "/etc/profiles/per-user/$USER/share/themes" ]; then
                  export GTK_DATA_PREFIX="/etc/profiles/per-user/$USER"
                elif [ -d "/run/current-system/sw/share/themes" ]; then
                  export GTK_DATA_PREFIX="/run/current-system/sw"
                else
                  export GTK_DATA_PREFIX="${pkgs.adw-gtk3}"
                fi
              fi
            '';
          };
        });
    };
}
