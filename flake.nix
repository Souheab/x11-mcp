{
  description = "Display-scoped X11 controller exposed as an MCP stdio server";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          package = pkgs.rustPlatform.buildRustPackage {
            pname = "x11-mcp";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeCheckInputs = [ pkgs.xorg-server pkgs.openbox ];
            preCheck = ''
              export DISPLAY=:99
              export X11_MCP_RUN_X11_TESTS=1
              Xvfb :99 -screen 0 800x600x24 -nolisten tcp &
              xvfb_pid=$!
              openbox >/dev/null 2>&1 &
              openbox_pid=$!
              trap 'kill "$openbox_pid" "$xvfb_pid" 2>/dev/null || true' EXIT
              sleep 1
            '';
            meta = {
              description = "Native X11 control server for Model Context Protocol clients";
              license = with pkgs.lib.licenses; [ mit asl20 ];
              mainProgram = "x11-mcp";
              platforms = pkgs.lib.platforms.linux;
            };
          };
        in {
          default = package;
          x11-mcp = package;
        });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/x11-mcp";
          meta.description = "Run the x11-mcp stdio server";
        };
      });

      checks = forAllSystems (system: {
        package = self.packages.${system}.default;
      });

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              openbox
              rust-analyzer
              rustc
              rustfmt
              xauth
              xorg-server
              xterm
            ];
            shellHook = ''
              echo "x11-mcp development shell"
              echo "Run: cargo test && cargo clippy --workspace --all-targets -- -D warnings"
            '';
          };
        });
    };
}
