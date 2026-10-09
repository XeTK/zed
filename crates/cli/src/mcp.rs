//! `zed --mcp`: an MCP server on stdin and stdout that lets an AI client such as
//! Claude Code read the agent threads in the running Zed.
//!
//! It holds no state of its own. Each tool call is forwarded, as one line of
//! JSON, to the control server inside Zed (see `agent_ui::thread_control`), which
//! is only listening when `agent.thread_control` is turned on.

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;

const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn run() -> Result<()> {
    #[cfg(not(unix))]
    {
        bail!("zed --mcp is only available on macOS and Linux");
    }

    #[cfg(unix)]
    {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Some(response) = handle_line(&line, &control_call) else {
                continue;
            };
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
        Ok(())
    }
}

/// Handles one JSON-RPC message. `call` forwards a tool to the control server.
/// Returns `None` for notifications, which get no reply.
fn handle_line(line: &str, call: &dyn Fn(&str, Value) -> Result<Value>) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => {
            return Some(error_reply(
                Value::Null,
                -32700,
                &format!("parse error: {error}"),
            ));
        }
    };
    let id = message.get("id").cloned();
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    // A message without an id is a notification.
    let Some(id) = id else {
        return None;
    };

    Some(match method {
        "initialize" => ok_reply(
            id,
            json!({
                "protocolVersion": params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSION),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "zed-threads", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Read the agent threads open in Zed. Zed must have agent.thread_control set to read_only.",
            }),
        ),
        "ping" => ok_reply(id, json!({})),
        "tools/list" => ok_reply(id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !tools().iter().any(|tool| tool["name"] == name) {
                return Some(error_reply(id, -32602, &format!("unknown tool {name:?}")));
            }
            ok_reply(
                id,
                match call(name, arguments) {
                    Ok(result) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }],
                    }),
                    Err(error) => json!({
                        "isError": true,
                        "content": [{ "type": "text", "text": format!("{error:#}") }],
                    }),
                },
            )
        }
        other => error_reply(id, -32601, &format!("method not found: {other}")),
    })
}

fn ok_reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tools() -> Vec<Value> {
    let read_only = json!({ "readOnlyHint": true });
    vec![
        json!({
            "name": "list_projects",
            "description": "List the projects open in Zed, with their folders and whether they are local or remote.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        }),
        json!({
            "name": "list_threads",
            "description": "List Zed agent threads, newest first, with their id, title, agent, status (idle, running or waiting_for_confirmation), and folders.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Only threads whose folders contain this text." },
                    "include_archived": { "type": "boolean", "description": "Also list archived threads. Default false." },
                    "limit": { "type": "integer", "description": "How many to return. Default 50, at most 500." },
                },
            },
            "annotations": read_only,
        }),
        json!({
            "name": "get_thread",
            "description": "Read a Zed agent thread's messages and tool calls as text. Long threads are paged: use offset and limit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "A thread id from list_threads." },
                    "offset": { "type": "integer", "description": "The first entry to return. Default 0." },
                    "limit": { "type": "integer", "description": "How many entries to return. Default 50, at most 500." },
                },
                "required": ["id"],
            },
            "annotations": read_only,
        }),
    ]
}

#[cfg(unix)]
fn control_call(method: &str, params: Value) -> Result<Value> {
    call_control_server(&paths::data_dir().join("control.json"), method, params)
}

#[cfg(unix)]
fn call_control_server(
    discovery_path: &std::path::Path,
    method: &str,
    params: Value,
) -> Result<Value> {
    use std::os::unix::net::UnixStream;

    let discovery: Value = serde_json::from_str(
        &std::fs::read_to_string(discovery_path).with_context(|| {
            format!(
                "Zed is not accepting thread requests: {} is missing. Set agent.thread_control to read_only in Zed's settings.",
                discovery_path.display()
            )
        })?,
    )
    .context("control.json is not valid")?;
    let socket = PathBuf::from(
        discovery["socket"]
            .as_str()
            .context("control.json has no socket")?,
    );
    let token = discovery["token"]
        .as_str()
        .context("control.json has no token")?;

    let stream = UnixStream::connect(&socket).with_context(|| {
        format!(
            "could not reach Zed at {}. It may have quit; reopen it.",
            socket.display()
        )
    })?;
    let timeout = Some(std::time::Duration::from_secs(60));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;

    let mut writer = stream.try_clone()?;
    writeln!(
        writer,
        "{}",
        json!({ "token": token, "method": method, "params": params })
    )?;
    let mut response = String::new();
    std::io::BufReader::new(stream).read_line(&mut response)?;
    let response: Value = serde_json::from_str(&response).context("Zed sent an invalid reply")?;
    if response["ok"] == true {
        Ok(response["result"].clone())
    } else {
        bail!(
            "{}",
            response["error"]
                .as_str()
                .unwrap_or("Zed refused the request")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(line: &str, call: &dyn Fn(&str, Value) -> Result<Value>) -> Value {
        handle_line(line, call).expect("a reply")
    }

    fn unused(_: &str, _: Value) -> Result<Value> {
        panic!("no tool should be called")
    }

    #[test]
    fn test_initialize_advertises_tools_and_echoes_the_protocol_version() {
        let response = reply(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
            &unused,
        );
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        assert!(response["result"]["capabilities"]["tools"].is_object());
        assert_eq!(response["result"]["serverInfo"]["name"], "zed-threads");
    }

    #[test]
    fn test_notifications_get_no_reply() {
        assert!(
            handle_line(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                &unused
            )
            .is_none()
        );
    }

    #[test]
    fn test_tools_are_read_only_and_have_schemas() {
        let response = reply(
            r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#,
            &unused,
        );
        let tools = response["result"]["tools"].as_array().unwrap();
        let names: Vec<_> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["list_projects", "list_threads", "get_thread"]);
        for tool in tools {
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        assert_eq!(tools[2]["inputSchema"]["required"], json!(["id"]));
    }

    #[test]
    fn test_a_tool_call_is_forwarded_and_its_result_returned_as_text() {
        let seen = std::cell::RefCell::new(None);
        let call = |method: &str, params: Value| -> Result<Value> {
            *seen.borrow_mut() = Some((method.to_string(), params));
            Ok(json!({ "threads": [] }))
        };
        let response = reply(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_threads","arguments":{"limit":3}}}"#,
            &call,
        );
        assert_eq!(
            seen.borrow().clone(),
            Some(("list_threads".to_string(), json!({ "limit": 3 })))
        );
        assert!(response["result"].get("isError").is_none());
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            json!({ "threads": [] })
        );
    }

    #[test]
    fn test_a_failed_tool_call_is_reported_as_a_tool_error_not_a_protocol_error() {
        let call =
            |_: &str, _: Value| -> Result<Value> { bail!("Zed is not accepting thread requests") };
        let response = reply(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_thread","arguments":{"id":"x"}}}"#,
            &call,
        );
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("not accepting")
        );
    }

    #[test]
    fn test_unknown_tools_and_methods_are_protocol_errors() {
        let call = |_: &str, _: Value| -> Result<Value> { panic!("must not be called") };
        let response = reply(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"delete_everything"}}"#,
            &call,
        );
        assert_eq!(response["error"]["code"], -32602);
        let response = reply(
            r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#,
            &unused,
        );
        assert_eq!(response["error"]["code"], -32601);
        let response = reply("not json", &unused);
        assert_eq!(response["error"]["code"], -32700);
    }

    #[cfg(unix)]
    #[test]
    fn test_calls_reach_the_control_server_with_the_token() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("s.sock");
        let discovery = directory.path().join("control.json");
        std::fs::write(
            &discovery,
            json!({ "socket": socket, "token": "secret", "pid": 1 }).to_string(),
        )
        .unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream)
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let response = if request["token"] == "secret" {
                json!({ "ok": true, "result": { "echo": request["method"], "params": request["params"] } })
            } else {
                json!({ "ok": false, "error": "invalid token" })
            };
            writeln!(writer, "{response}").unwrap();
        });

        let result =
            call_control_server(&discovery, "list_threads", json!({ "limit": 2 })).unwrap();
        assert_eq!(
            result,
            json!({ "echo": "list_threads", "params": { "limit": 2 } })
        );
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_a_missing_control_file_says_how_to_turn_it_on() {
        let directory = tempfile::tempdir().unwrap();
        let error = call_control_server(
            &directory.path().join("control.json"),
            "list_threads",
            json!({}),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("agent.thread_control"),
            "{error:#}"
        );
    }
}
