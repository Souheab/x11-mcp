use std::process::Stdio;

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    time::{Duration, timeout},
};
use x11rb::{
    connection::Connection as _,
    protocol::xproto::{ConnectionExt as _, CreateGCAux, Rectangle},
};

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn initializes_and_serves_tools_over_stdio() {
    if std::env::var_os("X11_MCP_RUN_X11_TESTS").is_none() {
        return;
    }
    let display = std::env::var("DISPLAY").expect("DISPLAY must be set by the test harness");
    let mut child = Command::new(env!("CARGO_BIN_EXE_x11-mcp"))
        .args([
            "--display",
            &display,
            "--allow-host-display",
            "--accessibility",
            "disabled",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch x11-mcp");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));

    let initialized = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "x11-mcp-test", "version": "0.2.0"}
            }
        }),
    )
    .await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "x11-mcp");
    assert_eq!(initialized["result"]["serverInfo"]["version"], "0.2.0");
    let instructions = initialized["result"]["instructions"]
        .as_str()
        .expect("server instructions");
    for required_guidance in [
        "x11.get_capabilities",
        "accessibility_generation",
        "frame_id",
        "expected_active_window",
        "x11.wait_for",
        "x11.batch",
        "complete=true",
        "precondition_failed",
        "confirm the intended postcondition",
    ] {
        assert!(
            instructions.contains(required_guidance),
            "server instructions missing {required_guidance}"
        );
    }
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;

    let tools = exchange(
        &mut stdin,
        &mut stdout,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let tools = tools["result"]["tools"]
        .as_array()
        .expect("tools/list array");
    assert_eq!(tools.len(), 15);
    let names = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for expected in [
        "x11.observe",
        "x11.batch",
        "x11.accessibility_snapshot",
        "x11.accessibility_action",
        "x11.wait_for",
    ] {
        assert!(names.contains(expected), "missing tool {expected}");
    }
    assert!(tools.iter().all(|tool| tool["annotations"].is_object()));
    let schemas = serde_json::to_string(tools).expect("serialize tool schemas");
    assert!(schemas.contains("frame_id"));
    assert!(schemas.contains("accessibility_generation"));
    assert!(schemas.contains("since_frame_id"));

    let capabilities = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "x11.get_capabilities", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(capabilities["result"]["isError"], false);
    assert_eq!(
        capabilities["result"]["structuredContent"]["display"],
        display
    );
    assert_eq!(
        capabilities["result"]["structuredContent"]["accessibility"]["available"],
        false
    );

    let idle = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "x11.wait_for",
                "arguments": {
                    "condition": {"condition": "frame_idle", "quiet_ms": 100},
                    "timeout_ms": 2000,
                    "observe": false
                }
            }
        }),
    )
    .await;
    assert_eq!(idle["result"]["isError"], false);

    let observation = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {"name": "x11.observe", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(observation["result"]["isError"], false);
    assert!(
        observation["result"]["content"]
            .as_array()
            .expect("observation content")
            .iter()
            .any(|content| content["type"] == "image" && content["mimeType"] == "image/png")
    );
    let base_frame_id = observation["result"]["structuredContent"]["frame_id"]
        .as_u64()
        .expect("base frame id");

    let empty_delta = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "x11.observe",
                "arguments": {"delivery": {"mode": "delta", "since_frame_id": base_frame_id}}
            }
        }),
    )
    .await;
    assert_eq!(empty_delta["result"]["isError"], false);
    assert_eq!(
        empty_delta["result"]["structuredContent"]["patches"]
            .as_array()
            .expect("empty patches")
            .len(),
        0
    );

    paint_root_rectangle(&display, 10, 10);
    let first_delta = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "x11.observe",
                "arguments": {"delivery": {"mode": "delta", "since_frame_id": base_frame_id}}
            }
        }),
    )
    .await;
    assert_eq!(first_delta["result"]["isError"], false);
    assert!(
        !first_delta["result"]["structuredContent"]["patches"]
            .as_array()
            .expect("first damage patches")
            .is_empty()
    );
    paint_root_rectangle(&display, 500, 400);
    let multi_delta = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "x11.observe",
                "arguments": {"delivery": {"mode": "delta", "since_frame_id": base_frame_id}}
            }
        }),
    )
    .await;
    assert_eq!(multi_delta["result"]["isError"], false);
    let patches = multi_delta["result"]["structuredContent"]["patches"]
        .as_array()
        .expect("delta patches");
    assert!(
        patches.len() >= 2,
        "expected multiple delta patches: {patches:?}"
    );
    let image_count = multi_delta["result"]["content"]
        .as_array()
        .expect("delta content")
        .iter()
        .filter(|content| content["type"] == "image")
        .count();
    assert_eq!(image_count, patches.len());
    for (index, patch) in patches.iter().enumerate() {
        assert_eq!(patch["image_index"], index);
    }

    let unguarded = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {"name": "x11.click", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(unguarded["result"]["isError"], true);
    assert_eq!(
        unguarded["result"]["structuredContent"]["code"],
        "precondition_failed"
    );

    let batch = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "x11.batch",
                "arguments": {
                    "steps": [
                        {
                            "step": "move_pointer",
                            "request": {"position": {"coordinate_space": "screen", "x": 5, "y": 5}}
                        },
                        {"step": "scroll", "request": {"dx": 0, "dy": 0}}
                    ]
                }
            }
        }),
    )
    .await;
    assert_eq!(batch["result"]["isError"], true);
    assert_eq!(
        batch["result"]["structuredContent"]["details"]["failed_step"],
        1
    );
    assert_eq!(
        batch["result"]["structuredContent"]["details"]["completed_steps"]
            .as_array()
            .expect("completed batch steps")
            .len(),
        1
    );

    let semantic = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {"name": "x11.accessibility_snapshot", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(semantic["result"]["isError"], true);
    assert_eq!(
        semantic["result"]["structuredContent"]["code"],
        "unsupported_capability"
    );

    child.kill().await.expect("stop x11-mcp");
    let _ = child.wait().await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn z_accessibility_modes_and_zenity_semantics() {
    if std::env::var_os("X11_MCP_RUN_ATSPI_TESTS").is_none() {
        return;
    }
    let display = std::env::var("DISPLAY").expect("DISPLAY must be set by the test harness");
    let missing_bus = format!("unix:path=/tmp/x11-mcp-missing-bus-{}", std::process::id());

    let mut auto = Command::new(env!("CARGO_BIN_EXE_x11-mcp"))
        .args([
            "--display",
            &display,
            "--allow-host-display",
            "--accessibility",
            "auto",
        ])
        .env("DBUS_SESSION_BUS_ADDRESS", &missing_bus)
        .env_remove("AT_SPI_BUS_ADDRESS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch auto-mode server without a bus");
    let mut auto_stdin = auto.stdin.take().expect("auto stdin");
    let mut auto_stdout = BufReader::new(auto.stdout.take().expect("auto stdout"));
    initialize(&mut auto_stdin, &mut auto_stdout).await;
    let capabilities = exchange(
        &mut auto_stdin,
        &mut auto_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {"name": "x11.get_capabilities", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(
        capabilities["result"]["structuredContent"]["accessibility"]["available"],
        false
    );
    let lazy_retry = exchange(
        &mut auto_stdin,
        &mut auto_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {"name": "x11.accessibility_snapshot", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(lazy_retry["result"]["isError"], true);
    assert_eq!(
        lazy_retry["result"]["structuredContent"]["code"],
        "unsupported_capability"
    );
    assert_eq!(lazy_retry["result"]["structuredContent"]["retryable"], true);
    auto.kill().await.expect("stop auto-mode server");
    let _ = auto.wait().await;

    let required = Command::new(env!("CARGO_BIN_EXE_x11-mcp"))
        .args([
            "--display",
            &display,
            "--allow-host-display",
            "--accessibility",
            "required",
        ])
        .env("DBUS_SESSION_BUS_ADDRESS", &missing_bus)
        .env_remove("AT_SPI_BUS_ADDRESS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch required-mode server without a bus");
    let output = timeout(Duration::from_secs(3), required.wait_with_output())
        .await
        .expect("required mode must fail promptly")
        .expect("wait for required mode");
    assert!(!output.status.success());

    let mut dialog = Command::new("zenity")
        .args([
            "--question",
            "--title=x11-mcp AT-SPI test",
            "--text=Deterministic accessibility dialog",
            "--ok-label=Confirm",
            "--cancel-label=Cancel",
        ])
        .env("DISPLAY", &display)
        .env("GDK_BACKEND", "x11")
        .env("GTK_A11Y", "atspi")
        .env("GTK_USE_PORTAL", "0")
        .env("NO_AT_BRIDGE", "0")
        .env_remove("AT_SPI_BUS_ADDRESS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch Zenity test dialog");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut server = Command::new(env!("CARGO_BIN_EXE_x11-mcp"))
        .args([
            "--display",
            &display,
            "--allow-host-display",
            "--accessibility",
            "required",
        ])
        .env_remove("AT_SPI_BUS_ADDRESS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch required-mode semantic server");
    let mut stdin = server.stdin.take().expect("semantic stdin");
    let mut stdout = BufReader::new(server.stdout.take().expect("semantic stdout"));
    initialize(&mut stdin, &mut stdout).await;

    let windows = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 19,
            "method": "tools/call",
            "params": {
                "name": "x11.list_windows",
                "arguments": {
                    "selector": {"title_contains": "x11-mcp AT-SPI test"}
                }
            }
        }),
    )
    .await;
    let dialog_window_ref = windows["result"]["structuredContent"]["windows"]
        .as_array()
        .and_then(|windows| windows.first())
        .and_then(|window| window["window_ref"].as_str())
        .expect("Zenity X11 window reference")
        .to_owned();

    let element_wait = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {
                "name": "x11.wait_for",
                "arguments": {
                    "condition": {
                        "condition": "element_matched",
                        "selector": {"name_contains": "zenity"}
                    },
                    "timeout_ms": 5000,
                    "observe": false
                }
            }
        }),
    )
    .await;
    assert_eq!(element_wait["result"]["isError"], false, "{element_wait:?}");

    let shallow = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {
                "name": "x11.accessibility_snapshot",
                "arguments": {"max_depth": 0, "max_nodes": 1}
            }
        }),
    )
    .await;
    assert_eq!(shallow["result"]["isError"], false, "{shallow:?}");
    assert_eq!(shallow["result"]["structuredContent"]["truncated"], true);

    let mut invoked = false;
    for attempt in 0..3 {
        let snapshot = exchange(
            &mut stdin,
            &mut stdout,
            json!({
                "jsonrpc": "2.0",
                "id": 22 + attempt,
                "method": "tools/call",
                "params": {
                    "name": "x11.accessibility_snapshot",
                    "arguments": {
                        "max_depth": 16,
                        "max_nodes": 1000
                    }
                }
            }),
        )
        .await;
        assert_eq!(snapshot["result"]["isError"], false, "{snapshot:?}");
        let generation = snapshot["result"]["structuredContent"]["generation"]
            .as_u64()
            .expect("accessibility generation");
        let nodes = snapshot["result"]["structuredContent"]["nodes"]
            .as_array()
            .expect("semantic nodes");
        let button = nodes
            .iter()
            .find(|node| {
                node["name"]
                    .as_str()
                    .is_some_and(|name| name.contains("Confirm"))
            })
            .unwrap_or_else(|| panic!("Confirm button missing from snapshot: {snapshot:?}"));
        let element_ref = button["element_ref"].as_str().expect("element ref");
        let stable = exchange(
            &mut stdin,
            &mut stdout,
            json!({
                "jsonrpc": "2.0",
                "id": 60 + attempt,
                "method": "tools/call",
                "params": {
                    "name": "x11.accessibility_snapshot",
                    "arguments": {"max_depth": 16, "max_nodes": 1000}
                }
            }),
        )
        .await;
        let stable_button = stable["result"]["structuredContent"]["nodes"]
            .as_array()
            .and_then(|nodes| {
                nodes.iter().find(|node| {
                    node["name"]
                        .as_str()
                        .is_some_and(|name| name.contains("Confirm"))
                })
            })
            .expect("Confirm button in repeated snapshot");
        assert_eq!(stable_button["element_ref"].as_str(), Some(element_ref));
        assert_eq!(
            stable["result"]["structuredContent"]["generation"].as_u64(),
            Some(generation)
        );
        let window_ref = &dialog_window_ref;
        let action_name = button["actions"]
            .as_array()
            .and_then(|actions| actions.first())
            .and_then(Value::as_str)
            .expect("named Confirm action");
        let action = exchange(
            &mut stdin,
            &mut stdout,
            json!({
                "jsonrpc": "2.0",
                "id": 30 + attempt,
                "method": "tools/call",
                "params": {
                    "name": "x11.batch",
                    "arguments": {
                        "guard": {"accessibility_generation": generation},
                        "timeout_ms": 5000,
                        "steps": [
                            {
                                "step": "move_pointer",
                                "request": {
                                    "position": {
                                        "coordinate_space": "screen",
                                        "x": 1,
                                        "y": 1
                                    }
                                }
                            },
                            {
                                "step": "accessibility_action",
                                "request": {
                                    "element_ref": element_ref,
                                    "action": "invoke",
                                    "name": action_name
                                }
                            },
                            {
                                "step": "wait_for",
                                "request": {
                                    "condition": {
                                        "condition": "window_closed",
                                        "window_ref": window_ref
                                    },
                                    "timeout_ms": 3000,
                                    "observe": false
                                }
                            }
                        ]
                    }
                }
            }),
        )
        .await;
        if action["result"]["isError"] == false {
            assert_eq!(action["result"]["structuredContent"]["ok"], true);
            assert_eq!(
                action["result"]["structuredContent"]["steps"]
                    .as_array()
                    .expect("mixed batch results")
                    .len(),
                3
            );
            invoked = true;
            break;
        }
        assert!(
            matches!(
                action["result"]["structuredContent"]["code"].as_str(),
                Some("precondition_failed" | "stale_element")
            ),
            "unexpected semantic batch failure: {action:?}"
        );
    }
    assert!(invoked, "failed to invoke the Zenity Confirm action");
    let status = timeout(Duration::from_secs(3), dialog.wait())
        .await
        .expect("Zenity did not close after semantic action")
        .expect("wait for Zenity");
    assert!(status.success());

    let mut entry = Command::new("zenity")
        .args([
            "--entry",
            "--title=x11-mcp entry test",
            "--text=Editable value",
            "--entry-text=Initial value",
            "--ok-label=Save",
        ])
        .env("DISPLAY", &display)
        .env("GDK_BACKEND", "x11")
        .env("GTK_A11Y", "atspi")
        .env("GTK_USE_PORTAL", "0")
        .env("NO_AT_BRIDGE", "0")
        .env_remove("AT_SPI_BUS_ADDRESS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch Zenity entry dialog");
    let entry_wait = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 70,
            "method": "tools/call",
            "params": {
                "name": "x11.wait_for",
                "arguments": {
                    "condition": {
                        "condition": "element_matched",
                        "selector": {"name_contains": "x11-mcp entry test"}
                    },
                    "timeout_ms": 5000,
                    "observe": false
                }
            }
        }),
    )
    .await;
    assert_eq!(entry_wait["result"]["isError"], false, "{entry_wait:?}");
    let entry_snapshot = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 71,
            "method": "tools/call",
            "params": {
                "name": "x11.accessibility_snapshot",
                "arguments": {
                    "max_depth": 16,
                    "max_nodes": 1000,
                    "include_text": true
                }
            }
        }),
    )
    .await;
    let entry_generation = entry_snapshot["result"]["structuredContent"]["generation"]
        .as_u64()
        .expect("entry generation");
    let entry_node = entry_snapshot["result"]["structuredContent"]["nodes"]
        .as_array()
        .and_then(|nodes| {
            nodes.iter().find(|node| {
                node["interfaces"].as_array().is_some_and(|interfaces| {
                    interfaces.iter().any(|interface| {
                        interface
                            .as_str()
                            .is_some_and(|name| name.to_lowercase().contains("editable"))
                    })
                })
            })
        })
        .unwrap_or_else(|| panic!("editable entry missing: {entry_snapshot:?}"));
    let entry_ref = entry_node["element_ref"]
        .as_str()
        .expect("entry element ref");
    let edit_batch = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 72,
            "method": "tools/call",
            "params": {
                "name": "x11.batch",
                "arguments": {
                    "guard": {"accessibility_generation": entry_generation},
                    "steps": [
                        {
                            "step": "accessibility_action",
                            "request": {
                                "element_ref": entry_ref,
                                "action": "set_text",
                                "text": "Agent updated value"
                            }
                        }
                    ]
                }
            }
        }),
    )
    .await;
    assert_eq!(edit_batch["result"]["isError"], false, "{edit_batch:?}");
    let edited = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 73,
            "method": "tools/call",
            "params": {
                "name": "x11.accessibility_snapshot",
                "arguments": {
                    "root": {"type": "element", "element_ref": entry_ref},
                    "max_depth": 0,
                    "max_nodes": 1,
                    "include_text": true
                }
            }
        }),
    )
    .await;
    assert!(
        edited["result"]["structuredContent"]["nodes"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Agent updated value")),
        "editable text was not replaced: {edited:?}"
    );
    entry.kill().await.expect("stop Zenity entry dialog");
    let _ = entry.wait().await;

    let mut scale = Command::new("zenity")
        .args([
            "--scale",
            "--title=x11-mcp scale test",
            "--text=Numeric value",
            "--value=25",
            "--min-value=0",
            "--max-value=100",
            "--ok-label=Apply",
        ])
        .env("DISPLAY", &display)
        .env("GDK_BACKEND", "x11")
        .env("GTK_A11Y", "atspi")
        .env("GTK_USE_PORTAL", "0")
        .env("NO_AT_BRIDGE", "0")
        .env_remove("AT_SPI_BUS_ADDRESS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch Zenity scale dialog");
    let scale_wait = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 74,
            "method": "tools/call",
            "params": {
                "name": "x11.wait_for",
                "arguments": {
                    "condition": {
                        "condition": "element_matched",
                        "selector": {"name_contains": "x11-mcp scale test"}
                    },
                    "timeout_ms": 5000,
                    "observe": false
                }
            }
        }),
    )
    .await;
    assert_eq!(scale_wait["result"]["isError"], false, "{scale_wait:?}");
    let scale_snapshot = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 75,
            "method": "tools/call",
            "params": {
                "name": "x11.accessibility_snapshot",
                "arguments": {"max_depth": 16, "max_nodes": 1000}
            }
        }),
    )
    .await;
    let scale_generation = scale_snapshot["result"]["structuredContent"]["generation"]
        .as_u64()
        .expect("scale generation");
    let scale_node = scale_snapshot["result"]["structuredContent"]["nodes"]
        .as_array()
        .and_then(|nodes| nodes.iter().find(|node| node["value"].is_object()))
        .unwrap_or_else(|| panic!("numeric value node missing: {scale_snapshot:?}"));
    let scale_ref = scale_node["element_ref"]
        .as_str()
        .expect("scale element ref");
    let set_value = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 76,
            "method": "tools/call",
            "params": {
                "name": "x11.accessibility_action",
                "arguments": {
                    "element_ref": scale_ref,
                    "action": "set_value",
                    "value": 73.0,
                    "guard": {"accessibility_generation": scale_generation}
                }
            }
        }),
    )
    .await;
    assert_eq!(set_value["result"]["isError"], false, "{set_value:?}");
    let valued = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 77,
            "method": "tools/call",
            "params": {
                "name": "x11.accessibility_snapshot",
                "arguments": {
                    "root": {"type": "element", "element_ref": scale_ref},
                    "max_depth": 0,
                    "max_nodes": 1
                }
            }
        }),
    )
    .await;
    let current = valued["result"]["structuredContent"]["nodes"][0]["value"]["current"]
        .as_f64()
        .expect("current numeric value");
    assert!(
        (current - 73.0).abs() < 0.5,
        "numeric value not set: {valued:?}"
    );
    scale.kill().await.expect("stop Zenity scale dialog");
    let _ = scale.wait().await;

    server.kill().await.expect("stop semantic server");
    let _ = server.wait().await;
}

async fn initialize(stdin: &mut ChildStdin, stdout: &mut BufReader<ChildStdout>) {
    let initialized = exchange(
        stdin,
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "x11-mcp-test", "version": "0.2.0"}
            }
        }),
    )
    .await;
    assert_eq!(initialized["result"]["serverInfo"]["version"], "0.2.0");
    send(
        stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
}

fn paint_root_rectangle(display: &str, x: i16, y: i16) {
    let (connection, screen_index) = x11rb::connect(Some(display)).expect("X11 paint connection");
    let screen = &connection.setup().roots[screen_index];
    let gc = connection.generate_id().expect("graphics context id");
    connection
        .create_gc(gc, screen.root, &CreateGCAux::new().foreground(0x0000_ff00))
        .expect("create graphics context");
    connection
        .poly_fill_rectangle(
            screen.root,
            gc,
            &[Rectangle {
                x,
                y,
                width: 24,
                height: 18,
            }],
        )
        .expect("paint root rectangle");
    connection.free_gc(gc).expect("free graphics context");
    connection.flush().expect("flush root damage");
    connection
        .get_input_focus()
        .expect("root paint round-trip request")
        .reply()
        .expect("root paint round-trip reply");
}

async fn send(stdin: &mut ChildStdin, message: Value) {
    let mut bytes = serde_json::to_vec(&message).expect("serialize request");
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.expect("write MCP request");
    stdin.flush().await.expect("flush MCP request");
}

async fn exchange(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    message: Value,
) -> Value {
    send(stdin, message).await;
    let mut line = String::new();
    timeout(Duration::from_secs(10), stdout.read_line(&mut line))
        .await
        .expect("MCP response timeout")
        .expect("read MCP response");
    serde_json::from_str(&line).expect("valid MCP JSON response")
}
