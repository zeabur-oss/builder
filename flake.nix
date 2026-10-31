{
  description = "zeabur-builder development environment";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1"; # unstable Nixpkgs
    # Rust toolchains. The locked revision sets the nightly (overlays.default).
    rust-overlay = {
      url = "https://flakehub.com/f/oxalica/rust-overlay/0.1";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # zeabur-oss-release: pushes, signs and verifies a release (scripts/release.ts).
    oss-release = {
      url = "github:zeabur-oss/.github";
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
      overlays.default = final: _: {
        # The newest nightly in the locked rust-overlay that has all these components and targets.
        rustToolchain = final.rust-bin.selectLatestNightlyWith (
          toolchain:
          toolchain.minimal.override {
            extensions = [
              "rustfmt"
              "clippy"
              "rust-src"
              "rust-analyzer"
            ];
            targets = [
              "x86_64-unknown-linux-musl"
              "aarch64-unknown-linux-musl"
            ];
          }
        );
      };

      devShells = forEachSupportedSystem (
        { pkgs, system }:
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain

              # Quality and supply chain
              cargo-auditable
              cargo-deny
              cargo-nextest
              jq
              shellcheck
              actionlint

              # Kubernetes and BuildKit integration tests
              kind
              k3d
              kubectl
              kubernetes-helm
              buildkit
              openssl

              # Releases
              inputs.oss-release.packages.${system}.zeabur-oss-release
              crane
              gh
              bun

              self.formatter.${system}
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
