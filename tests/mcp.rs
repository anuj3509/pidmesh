use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use pidmesh::store::MeshStore;
use serde_json::{Value, json};

fn exchange(stdin: &mut impl Write, stdout: &mut impl BufRead, request: &Value) -> Result<Value> {
    writeln!(stdin, "{request}")?;
    stdin.flush()?;
    let mut response = String::new();
    stdout.read_line(&mut response)?;
    serde_json::from_str(&response).context("invalid MCP response")
}

#[test]
fn stdio_server_negotiates_and_exposes_native_tools() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_pidmesh-mcp"))
        .env("PIDMESH_DB", directory.path().join("mesh.db"))
        .env("PIDMESH_WORKSPACE", directory.path())
        .env("PIDMESH_AGENT_NAME", "integration-test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("missing MCP stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("missing MCP stdout")?);

    let initialized = exchange(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "pidmesh-test", "version": "1.0.0"}
            }
        }),
    )?;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "PidMesh");
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )?;
    stdin.flush()?;

    let tools = exchange(
        &mut stdin,
        &mut stdout,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )?;
    let names = tools["result"]["tools"]
        .as_array()
        .context("missing MCP tools")?
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 13);
    assert!(names.contains(&"claim"));
    assert!(names.contains(&"reserve_resources"));
    assert!(names.contains(&"release_resources"));
    assert!(names.contains(&"wait_for_events"));
    assert!(names.contains(&"sync_footprint"));
    assert!(names.contains(&"collisions"));

    let remembered = exchange(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "remember",
                "arguments": {"content": "native MCP memory", "kind": "decision"}
            }
        }),
    )?;
    assert_eq!(remembered["result"]["isError"], false);
    assert!(remembered["result"]["structuredContent"]["memory_id"].is_number());

    let reserved = exchange(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "reserve_resources",
                "arguments": {
                    "resources": ["path:src/mcp.rs", "port:4399"],
                    "task": "mcp-integration",
                    "lease_seconds": 60
                }
            }
        }),
    )?;
    assert_eq!(reserved["result"]["isError"], false);
    assert_eq!(reserved["result"]["structuredContent"]["acquired"], true);

    drop(stdin);
    let status = child.wait()?;
    assert!(status.success());
    let store = MeshStore::new(directory.path().join("mesh.db"))?;
    let mesh = store.status(None, Some(directory.path()))?;
    assert_eq!(mesh["agents"][0]["status"], "stopped");
    assert_eq!(mesh["resources"], json!([]));
    Ok(())
}
