use std::process::Stdio;

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    time::{Duration, timeout},
};

#[tokio::test]
async fn initializes_and_serves_tools_over_stdio() {
    if std::env::var_os("X11_MCP_RUN_X11_TESTS").is_none() {
        return;
    }
    let display = std::env::var("DISPLAY").expect("DISPLAY must be set by the test harness");
    let mut child = Command::new(env!("CARGO_BIN_EXE_x11-mcp"))
        .args(["--display", &display, "--allow-host-display"])
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
                "clientInfo": {"name": "x11-mcp-test", "version": "0.1.0"}
            }
        }),
    )
    .await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "x11-mcp");
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
    assert_eq!(tools.len(), 12);
    assert!(tools.iter().any(|tool| tool["name"] == "x11.observe"));
    assert!(tools.iter().all(|tool| tool["annotations"].is_object()));

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

    let observation = exchange(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
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

    child.kill().await.expect("stop x11-mcp");
    let _ = child.wait().await;
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
    timeout(Duration::from_secs(5), stdout.read_line(&mut line))
        .await
        .expect("MCP response timeout")
        .expect("read MCP response");
    serde_json::from_str(&line).expect("valid MCP JSON response")
}
