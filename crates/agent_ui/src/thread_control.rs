//! A local control server that lets other programs, such as an MCP bridge,
//! read the agent threads in this Zed.
//!
//! It is off unless `agent.thread_control` says otherwise. When on, it listens
//! on a Unix socket that only this user can use, and every request must carry
//! the token written to `control.json` in the data directory. Requests and
//! responses are single lines of JSON:
//!
//! ```text
//! {"token": "...", "method": "list_threads", "params": {"limit": 10}}
//! {"ok": true, "result": {...}}   or   {"ok": false, "error": "..."}
//! ```

use crate::AgentPanel;
use crate::thread_metadata_store::{ThreadMetadata, ThreadMetadataStore};
use acp_thread::{AcpThread, ThreadStatus};
use agent::ThreadStore;
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result, anyhow};
use futures::{AsyncBufReadExt as _, AsyncWriteExt as _, StreamExt as _, io::BufReader};
use gpui::{App, AsyncApp, Entity, Global, Task};
use net::async_net::{UnixListener, UnixStream};
use serde::Deserialize;
use serde_json::{Value, json};
use settings::{Settings as _, ThreadControlMode};
use std::{collections::HashMap, path::PathBuf};
use workspace::{MultiWorkspace, Workspace};

const DEFAULT_THREAD_LIMIT: usize = 50;
const MAX_THREAD_LIMIT: usize = 500;
const DEFAULT_ENTRY_LIMIT: usize = 50;
const MAX_ENTRY_LIMIT: usize = 500;
const MAX_ENTRY_CHARS: usize = 20_000;
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

pub fn init(cx: &mut App) {
    cx.set_global(ControlServer::default());
    apply_settings(cx);
    cx.observe_global::<settings::SettingsStore>(apply_settings)
        .detach();
}

#[derive(Default)]
struct ControlServer {
    running: Option<RunningServer>,
}

impl Global for ControlServer {}

struct RunningServer {
    mode: ThreadControlMode,
    socket_path: PathBuf,
    discovery_path: PathBuf,
    _accept_task: Task<()>,
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        std::fs::remove_file(&self.socket_path).ok();
        std::fs::remove_file(&self.discovery_path).ok();
    }
}

fn apply_settings(cx: &mut App) {
    let mode = AgentSettings::get_global(cx).thread_control;
    let running_mode = cx
        .global::<ControlServer>()
        .running
        .as_ref()
        .map(|r| r.mode);
    if running_mode == Some(mode) || (running_mode.is_none() && mode == ThreadControlMode::Off) {
        return;
    }

    // Dropping the old server closes its socket and removes its files.
    cx.global_mut::<ControlServer>().running = None;
    if mode == ThreadControlMode::Off {
        return;
    }

    match start_server(mode, socket_path(), discovery_path(), cx) {
        Ok(server) => cx.global_mut::<ControlServer>().running = Some(server),
        Err(error) => log::error!("failed to start the thread control server: {error:#}"),
    }
}

fn start_server(
    mode: ThreadControlMode,
    socket_path: PathBuf,
    discovery_path: PathBuf,
    cx: &mut App,
) -> Result<RunningServer> {
    std::fs::remove_file(&socket_path).ok();
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    restrict_to_owner(&socket_path)?;

    let token = uuid::Uuid::new_v4().simple().to_string();
    write_discovery_file(&discovery_path, &socket_path, &token)?;

    let accept_task = cx.spawn(async move |cx| {
        let mut incoming = listener.incoming();
        while let Some(stream) = incoming.next().await {
            let Ok(stream) = stream else {
                continue;
            };
            let token = token.clone();
            cx.spawn(async move |cx| {
                if let Err(error) = serve_connection(stream, &token, cx).await {
                    log::debug!("thread control connection ended: {error:#}");
                }
            })
            .detach();
        }
    });

    Ok(RunningServer {
        mode,
        socket_path,
        discovery_path,
        _accept_task: accept_task,
    })
}

/// A short path: Unix socket paths longer than about 100 bytes cannot bind.
fn socket_path() -> PathBuf {
    PathBuf::from("/tmp").join(format!("zed-control-{}.sock", std::process::id()))
}

/// How a client finds the running server and the token to use.
pub fn discovery_path() -> PathBuf {
    paths::data_dir().join("control.json")
}

fn write_discovery_file(
    path: &std::path::Path,
    socket: &std::path::Path,
    token: &str,
) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let contents = json!({
        "socket": socket,
        "token": token,
        "pid": std::process::id(),
    });
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(contents.to_string().as_bytes())?;
    Ok(())
}

fn restrict_to_owner(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))
}

#[derive(Deserialize)]
struct Request {
    token: String,
    method: String,
    #[serde(default)]
    params: Value,
}

async fn serve_connection(stream: UnixStream, token: &str, cx: &mut AsyncApp) -> Result<()> {
    let (reader, mut writer) = futures::AsyncReadExt::split(stream);
    let mut lines = BufReader::new(futures::AsyncReadExt::take(reader, u64::MAX)).lines();
    while let Some(line) = lines.next().await {
        let line = line?;
        if line.len() as u64 > MAX_REQUEST_BYTES {
            return Err(anyhow!("request too large"));
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Err(error) => error_response(&format!("invalid request: {error}")),
            Ok(request) if !tokens_match(&request.token, token) => error_response("invalid token"),
            Ok(request) => match handle_request(&request.method, request.params, cx).await {
                Ok(result) => json!({ "ok": true, "result": result }),
                Err(error) => error_response(&format!("{error:#}")),
            },
        };
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
    }
    Ok(())
}

fn error_response(message: &str) -> Value {
    json!({ "ok": false, "error": message })
}

/// Compared without stopping at the first difference.
fn tokens_match(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

async fn handle_request(method: &str, params: Value, cx: &mut AsyncApp) -> Result<Value> {
    match method {
        "list_projects" => cx.update(|cx| list_projects(cx)),
        "list_threads" => cx.update(|cx| list_threads(params, cx)),
        "get_thread" => get_thread(params, cx).await,
        other => Err(anyhow!("unknown method {other:?}")),
    }
}

fn workspaces(cx: &App) -> Vec<Entity<Workspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .filter_map(|window| window.read(cx).ok())
        .flat_map(|multi_workspace| multi_workspace.workspaces().cloned().collect::<Vec<_>>())
        .collect()
}

fn list_projects(cx: &App) -> Result<Value> {
    let mut projects = Vec::new();
    for window in cx.windows() {
        let Some(window) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for group in multi_workspace.project_groups(cx) {
            let paths: Vec<_> = group
                .key
                .path_list()
                .paths()
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            projects.push(json!({
                "name": paths
                    .first()
                    .and_then(|path| path.rsplit('/').next())
                    .unwrap_or_default(),
                "paths": paths,
                "kind": match group.key.host() {
                    None => "local",
                    Some(host) => host.connection_type(),
                },
            }));
        }
    }
    Ok(json!({ "projects": projects }))
}

#[derive(Default, Deserialize)]
struct ListThreadsParams {
    /// Only threads whose folders contain this text.
    project: Option<String>,
    #[serde(default)]
    include_archived: bool,
    limit: Option<usize>,
}

fn live_statuses(cx: &App) -> HashMap<crate::thread_metadata_store::ThreadId, &'static str> {
    let mut statuses = HashMap::new();
    for workspace in workspaces(cx) {
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            continue;
        };
        for view in panel.read(cx).conversation_views() {
            let view = view.read(cx);
            let Some(thread) = view.root_thread(cx) else {
                continue;
            };
            statuses.insert(view.thread_id, thread_status(thread.read(cx)));
        }
    }
    statuses
}

fn thread_status(thread: &AcpThread) -> &'static str {
    if thread.is_waiting_for_confirmation() {
        "waiting_for_confirmation"
    } else if thread.status() == ThreadStatus::Generating {
        "running"
    } else {
        "idle"
    }
}

fn list_threads(params: Value, cx: &App) -> Result<Value> {
    let params: ListThreadsParams = serde_json::from_value(params).unwrap_or_default();
    let limit = params
        .limit
        .unwrap_or(DEFAULT_THREAD_LIMIT)
        .min(MAX_THREAD_LIMIT);
    let statuses = live_statuses(cx);
    let store = ThreadMetadataStore::global(cx).read(cx);

    let mut threads: Vec<&ThreadMetadata> = store
        .entries()
        .filter(|metadata| params.include_archived || !metadata.archived)
        .filter(|metadata| {
            params.project.as_ref().is_none_or(|project| {
                metadata
                    .folder_paths()
                    .paths()
                    .iter()
                    .any(|path| path.to_string_lossy().contains(project.as_str()))
            })
        })
        .collect();
    threads.sort_by_key(|metadata| {
        std::cmp::Reverse(metadata.interacted_at.unwrap_or(metadata.updated_at))
    });
    let total = threads.len();

    let threads: Vec<Value> = threads
        .into_iter()
        .take(limit)
        .map(|metadata| {
            json!({
                "id": metadata.thread_id.to_key_string(),
                "title": metadata.display_title(),
                "agent": metadata.agent_id.to_string(),
                "status": statuses.get(&metadata.thread_id).copied().unwrap_or("idle"),
                "open": statuses.contains_key(&metadata.thread_id),
                "draft": metadata.is_draft(),
                "archived": metadata.archived,
                "created_at": metadata.created_at,
                "updated_at": metadata.updated_at,
                "last_interacted_at": metadata.interacted_at,
                "folders": metadata
                    .folder_paths()
                    .paths()
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "total": total, "threads": threads }))
}

#[derive(Deserialize)]
struct GetThreadParams {
    id: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

async fn get_thread(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let params: GetThreadParams =
        serde_json::from_value(params).context("get_thread needs an id")?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_ENTRY_LIMIT)
        .min(MAX_ENTRY_LIMIT);

    enum Source {
        Live(Entity<AcpThread>),
        Saved(acp::SessionId),
    }

    let (metadata, source) = cx.update(|cx| -> Result<_> {
        let store = ThreadMetadataStore::global(cx).read(cx);
        let metadata = store
            .entries()
            .find(|metadata| metadata.thread_id.to_key_string() == params.id)
            .cloned()
            .with_context(|| format!("no thread with id {}", params.id))?;

        for workspace in workspaces(cx) {
            let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
                continue;
            };
            for view in panel.read(cx).conversation_views() {
                let view = view.read(cx);
                if view.thread_id == metadata.thread_id
                    && let Some(thread) = view.root_thread(cx)
                {
                    return Ok((metadata, Source::Live(thread)));
                }
            }
        }

        let session_id = metadata
            .session_id
            .clone()
            .context("this thread is a draft and has no messages")?;
        Ok((metadata, Source::Saved(session_id)))
    })?;

    let (status, entries): (&str, Vec<Value>) = match source {
        Source::Live(thread) => cx.update(|cx| {
            let thread = thread.read(cx);
            let entries = thread
                .entries()
                .iter()
                .enumerate()
                .map(|(index, entry)| entry_json(index, entry, cx))
                .collect();
            (thread_status(thread), entries)
        }),
        Source::Saved(session_id) => {
            let load = cx.update(|cx| {
                ThreadStore::global(cx).update(cx, |store, cx| store.load_thread(session_id, cx))
            });
            let thread = load.await?.context("the saved thread could not be found")?;
            let entries = thread
                .messages
                .iter()
                .enumerate()
                .map(|(index, message)| {
                    json!({
                        "index": index,
                        "role": message_role(message),
                        "text": truncate(message.to_markdown()),
                    })
                })
                .collect();
            ("idle", entries)
        }
    };

    let total_entries = entries.len();
    let entries: Vec<Value> = entries
        .into_iter()
        .skip(params.offset)
        .take(limit)
        .collect();
    Ok(json!({
        "id": metadata.thread_id.to_key_string(),
        "title": metadata.display_title(),
        "agent": metadata.agent_id.to_string(),
        "status": status,
        "archived": metadata.archived,
        "total_entries": total_entries,
        "offset": params.offset,
        "entries": entries,
    }))
}

fn entry_json(index: usize, entry: &acp_thread::AgentThreadEntry, cx: &App) -> Value {
    use acp_thread::AgentThreadEntry;
    let role = match entry {
        AgentThreadEntry::UserMessage(_) => "user",
        AgentThreadEntry::AssistantMessage(_) => "assistant",
        AgentThreadEntry::ToolCall(_) => "tool_call",
        _ => "other",
    };
    let mut value = json!({
        "index": index,
        "role": role,
        "text": truncate(entry.to_markdown(cx)),
    });
    match entry {
        AgentThreadEntry::UserMessage(message) => {
            value["sent_at"] = json!(message.created_at);
        }
        AgentThreadEntry::AssistantMessage(message) => {
            value["sent_at"] = json!(message.created_at);
        }
        AgentThreadEntry::ToolCall(call) => {
            value["status"] = json!(call.status.to_string());
        }
        _ => {}
    }
    value
}

fn message_role(message: &agent::Message) -> &'static str {
    match message {
        agent::Message::User(_) => "user",
        agent::Message::Agent(_) => "assistant",
        agent::Message::Resume => "other",
        agent::Message::Compaction(_) => "other",
    }
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_ENTRY_CHARS {
        let mut end = MAX_ENTRY_CHARS;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[truncated]");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gpui::TestAppContext;
    use project::WorktreePaths;
    use std::io::{BufRead as _, Write as _};
    use std::time::Duration;
    use util::path_list::PathList;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            release_channel::init("0.0.0".parse().unwrap(), cx);
            ThreadMetadataStore::init_global(cx);
            ThreadStore::init_global(cx);
        });
        cx.run_until_parked();
    }

    fn save_thread(title: &str, folder: &str, archived: bool, cx: &mut TestAppContext) {
        cx.update(|cx| {
            let paths = PathList::new(&[std::path::Path::new(folder)]);
            let now = Utc::now();
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(
                    ThreadMetadata {
                        thread_id: crate::thread_metadata_store::ThreadId::new(),
                        session_id: Some(acp::SessionId::new(title)),
                        agent_id: agent::ZED_AGENT_ID.clone(),
                        title: Some(title.to_string().into()),
                        title_override: None,
                        updated_at: now,
                        created_at: Some(now),
                        interacted_at: None,
                        worktree_paths: WorktreePaths::from_folder_paths(&paths),
                        remote_connection: None,
                        archived,
                    },
                    cx,
                );
            });
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_list_threads_hides_archived_filters_by_project_and_limits(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        save_thread("alpha", "/work/app", false, cx);
        save_thread("beta", "/work/app", true, cx);
        save_thread("gamma", "/work/lib", false, cx);

        let titles = |params: Value, cx: &mut TestAppContext| -> Vec<String> {
            let result = cx.update(|cx| list_threads(params, cx)).unwrap();
            let mut titles: Vec<String> = result["threads"]
                .as_array()
                .unwrap()
                .iter()
                .map(|thread| thread["title"].as_str().unwrap().to_string())
                .collect();
            titles.sort();
            titles
        };

        assert_eq!(titles(json!({}), cx), ["alpha", "gamma"]);
        assert_eq!(
            titles(json!({ "include_archived": true }), cx),
            ["alpha", "beta", "gamma"]
        );
        assert_eq!(titles(json!({ "project": "/work/lib" }), cx), ["gamma"]);
        assert_eq!(titles(json!({ "limit": 1 }), cx).len(), 1);

        let result = cx
            .update(|cx| list_threads(json!({ "limit": 1 }), cx))
            .unwrap();
        assert_eq!(
            result["total"], 2,
            "total counts what matched, not what was returned"
        );
        let thread = &result["threads"][0];
        assert_eq!(thread["status"], "idle");
        assert_eq!(thread["open"], false);
        assert_eq!(thread["agent"], agent::ZED_AGENT_ID.to_string());
    }

    #[gpui::test]
    async fn test_get_thread_rejects_unknown_ids_and_drafts(cx: &mut TestAppContext) {
        init_test(cx);
        let error = handle_request_in_test("get_thread", json!({ "id": "nope" }), cx)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no thread with id nope"),
            "{error}"
        );

        let error = handle_request_in_test("get_thread", json!({}), cx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("needs an id"), "{error}");

        let error = handle_request_in_test("nonsense", json!({}), cx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown method"), "{error}");
    }

    async fn handle_request_in_test(
        method: &'static str,
        params: Value,
        cx: &mut TestAppContext,
    ) -> Result<Value> {
        cx.spawn(async move |cx| handle_request(method, params, &mut cx.clone()).await)
            .await
    }

    #[gpui::test]
    async fn test_the_socket_needs_the_token_and_answers_requests(cx: &mut TestAppContext) {
        init_test(cx);
        save_thread("alpha", "/work/app", false, cx);

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("c.sock");
        let discovery = directory.path().join("control.json");
        let server = cx
            .update(|cx| {
                start_server(
                    ThreadControlMode::ReadOnly,
                    socket.clone(),
                    discovery.clone(),
                    cx,
                )
            })
            .unwrap();

        // Only its owner may read the token or use the socket.
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&discovery).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let discovery_contents: Value =
            serde_json::from_str(&std::fs::read_to_string(&discovery).unwrap()).unwrap();
        let token = discovery_contents["token"].as_str().unwrap().to_string();
        assert_eq!(discovery_contents["socket"], json!(socket));

        let (sender, receiver) = std::sync::mpsc::channel();
        let client_socket = socket.clone();
        std::thread::spawn(move || {
            let stream = std::os::unix::net::UnixStream::connect(client_socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut ask_line = |line: &str| -> Value {
                writeln!(writer, "{line}").unwrap();
                let mut response = String::new();
                reader.read_line(&mut response).unwrap();
                serde_json::from_str(&response).unwrap()
            };
            let bad_token =
                ask_line(&json!({ "token": "wrong", "method": "list_threads" }).to_string());
            let garbage = ask_line("not json");
            let threads =
                ask_line(&json!({ "token": token, "method": "list_threads" }).to_string());
            sender.send((bad_token, garbage, threads)).unwrap();
        });

        cx.executor().allow_parking();
        let (bad_token, garbage, threads) = loop {
            cx.run_until_parked();
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(responses) => break responses,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => panic!("client failed: {error}"),
            }
        };

        assert_eq!(bad_token["ok"], false);
        assert_eq!(bad_token["error"], "invalid token");
        assert_eq!(garbage["ok"], false);
        assert_eq!(threads["ok"], true);
        assert_eq!(threads["result"]["threads"][0]["title"], "alpha");

        drop(server);
        assert!(
            !socket.exists(),
            "the socket is removed when the server stops"
        );
        assert!(!discovery.exists(), "and so is the token file");
    }

    #[test]
    fn test_tokens_match_only_when_equal() {
        assert!(tokens_match("abc123", "abc123"));
        assert!(!tokens_match("abc123", "abc124"));
        assert!(!tokens_match("abc12", "abc123"));
        assert!(!tokens_match("", "abc123"));
    }

    #[test]
    fn test_truncate_keeps_short_text_and_cuts_long_text_on_a_boundary() {
        assert_eq!(truncate("short".to_string()), "short");

        let long = "é".repeat(MAX_ENTRY_CHARS);
        let cut = truncate(long);
        assert!(cut.ends_with("[truncated]"));
        assert!(cut.len() <= MAX_ENTRY_CHARS + "\n[truncated]".len());
    }
}
