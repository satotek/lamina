{
  description = "Strata — GPUI performance validation (phase 0)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (
        pkgs:
        let
          # Parallel agents build in separate worktrees. One shared target
          # directory keeps a single copy of the dependency builds (GPUI is
          # ~600 crates, several GB) instead of one per worktree. Cargo gives a
          # workspace crate the same artifact name in every worktree, though, so
          # a build in one worktree can leave another with a stale
          # strata-core; touch its sources and rebuild when errors contradict
          # the code.
          sharedTarget = ''
            export CARGO_TARGET_DIR="''${XDG_CACHE_HOME:-$HOME/.cache}/strata/target"
          '';

          # GPUI's Linux backend (wayland / x11 / wgpu) loads most of these at
          # runtime through dlopen, so they have to be on LD_LIBRARY_PATH as
          # well as available to pkg-config at build time.
          runtimeLibs =
            with pkgs;
            [
              alsa-lib
              fontconfig
              freetype
              libGL
              libX11
              libXcursor
              libXi
              libXrandr
              libXrender
              libxcb
              libxkbcommon
              openssl
              stdenv.cc.cc.lib
              vulkan-loader
              wayland
            ]
            # Mesa's lavapipe gives the headless smoke test a software Vulkan
            # device; without an ICD file wgpu finds no adapter at all.
            ++ lib.optionals pkgs.stdenv.hostPlatform.isx86_64 [ pkgs.mesa ];

          # GPUI's macOS backend links the system frameworks and compiles its
          # Metal shaders with `xcrun metal`, which only Xcode provides. Nix's
          # compiler wrapper and SDK would shadow both, so this shell adds the
          # Rust toolchain alone and leaves cc, the SDK and xcrun to Xcode.
          darwinShell = pkgs.mkShellNoCC {
            packages = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
            ];
            RUST_BACKTRACE = "1";
            shellHook = sharedTarget;
          };

          linuxShell = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              pkg-config
              rustc
              rustfmt
              xvfb
            ];

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;

            # Bare `-lxcb`/`-lxkbcommon` flags come from pkg-config; `cc` needs
            # the same paths for linking.
            LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;

            # `mkShell` does not run the pkg-config setup hook for `packages`,
            # and GPUI's font stack probes fontconfig through pkg-config.
            PKG_CONFIG_PATH = pkgs.lib.makeSearchPathOutput "dev" "lib/pkgconfig" (
              with pkgs;
              [
                alsa-lib
                fontconfig
                freetype
                libX11
                libXcursor
                libXi
                libXrandr
                libxcb
                libxkbcommon
                openssl
                vulkan-loader
                wayland
              ]
            );

            # libxkbcommon ships no compiled keymap tables; point it at the
            # X11 data instead of the (absent) /usr/share/X11/xkb.
            XKB_CONFIG_ROOT = "${pkgs.xkeyboard_config}/etc/X11/xkb";

            # GPUI resolves font families through fontconfig. Nix has no
            # /usr/share/fonts, so without this every text run falls back to a
            # missing ".SystemUIFont" and the window renders blank.
            FONTCONFIG_FILE = pkgs.makeFontsConf {
              fontDirectories = [
                pkgs.dejavu_fonts
                pkgs.liberation_ttf
              ];
            };

            VK_ICD_FILENAMES = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isx86_64 (
              "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json"
            );

            RUST_BACKTRACE = "1";

            shellHook = sharedTarget;
          };
        in
        {
          default = if pkgs.stdenv.hostPlatform.isDarwin then darwinShell else linuxShell;
        }
      );
    };
}
