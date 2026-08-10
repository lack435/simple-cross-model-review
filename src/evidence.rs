//! Phase-0 compatibility fixture for the Codex reviewer evidence service.
//!
//! This is intentionally not the evidence service itself. It is a tiny, read-only MCP
//! server used to establish load-bearing behavior in the external Codex CLI before product
//! code depends on it: explicit server injection under `--ignore-user-config`, required
//! startup, per-server tool approval, resume-time startup, JSONL tool events, and Windows
//! job inheritance. It exposes no review tool, filesystem path, shell, or network operation.

use std::io::{self, BufRead, BufReader, BufWriter, Write};

use serde_json::{json, Value};

pub const PROBE_FLAG: &str = "--evidence-probe-server";

const PROTOCOL: &str = "2025-06-18";
const TOOL: &str = "repository_scope";
const SCHEMA_VERSION: u32 = 1;
const MAX_LINE_BYTES: usize = 1024 * 1024;

pub fn valid_probe_nonce(nonce: &str) -> bool {
    !nonce.is_empty()
        && nonce.len() <= 128
        && nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

pub fn serve_probe_stdio(nonce: &str, approval_control: bool) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_probe(
        BufReader::new(stdin.lock()),
        BufWriter::new(stdout.lock()),
        nonce,
        approval_control,
    )
}

fn serve_probe<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    nonce: &str,
    approval_control: bool,
) -> io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(());
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP request exceeded 1 MiB",
            ));
        }

        let request: Value = serde_json::from_str(line.trim_end()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON-RPC: {e}"))
        })?;
        let Some(response) = handle_probe(&request, nonce, approval_control) else {
            continue;
        };
        serde_json::to_writer(&mut writer, &response)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
}

fn handle_probe(request: &Value, nonce: &str, approval_control: bool) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let id = request.get("id").cloned();

    // Notifications have no response. Codex sends this after initialize, and treating it as
    // an error would put non-protocol traffic on stdout in the middle of the handshake.
    let id = id?;

    let result = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "cross-review-evidence-probe", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Phase-0 read-only compatibility fixture. Call repository_scope exactly as directed."
        }),
        "tools/list" => json!({"tools": [tool_definition(approval_control)]}),
        "tools/call" => {
            let params = request.get("params").unwrap_or(&Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params.get("arguments").unwrap_or(&Value::Null);
            if name != TOOL {
                return Some(tool_error(
                    id,
                    format!("unknown evidence probe tool '{name}'"),
                ));
            }
            if !arguments.as_object().is_some_and(serde_json::Map::is_empty) {
                return Some(tool_error(
                    id,
                    "repository_scope accepts no arguments".to_string(),
                ));
            }
            let in_job = crate::winjob::current_process_in_job();
            let structured = json!({
                "schema_version": SCHEMA_VERSION,
                "nonce": nonce,
                "current_process_in_job": in_job,
                "process_id": std::process::id(),
            });
            json!({
                "content": [{"type": "text", "text": structured.to_string()}],
                "structuredContent": structured,
                "isError": false,
            })
        }
        "ping" => json!({}),
        other => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not found: {other}")},
            }))
        }
    };

    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn tool_definition(approval_control: bool) -> Value {
    // The control mode remains operationally read-only. Its deliberately conservative MCP
    // annotations make Codex request approval, so a two-server probe can prove that
    // `default_tools_approval_mode="approve"` on the evidence entry does not leak to a
    // different server. No code path gains a destructive operation.
    json!({
        "name": TOOL,
        "description": if approval_control {
            "Approval-scope control: return process metadata without reading or writing repository data."
        } else {
            "Return this probe process's nonce and Windows job-membership state. Reads no repository data."
        },
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "schema_version": {"type": "integer"},
                "nonce": {"type": "string"},
                "current_process_in_job": {"type": ["boolean", "null"]},
                "process_id": {"type": "integer"}
            },
            "required": ["schema_version", "nonce", "current_process_in_job", "process_id"],
            "additionalProperties": false
        },
        "annotations": {
            "readOnlyHint": !approval_control,
            "destructiveHint": approval_control,
            "idempotentHint": true,
            "openWorldHint": false
        }
    })
}

fn tool_error(id: Value, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": message}],
            "isError": true
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(lines: &[Value], nonce: &str) -> Vec<Value> {
        exchange_with_mode(lines, nonce, false)
    }

    fn exchange_with_mode(lines: &[Value], nonce: &str, approval_control: bool) -> Vec<Value> {
        let mut input = Vec::new();
        for line in lines {
            serde_json::to_writer(&mut input, line).unwrap();
            input.push(b'\n');
        }
        let mut output = Vec::new();
        serve_probe(io::Cursor::new(input), &mut output, nonce, approval_control).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn probe_nonce_is_small_ascii_and_unambiguous() {
        for good in ["rv-1-2", "abc_DEF", "9"] {
            assert!(valid_probe_nonce(good), "{good}");
        }
        for bad in ["", "a b", "../x", "é", &"x".repeat(129)] {
            assert!(!valid_probe_nonce(bad), "{bad}");
        }
    }

    #[test]
    fn initialize_and_tools_list_expose_only_the_probe() {
        let responses = exchange(
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            ],
            "nonce-1",
        );
        assert_eq!(responses.len(), 2);
        assert_eq!(
            responses[0]["result"]["serverInfo"]["name"],
            "cross-review-evidence-probe"
        );
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], TOOL);
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
        assert_eq!(tools[0]["inputSchema"]["additionalProperties"], false);
    }

    #[test]
    fn scope_echoes_the_nonce_and_reports_job_membership_without_guessing() {
        let responses = exchange(
            &[json!({
                "jsonrpc":"2.0", "id":7, "method":"tools/call",
                "params":{"name":TOOL,"arguments":{}}
            })],
            "turn-42",
        );
        let structured = &responses[0]["result"]["structuredContent"];
        assert_eq!(structured["schema_version"], SCHEMA_VERSION);
        assert_eq!(structured["nonce"], "turn-42");
        assert_eq!(structured["process_id"], std::process::id());
        assert!(
            structured["current_process_in_job"].is_boolean()
                || structured["current_process_in_job"].is_null()
        );
        assert_eq!(responses[0]["result"]["isError"], false);
    }

    #[test]
    fn approval_control_changes_only_metadata_not_the_tool_operation() {
        let requests = [
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            json!({
                "jsonrpc":"2.0", "id":2, "method":"tools/call",
                "params":{"name":TOOL,"arguments":{}}
            }),
        ];
        let responses = exchange_with_mode(&requests, "control", true);
        let tool = &responses[0]["result"]["tools"][0];
        assert_eq!(tool["annotations"]["readOnlyHint"], false);
        assert_eq!(tool["annotations"]["destructiveHint"], true);
        assert_eq!(responses[1]["result"]["isError"], false);
        assert_eq!(
            responses[1]["result"]["structuredContent"]["nonce"],
            "control"
        );
    }

    #[test]
    fn unknown_or_argument_bearing_tools_fail_as_tool_results() {
        let responses = exchange(
            &[
                json!({
                    "jsonrpc":"2.0", "id":1, "method":"tools/call",
                    "params":{"name":"cross_model_review","arguments":{}}
                }),
                json!({
                    "jsonrpc":"2.0", "id":2, "method":"tools/call",
                    "params":{"name":TOOL,"arguments":{"path":"secret"}}
                }),
            ],
            "n",
        );
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["result"]["isError"], true);
        assert_eq!(responses[1]["result"]["isError"], true);
    }
}
