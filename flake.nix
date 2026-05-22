{
  description = "sftp-s3-rs - Pluggable SFTP server with S3 and custom backend support";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {
        inherit system overlays;
      };

      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        extensions = ["rust-src" "rust-analyzer" "llvm-tools-preview"];
      };

      # Nightly toolchain for tools that require it (cargo-udeps)
      rustNightlyForUdeps = pkgs.rust-bin.nightly.latest.default;
      openssh = pkgs.openssh;

      # Wrapper for cargo-udeps that uses nightly
      cargo-udeps-wrapped = pkgs.writeShellScriptBin "cargo-udeps" ''
        export RUSTC="${rustNightlyForUdeps}/bin/rustc"
        export CARGO="${rustNightlyForUdeps}/bin/cargo"
        exec ${pkgs.cargo-udeps}/bin/cargo-udeps "$@"
      '';

      nativeBuildInputs = with pkgs;
        [
          rustToolchain
          pkg-config

          # Code quality & linting
          cargo-deny
          cargo-audit

          # Testing & coverage
          cargo-nextest
          cargo-mutants

          # Build & dependencies
          cargo-outdated
          cargo-bloat
          cargo-udeps-wrapped

          # Utilities
          tokei
          gh
          netcat

          # Performance profiling
          cargo-flamegraph

          # Benchmarking
          gnuplot
          hyperfine

          # SFTP testing
          sshpass
          lsof
          bc
          openssh  # sftp client
          minio

          # Build optimization
          sccache
        ]
        ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
          perf
          bpftrace
          inferno
          cargo-llvm-cov
          valgrind  # Required for IAI benchmarks
          heaptrack
          mold
        ];

      buildInputs = with pkgs; [
        openssl
      ];

      cargoTargetEnvPrefix = pkgs.lib.toUpper (builtins.replaceStrings ["-"] ["_"] pkgs.rust.toRustTargetSpec pkgs.stdenv.hostPlatform);
    in {
      devShells.default = pkgs.mkShell {
        inherit nativeBuildInputs buildInputs;

        shellHook = ''
          export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
          export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig"
          export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
          export "CARGO_TARGET_''${cargoTargetEnvPrefix}_LINKER"="${pkgs.lib.optionalString pkgs.stdenv.isLinux "${pkgs.mold}/bin/mold -run "}${pkgs.stdenv.cc}/bin/cc"
          export "CARGO_TARGET_''${cargoTargetEnvPrefix}_RUSTFLAGS"="-C target-cpu=native"

          echo "sftp-s3-rs development environment"
          echo "  Rust: $(rustc --version)"
          echo "  OpenSSH: $(${openssh}/bin/ssh -V 2>&1)"
          echo ""
          echo "Commands:"
          echo "  cargo build          - Build the project"
          echo "  cargo test           - Run tests"
          echo "  cargo nextest run    - Fast parallel tests"
          echo "  cargo clippy         - Run linter"
          echo "  cargo fmt            - Format code"
          echo ""
          echo "Performance:"
          echo "  cargo flamegraph     - Generate CPU flamegraph"
          echo "  cargo bench          - Run benchmarks"
          ${pkgs.lib.optionalString pkgs.stdenv.isLinux ''
          echo "  perf                 - Linux perf tools"
          echo "  heaptrack            - Heap allocation profiler"
          ''}
          echo ""
          echo "Code quality:"
          echo "  cargo deny check     - Check dependencies"
          echo "  cargo audit          - Security audit"
          echo "  cargo outdated       - Check for updates"
          echo "  cargo bloat          - Analyze binary size"
          echo ""
        '';

        GH_PAGER = "cat";

        # OpenSSL configuration
        OPENSSL_DIR = "${pkgs.openssl.dev}";
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
      };

      packages.default = pkgs.rustPlatform.buildRustPackage {
        pname = "sftp-s3";
        version = "0.1.0";
        src = ./.;

        cargoLock = {
          lockFile = ./Cargo.lock;
        };

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        buildInputs = with pkgs; [
          openssl
        ];

        OPENSSL_DIR = "${pkgs.openssl.dev}";
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";

        meta = with pkgs.lib; {
          description = "Pluggable SFTP server with S3 and custom backend support";
          license = licenses.asl20;
          platforms = platforms.all;
        };
      };
    });
}
