{
  description = "Build environment for libkrun and krunwipe, incl. aarch64 cross-compilation";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      # Distro rustc ships only the host's std. The aarch64 targets are what
      # this shell exists for: the gnu one for libkrun and krunwipe, the musl
      # one because init-blob's build.rs spawns a nested cargo to build the
      # guest init binary statically. The host musl target is here for the same
      # build.rs, which wants it whenever the build is not cross.
      rust = pkgs.rust-bin.stable.latest.default.override {
        extensions = [ "clippy" "rustfmt" "rust-src" ];
        targets = [
          "aarch64-unknown-linux-gnu"
          "aarch64-unknown-linux-musl"
          "x86_64-unknown-linux-musl"
        ];
      };

      crossCC = pkgs.pkgsCross.aarch64-multiplatform.stdenv.cc;
      crossPrefix = "aarch64-unknown-linux-gnu";
      # The cross cc-wrapper only carries the shared glibc, so a -static link
      # has nothing to resolve -lc against. guest-probe needs one.
      crossStaticLibs = "${pkgs.pkgsCross.aarch64-multiplatform.glibc.static}/lib";
    in {
      devShells.${system}.default = pkgs.mkShell {
        # crossCC is deliberately not in packages: its setup hook exports a
        # global CC/CXX, which would hand the aarch64 compiler to host builds
        # too. It goes on PATH in the shellHook instead, and only the
        # per-target cargo variables below point at it.
        packages = [
          rust
          pkgs.pkg-config
          pkgs.cpio          # guest-probe initramfs
          pkgs.android-tools # adb, to deploy to the target
        ];

        # Only the *_<target>_* variables are set, never a bare CC or LINKER,
        # so a plain `cargo build` in this shell still builds for the host.
        env = {
          CROSS_TARGET = "aarch64-unknown-linux-gnu";
          CROSS_CC = "${crossPrefix}-gcc";
          CROSS_STATIC_LIBS = crossStaticLibs;

          CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER = "${crossPrefix}-gcc";
          CC_aarch64_unknown_linux_gnu = "${crossPrefix}-gcc";
          AR_aarch64_unknown_linux_gnu = "${crossPrefix}-ar";

          # nixpkgs' cross gcc points the ELF interpreter at its own glibc in
          # /nix/store, which does not exist on the target and makes the binary
          # die with a bare "No such file or directory". Point it back at the
          # distro path. The nix RUNPATH entries it also adds are harmless --
          # the loader skips directories that are not there -- and $ORIGIN
          # still comes first, so a libkrun.so sitting next to the binary wins.
          #
          # This is only safe while the nix glibc is newer than the target's:
          # check with `readelf -V <binary> | grep -o 'GLIBC_[0-9.]*' | sort -uV`
          # that nothing newer than the target's glibc is required.
          CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS =
            "-C link-arg=-Wl,--dynamic-linker=/lib/ld-linux-aarch64.so.1";

          # musl links self-contained against rust's own crt objects, so the
          # gnu cross-gcc is only acting as the linker driver here. That avoids
          # pulling in a second cross toolchain just for the init binary.
          CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER = "${crossPrefix}-gcc";
          CC_aarch64_unknown_linux_musl = "${crossPrefix}-gcc";
          AR_aarch64_unknown_linux_musl = "${crossPrefix}-ar";
          CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "-C link-self-contained=yes";
        };

        shellHook = ''
          export PATH="${crossCC}/bin:$PATH"
          echo "libkrun cross shell: $(rustc --version)"
          echo "  host build:   cargo build --release -p libkrun"
          echo "  target build: cargo build --release --target \$CROSS_TARGET -p libkrun"
        '';
      };
    };
}
