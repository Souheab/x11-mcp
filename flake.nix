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
          atspiForTests = pkgs.at-spi2-core.overrideAttrs (old: {
            mesonFlags =
              builtins.filter
                (flag: !(pkgs.lib.hasPrefix "-Ddbus_daemon=" flag))
                old.mesonFlags
              ++ [ "-Ddbus_daemon=${pkgs.dbus}/bin/dbus-daemon" ];
          });
          package = pkgs.rustPlatform.buildRustPackage {
            pname = "x11-mcp";
            version = "0.3.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeCheckInputs = [
              atspiForTests
              pkgs.dbus
              pkgs.openbox
              pkgs.xorg-server
              pkgs.zenity
            ];
            checkPhase = ''
              runHook preCheck
              dbus-run-session \
                --config-file=${pkgs.dbus}/share/dbus-1/session.conf \
                -- bash -euo pipefail -c '
                export ATSPI_DBUS_IMPLEMENTATION=dbus-daemon
                export DISPLAY=:99
                export GTK_A11Y=atspi
                unset AT_SPI_BUS_ADDRESS
                export NO_AT_BRIDGE=0
                export X11_MCP_RUN_ATSPI_TESTS=1
                export X11_MCP_RUN_X11_TESTS=1
                mkdir -p /tmp/.X11-unix
                chmod 1777 /tmp/.X11-unix
                Xvfb :99 -screen 0 800x600x24 -nolisten tcp &
                xvfb_pid=$!
                sleep 0.5
                DISPLAY=:99 openbox >/dev/null 2>&1 &
                openbox_pid=$!
                ${atspiForTests}/libexec/at-spi-bus-launcher --launch-immediately --a11y=1 --screen-reader=1 &
                atspi_pid=$!
                trap "kill $atspi_pid $openbox_pid $xvfb_pid 2>/dev/null || true" EXIT
                sleep 1
                cargo test --workspace -- --test-threads=1
              '
              runHook postCheck
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
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              at-spi2-core
              cargo
              clippy
              dbus
              openbox
              rust-analyzer
              rustc
              rustfmt
              xauth
              xorg-server
              xterm
              zenity
            ];
            shellHook = ''
              echo "x11-mcp development shell"
              echo "Run: cargo test && cargo clippy --workspace --all-targets -- -D warnings"
            '';
          };
        });
    };
}
