# x11-mcp

`x11-mcp` is a display-scoped MCP stdio server for observing and controlling an X11 desktop. It speaks the X11 protocol directly through `x11rb`; it does not shell out to `xdotool`, `wmctrl`, or screenshot utilities.

The server is intended for isolated agent desktops such as Xvfb or Xephyr. An X11 client can observe every window on a display and synthesize input, so a dedicated display is the meaningful security boundary.

## Features

- Full-desktop, visible-window, and region PNG observations with window and pointer metadata.
- EWMH window discovery, stable session window references, focus, move, resize, minimize, maximize, restore, and close.
- XTEST pointer movement, clicks, drags, scrolling, key chords, and text entry.
- Unicode text insertion through X11 clipboard ownership with best-effort restoration.
- Polling-based change/idle/window/focus waits and settled observations after actions.
- Window-class action allowlists, input rate limiting, host-display refusal, and a latched emergency stop.
- MCP 2025-11-25-compatible structured tool output plus PNG image content over stdio.

MIT-SHM capture, XDamage-driven settling, Composite off-screen capture, cursor compositing, AT-SPI, HTTP transport, and composite action batches are deliberately deferred.

## Install on NixOS

For a flake-based NixOS configuration, add `x11-mcp` to your inputs and pass the inputs to your NixOS modules:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    x11-mcp = {
      url = "github:Souheab/x11-mcp";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs@{ nixpkgs, ... }: {
    nixosConfigurations.your-hostname = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      specialArgs = { inherit inputs; };
      modules = [ ./configuration.nix ];
    };
  };
}
```

Then add the package to `configuration.nix`:

```nix
{ inputs, pkgs, ... }:

{
  environment.systemPackages = [
    inputs.x11-mcp.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
```

Replace `your-hostname` with the name of your NixOS configuration, then apply it:

```console
sudo nixos-rebuild switch --flake .#your-hostname
x11-mcp --help
```

The package supports `x86_64-linux` and `aarch64-linux`. Installing it makes the server binary available system-wide; an X11 display still needs to be configured when the server is launched, as described below.

## Build and development

```console
nix build
nix develop
cargo test
cargo clippy --workspace --all-targets -- -D warnings
nix flake check
```

The flake exports `packages.default`, `apps.default`, `devShells.default`, and package checks on `x86_64-linux` and `aarch64-linux`. Its test phase starts an isolated 800x600 Xvfb display and Openbox session.

## Start an isolated display

```console
Xvfb :20 -screen 0 1920x1080x24 -nolisten tcp &
DISPLAY=:20 openbox &
nix run . -- --display :20
```

For a nested visible desktop, use Xephyr instead:

```console
Xephyr :20 -screen 1280x800 -nolisten tcp &
DISPLAY=:20 openbox &
nix run . -- --display :20
```

`--display` is always required. If the requested display is the inherited `DISPLAY`, or is `:0` when there is no inherited display, startup fails unless `--allow-host-display` is explicitly supplied.

Use normal Xauthority credentials. `--xauthority /path/to/Xauthority` overrides `XAUTHORITY`; otherwise `x11rb` performs standard Xauthority lookup. Never use `xhost +`.

## MCP client configuration

Configure an MCP client to launch the binary and pass its arguments separately. A generic configuration for a local checkout looks like:

```json
{
  "mcpServers": {
    "x11": {
      "command": "nix",
      "args": [
        "run",
        "/absolute/path/to/x11-mcp",
        "--",
        "--display",
        ":20"
      ],
      "env": {
        "XAUTHORITY": "/path/to/Xauthority"
      }
    }
  }
}
```

All logs go to stderr. Stdout contains only MCP protocol messages.

## Tools

- `x11.get_capabilities`
- `x11.observe`
- `x11.list_windows`
- `x11.focus_window`
- `x11.move_pointer`
- `x11.click`
- `x11.drag`
- `x11.scroll`
- `x11.key`
- `x11.type_text`
- `x11.window_action`
- `x11.wait_for`

Positions use a tagged `coordinate_space` of `screen`, `window`, or `window_relative`. Window-relative coordinates are normalized from `0.0` through `1.0` and are resolved immediately before the action.

Mutating tools accept `observe_after` where applicable:

```json
{
  "position": {
    "coordinate_space": "screen",
    "x": 400,
    "y": 300
  },
  "button": 1,
  "count": 1,
  "observe_after": {
    "quiet_ms": 150,
    "timeout_ms": 3000,
    "require_change": false
  }
}
```

An action is never reported as failed merely because settling timed out. Its result contains the latest observation with `settled: false`. Explicit wait timeouts are returned as retryable tool errors.

## Safety controls

Repeat `--allow-window-class 'glob'` to restrict targeted actions. Screen-coordinate actions must resolve to a topmost window whose `WM_CLASS` instance or class matches one of those globs. This does not hide other windows from full-desktop observations.

The default input burst limit is 200 XTEST events per second. Override it with `--max-input-events-per-second`.

Send `SIGUSR1` to latch the emergency stop:

```console
kill -USR1 "$(pgrep -n x11-mcp)"
```

Observation tools remain available, but all subsequent mutations fail until the process is restarted. SIGINT or terminating the MCP client shuts the server down and releases any held synthetic keys or buttons.

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at your option.
