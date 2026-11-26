{
  description = "zeabur-builder development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    # Rust toolchains. rust-toolchain.toml sets the nightly; the locked revision must have its manifest.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # The base shell of every zeabur-oss repository: Bun, the release tools and the shared linters.
    oss = {
      url = "github:zeabur-oss/.github/395adf27d31c77538ebf7c154946b52822387b96";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, ... }@inputs:

    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forEachSupportedSystem =
        f:
        inputs.nixpkgs.lib.genAttrs supportedSystems (
          system:
          f {
            inherit system;
            pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [
                inputs.rust-overlay.overlays.default
                inputs.self.overlays.default
              ];
            };
          }
        );
    in
    {
      overlays.default = final: prev: {
        # The nightly, components and targets of rust-toolchain.toml, the same file rustup reads.
        rustToolchain = final.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      };

      devShells = forEachSupportedSystem (
        { pkgs, system }:
        {
          default = pkgs.mkShell {
            inputsFrom = [ inputs.oss.devShells.${system}.base ];
            packages = with pkgs; [
              rustToolchain

              # Quality and supply chain
              cargo-auditable
              cargo-deny
              cargo-nextest
              # Pattern rules (scripts/ci/patterns.sh)
              ripgrep

              # Kubernetes and BuildKit integration tests
              kind
              k3d
              kubectl
              buildkit
              openssl
            ];

            env = {
              # Required by rust-analyzer
              RUST_SRC_PATH = "${pkgs.rustToolchain}/lib/rustlib/src/rust/library";
              # Mozilla's CA bundle (MPL-2.0), copied into the controller image for HTTPS to Git
              # servers; pinned with nixpkgs.
              ZEABUR_BUILDER_CA_BUNDLE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
            }
            // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
              # Static musl builds of the host architecture: aws-lc-sys must be compiled against musl
              # headers.
              let
                muslCc = pkgs.pkgsStatic.stdenv.cc;
                bin = "${muslCc}/bin/${muslCc.targetPrefix}";
                triple = "${pkgs.stdenv.hostPlatform.parsed.cpu.name}_unknown_linux_musl";
              in
              {
                "CC_${triple}" = "${bin}cc";
                "AR_${triple}" = "${bin}ar";
                "CARGO_TARGET_${pkgs.lib.toUpper triple}_LINKER" = "${bin}cc";
              }
            );
          };
        }
      );

      formatter = forEachSupportedSystem ({ pkgs, ... }: pkgs.nixfmt);
    };
}
