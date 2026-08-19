# x11-mcp

`x11-mcp` is a display-scoped MCP stdio server for observing and controlling an X11 desktop. It speaks X11 directly through `x11rb` and can optionally use AT-SPI for semantic UI inspection and actions. It does not shell out to `xdotool`, `wmctrl`, or screenshot utilities.

The server is intended for isolated agent desktops such as Xvfb or Xephyr. An X11 client can observe every window on a display and synthesize input, so a dedicated display is the meaningful security boundary.

## v0.3 features

- Full-desktop, visible-window, and region PNG observations with window and pointer metadata.
- XDamage-driven change detection, quiet-period settling, and rectangular PNG frame deltas. A 64-frame history protects actions from stale visual state.
- EWMH window discovery, stable session window references, focus, move, resize, minimize, maximize, restore, and close.
- XDG desktop-application discovery with localized names and display-scoped, shell-free launching by stable desktop-entry ID.
- XTEST pointer movement, guarded clicks and drags, scrolling, key chords, and text entry.
- Fail-fast batches of up to 64 application-launch, X11, semantic, and wait steps under one mutation lock and deadline.
- Event-driven frame, window, and optional AT-SPI element waits.
- Optional AT-SPI snapshots and actions with stable element references, generation guards, bounded traversal, text/value support, and X11 window association.
- Independent desktop-application and window-class allowlists, input rate limiting, host-display refusal, and a latched emergency stop.
- MCP 2025-11-25-compatible structured output plus one or more PNG image content blocks over stdio.

OCR, template matching, HTTP transport, XComposite off-screen capture, MIT-SHM, and advanced clipboard operations are outside the v0.3 scope.

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

Apply it and inspect the CLI:

```console
sudo nixos-rebuild switch --flake .#your-hostname
x11-mcp --help
```

The package supports `x86_64-linux` and `aarch64-linux`.

## Build and development

```console
nix develop
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
nix flake check
```

The Nix check starts an isolated D-Bus session, 800x600 Xvfb display, Openbox, an AT-SPI bus, and deterministic Zenity applications. It also discovers a temporary desktop entry, launches it through a batch, and waits for its window. The development and check inputs include D-Bus, `at-spi2-core`, and Zenity.

## Start an isolated display

```console
Xvfb :20 -screen 0 1920x1080x24 -nolisten tcp &
DISPLAY=:20 openbox &
DISPLAY=:20 x11-mcp --display :20
```

For semantic control, run the server and applications in a desktop session with an AT-SPI bus. `--accessibility` accepts:

- `auto` (default): startup succeeds without AT-SPI, capabilities report `available: false`, and the next semantic request retries once.
- `disabled`: semantic tools return `unsupported_capability` without trying to connect.
- `required`: startup fails unless AT-SPI connects.

`--display` is required. If the requested display is the inherited `DISPLAY`, or is `:0` when there is no inherited display, startup fails unless `--allow-host-display` is explicitly supplied.

Use normal Xauthority credentials. `--xauthority /path/to/Xauthority` overrides `XAUTHORITY`; otherwise `x11rb` performs standard Xauthority lookup. Never use `xhost +`.

## MCP client configuration

A generic configuration for a local checkout is:

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
        ":20",
        "--accessibility",
        "auto"
      ],
      "env": {
        "XAUTHORITY": "/path/to/Xauthority"
      }
    }
  }
}
```

All logs go to stderr. Stdout contains only MCP protocol messages.

During MCP initialization the server supplies clients with an operating playbook. It directs agents
to discover capabilities, applications, and windows first; prefer AT-SPI semantics; guard coordinate
actions with fresh observations; use scoped event-driven waits; interpret delta patches correctly;
batch dependent steps; refresh stale state instead of blindly retrying; and verify every mutation.

## Tools

v0.3 exposes 17 focused tools:

- `x11.get_capabilities`
- `x11.observe`
- `x11.list_windows`
- `x11.list_apps`
- `x11.launch_app`
- `x11.focus_window`
- `x11.move_pointer`
- `x11.click`
- `x11.drag`
- `x11.scroll`
- `x11.key`
- `x11.type_text`
- `x11.window_action`
- `x11.wait_for`
- `x11.batch`
- `x11.accessibility_snapshot`
- `x11.accessibility_action`

The MCP `tools/list` response is the authoritative JSON Schema for every request.

## Desktop applications

`x11.list_apps` discovers visible `Type=Application` desktop entries using XDG data-directory
precedence and the session locale. It honors hidden overrides, `NoDisplay`, `OnlyShowIn`,
`NotShowIn`, `TryExec`, and the active application allowlist. Results are sorted by localized
name and then desktop-entry ID:

```json
{
  "query": "browser"
}
```

```json
{
  "apps": [
    {
      "app_id": "org.mozilla.firefox",
      "name": "Firefox"
    }
  ]
}
```

Launch the selected entry by its exact ID:

```json
{
  "app_id": "org.mozilla.firefox"
}
```

The result contains `launched: true`, the ID and localized name, and the spawned process ID. The
server re-resolves the desktop entry at launch time, expands its standard `Exec` field codes
without file or URL inputs, spawns it directly without a shell, honors `Path`, detaches stdio, and
forces `DISPLAY` to the configured display while inheriting the session and Xauthority
environment. `DBusActivatable` entries deliberately use their `Exec` compatibility path.

Only primary GUI launches are exposed. Terminal entries, desktop actions, caller-supplied
arguments, files, URLs, raw commands, and icons are not available through MCP.

Use a batch when launch and window detection must not interleave with another mutation:

```json
{
  "timeout_ms": 8000,
  "steps": [
    {
      "step": "launch_app",
      "request": { "app_id": "org.mozilla.firefox" }
    },
    {
      "step": "wait_for",
      "request": {
        "condition": {
          "condition": "window_matched",
          "selector": { "class_contains": "firefox" }
        },
        "timeout_ms": 7000,
        "observe": true
      }
    }
  ]
}
```


## Observations and deltas

Observation targets are tagged objects: `desktop`, `window`, or `region`. Full delivery is the default:

```json
{
  "target": { "type": "window", "window_ref": "window-3" },
  "include_windows": true,
  "delivery": { "mode": "full" }
}
```

Request a delta from a prior compatible frame:

```json
{
  "target": { "type": "desktop" },
  "delivery": { "mode": "delta", "since_frame_id": 42 }
}
```

A delta result contains `base_frame_id`, `complete`, and `patches`. Each patch has screen-space `bounds` and an `image_index`, where zero identifies the first MCP image content block, one the second, and so on. No visual change returns an empty patch list and no image blocks.

The server returns a complete full frame when the target bounds or desktop topology changed, more than 16 coalesced patches remain, or patch area reaches 60% of the target. An expired or target-incompatible base returns retryable `stale_frame`. Without XDamage, the fallback compares full-frame signatures and returns either an empty delta or a full frame.

`observe_after` now selects its own target and delivery. For `delivery: "delta"`, the action or batch start frame is the base:

```json
{
  "quiet_ms": 150,
  "timeout_ms": 3000,
  "require_change": true,
  "target": { "type": "region", "x": 100, "y": 100, "width": 640, "height": 480 },
  "include_windows": false,
  "delivery": "delta"
}
```

An action is not reported as failed merely because settling timed out; its result has `settled: false` and the final observation. Explicit waits still return retryable timeout errors.

## State guards

Every mutating request accepts a `guard`:

```json
{
  "frame_id": 42,
  "accessibility_generation": 17,
  "expected_active_window": "window-3"
}
```

A matching `frame_id` is mandatory for clicks, drags, positioned scrolling, and window-coordinate pointer actions. The referenced observation must still be in the 64-frame history, cover the action point, have compatible bounds/topology, and have no intersecting damage. Keyboard-only and direct window-reference operations may omit it.

Semantic element mutations require `accessibility_generation` from the snapshot that produced the `element_ref`. `expected_active_window` is optional for either mutation family. Stale pixels, topology, elements, semantic generations, or focus fail before mutation with a retryable `precondition_failed` or `stale_element` result and current state details where safe.

A guarded click typically follows this pattern:

```json
{
  "position": { "coordinate_space": "screen", "x": 400, "y": 300 },
  "button": 1,
  "count": 1,
  "guard": {
    "frame_id": 42,
    "expected_active_window": "window-3"
  }
}
```

## Batches

`x11.batch` executes 1–64 tagged application-launch, X11, semantic, and wait steps under the same session mutation lock used by standalone mutations. It is fail-fast, has no rollback, respects one 1–60 second deadline, and releases held keys/buttons on every exit. A failure includes the failed zero-based step index and completed step results. A `launch_app` step requires no frame guard; if a later step fails, the application is not rolled back.

```json
{
  "guard": {
    "frame_id": 42,
    "accessibility_generation": 17,
    "expected_active_window": "window-3"
  },
  "timeout_ms": 8000,
  "steps": [
    {
      "step": "click",
      "request": {
        "position": { "coordinate_space": "screen", "x": 400, "y": 300 },
        "button": 1,
        "count": 1
      }
    },
    {
      "step": "wait_for",
      "request": {
        "condition": {
          "condition": "window_state",
          "window_ref": "window-3",
          "title_contains": "Ready"
        },
        "timeout_ms": 4000,
        "observe": false
      }
    }
  ],
  "observe_after": {
    "delivery": "delta",
    "quiet_ms": 150,
    "timeout_ms": 2000
  }
}
```

Step request guards and step-level `observe_after` values are replaced by the one batch guard and final observation.

## Waits

`x11.wait_for` uses the tagged `condition` variants:

- `frame_changed` with optional `since_frame_id` and desktop/window/region target.
- `frame_idle` with `quiet_ms` and target.
- `window_matched` with a window selector.
- `window_state` with mapped, active, and title constraints.
- `window_closed`.
- `element_matched` with an element selector.
- `element_state` with required states, name/text matching, and optional numeric `minimum`/`maximum`.

XDamage and AT-SPI events wake relevant waits. Bounded 50 ms polling is retained only when the corresponding event capability is unavailable.

## Accessibility

A snapshot root can be the desktop, a `window_ref`, or an `element_ref`. The result is a flat node list linked by `element_ref` and `parent_ref`. Nodes include role, name, description, states, screen bounds, associated X11 window when unambiguous, interfaces, actions, optional text, and optional numeric value metadata.

Defaults are depth 8, 500 nodes, and no text. Limits are depth 32, 2,000 nodes, and 4,096 text characters per node. `truncated: true` reports a depth or node limit.

```json
{
  "root": { "type": "window", "window_ref": "window-3" },
  "selector": {
    "role": "push button",
    "name_contains": "Save",
    "states_all": ["enabled"],
    "action": "click"
  },
  "max_depth": 12,
  "max_nodes": 800,
  "include_text": false
}
```

Semantic actions invoke the default or a named action, focus an element, replace editable text, or set a numeric value:

```json
{
  "element_ref": "e27",
  "action": "invoke",
  "name": "click",
  "guard": { "accessibility_generation": 17 }
}
```

X11 windows are associated primarily by process ID and verified by overlapping screen extents. Ambiguous nodes omit `window_ref`. When a window-class allowlist is active, a semantic mutation is denied unless its element resolves unambiguously to an allowed window. Semantic snapshots remain unrestricted, like screenshots.

## v0.2 to v0.3 migration

v0.3 adds application discovery and launching without changing existing action schemas:

- Expect 17 tools instead of 15; add support for `x11.list_apps` and `x11.launch_app`.
- Read `get_capabilities.applications.desktop_entry_launch`,
  `terminal_entries_excluded`, and `allowlist_enabled`.
- Accept `{"step":"launch_app","request":{"app_id":"..."}}` in batches. It uses the shared
  mutation lock, requires no frame guard, and follows existing fail-fast/no-rollback behavior.
- Repeat `--allow-app 'glob'` to restrict both discovery and direct launching by desktop-entry ID.
  An empty application allowlist permits every otherwise-visible GUI entry.
- Treat `access_denied` from a direct launch as an application-policy failure; the window-class
  allowlist remains independent.


## v0.1 to v0.2 migration

v0.2 intentionally permits request/result schema breaks:

- Expect 15 tools instead of 12; add support for batch and semantic tools.
- Replace the old wait enum with the `frame_changed`, `frame_idle`, `window_matched`, `window_state`, `window_closed`, `element_matched`, and `element_state` condition tags.
- Add `guard.frame_id` to every targeted pointer mutation. Re-observe and retry on `precondition_failed` or `stale_frame`.
- Read observation metadata directly from the structured result and consume `patches[*].image_index` instead of assuming exactly one image.
- Set `delivery` explicitly when requesting deltas. Treat `complete: true` as a replacement frame and an empty patch list as no change.
- Extend `observe_after` with `target`, `include_windows`, and `delivery`; omitted fields retain full-desktop/full-frame behavior.
- Pass `--accessibility disabled` if the deployment must never connect to AT-SPI, or `required` if semantic availability is mandatory.

Old click:

```json
{
  "position": { "coordinate_space": "screen", "x": 400, "y": 300 }
}
```

v0.2 click after observing frame 42:

```json
{
  "position": { "coordinate_space": "screen", "x": 400, "y": 300 },
  "guard": { "frame_id": 42 }
}
```

Old wait intent “wait for any visual change” becomes:

```json
{
  "condition": {
    "condition": "frame_changed",
    "since_frame_id": 42,
    "target": { "type": "desktop" }
  },
  "timeout_ms": 3000,
  "observe": true
}
```

## Safety controls

Repeat `--allow-window-class 'glob'` to restrict targeted actions. Screen-coordinate actions must resolve to a topmost window whose `WM_CLASS` instance or class matches one of those globs. This does not hide other windows from observations or semantic snapshots.

Repeat `--allow-app 'glob'` to restrict desktop-application discovery and launching by exact
desktop-entry ID. With no `--allow-app` values, every otherwise-visible launchable GUI entry is
permitted. This policy is independent from the window-class action allowlist.


The default input burst limit is 200 XTEST events per second. Override it with `--max-input-events-per-second`.

Send `SIGUSR1` to latch the emergency stop:

```console
kill -USR1 "$(pgrep -n x11-mcp)"
```

Observation and discovery tools remain available, but all subsequent mutations—including application launches—fail until the process is restarted. SIGINT or terminating the MCP client shuts the server down and releases held synthetic keys or buttons.

## License

Licensed under either the Apache License, Version 2.0 or the MIT License, at your option.
