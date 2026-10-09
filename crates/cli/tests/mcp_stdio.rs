//! Runs the real `cli --mcp` as an MCP client would: JSON-RPC lines on stdin and
//! stdout, with a fake control server standing in for Zed.

// These tests run real processes outside gpui.
#![allow(clippy::disallowed_methods)]
#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    io::{BufRead as _, BufReader, Write as _},
    os::unix::net::UnixListener,
    process::{ChildStdin, ChildStdout, Command, Stdio},
};

struct Client {
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Client {
    fn notify(&mut self, message: Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{message}").unwrap();
    }

    fn ask(&mut self, message: Value) -> Value {
        self.notify(message);
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

#[test]
fn test_an_mcp_client_can_list_and_read_threads_through_the_bridge() {
    let directory = tempfile::tempdir().unwrap();
    let data_dir = directory.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let socket = directory.path().join("s.sock");
    std::fs::write(
        data_dir.join("control.json"),
        json!({ "socket": socket, "token": "secret", "pid": 1 }).to_string(),
    )
    .unwrap();

    let listener = UnixListener::bind(&socket).unwrap();
    let fake_zed = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let result = match request["method"].as_str().unwrap() {
                "list_threads" => {
                    json!({ "total": 1, "threads": [{ "id": "t-1", "title": "Fix the build" }] })
                }
                _ => json!({ "id": "t-1", "entries": [{ "role": "user", "text": "hello" }] }),
            };
            writeln!(writer, "{}", json!({ "ok": true, "result": result })).unwrap();
            seen.push(request);
        }
        seen
    });

    let mut bridge = Command::new(env!("CARGO_BIN_EXE_cli"))
        .arg("--mcp")
        .arg("--user-data-dir")
        .arg(&data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut client = Client {
        stdin: Some(bridge.stdin.take().unwrap()),
        stdout: BufReader::new(bridge.stdout.take().unwrap()),
    };

    let initialized =
        client.ask(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
    assert_eq!(initialized["result"]["serverInfo"]["name"], "zed-threads");
    client.notify(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let listed = client.ask(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "list_threads", "arguments": { "limit": 5 } }
    }));
    let text = listed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Fix the build"), "{text}");

    let read = client.ask(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "get_thread", "arguments": { "id": "t-1" } }
    }));
    assert!(
        read["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hello")
    );

    client.stdin = None;
    bridge.wait().unwrap();

    let seen = fake_zed.join().unwrap();
    assert_eq!(
        seen[0]["token"], "secret",
        "the token from control.json is sent"
    );
    assert_eq!(seen[0]["method"], "list_threads");
    assert_eq!(seen[0]["params"], json!({ "limit": 5 }));
    assert_eq!(seen[1]["method"], "get_thread");
}
