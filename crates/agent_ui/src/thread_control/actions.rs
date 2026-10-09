//! The things the control server can do to threads. Every one checks its
//! permission first, and an `ask` permission waits for you in Zed.

use super::policy::{Capability, Policy, authorize};
use super::{find_thread_metadata, live_conversation_views, truncate_for_prompt};
use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore};
use crate::{Agent, AgentInitialContent, AgentPanel, AgentThreadSource};
use acp_thread::ThreadStatus;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{AsyncApp, Entity, Task, WindowHandle};
use project::AgentId;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use workspace::MultiWorkspace;

const MAX_MESSAGE_CHARS: usize = 100_000;
const OPEN_THREAD_TIMEOUT: Duration = Duration::from_secs(20);

fn parse<T: for<'de> Deserialize<'de>>(params: Value, what: &str) -> Result<T> {
    serde_json::from_value(params).with_context(|| format!("{what} was given the wrong arguments"))
}

fn check_text(text: &str, what: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        bail!("{what} must not be empty");
    }
    if text.len() > MAX_MESSAGE_CHARS {
        bail!("{what} is too long (at most {MAX_MESSAGE_CHARS} characters)");
    }
    Ok(text.to_string())
}

fn describe_folders(metadata: &ThreadMetadata) -> String {
    metadata
        .folder_paths()
        .paths()
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Deserialize)]
struct SendMessageParams {
    id: String,
    text: String,
}

pub async fn send_message(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let params: SendMessageParams = parse(params, "send_message")?;
    let text = check_text(&params.text, "the message")?;
    let metadata = cx.update(|cx| find_thread_metadata(&params.id, cx))?;
    if metadata.is_draft() {
        bail!("this thread is a draft; use create_thread to start one");
    }

    authorize(
        Capability::SendMessage,
        format!(
            "Send a message to \"{}\" ({}):\n\n{}",
            metadata.display_title(),
            describe_folders(&metadata),
            truncate_for_prompt(&text)
        ),
        cx,
    )
    .await?;

    let (window, view) = open_thread_view(&metadata, cx).await?;
    window
        .update(cx, |_, window, cx| -> Result<()> {
            view.update(cx, |view, cx| {
                if view.thread.read(cx).status() != ThreadStatus::Idle {
                    bail!("the thread is busy; wait for it to finish or cancel it first");
                }
                let contents = vec![acp::ContentBlock::from(text.as_str())];
                view.send_content(
                    Task::ready(Ok(Some((contents, Vec::new())))),
                    false,
                    window,
                    cx,
                );
                Ok(())
            })
        })
        .map_err(|_| anyhow!("the window closed"))??;
    Ok(json!({ "sent": true, "id": params.id }))
}

/// The view of a thread, opening the thread in its project's agent panel first
/// if it is not already open.
async fn open_thread_view(
    metadata: &ThreadMetadata,
    cx: &mut AsyncApp,
) -> Result<(
    WindowHandle<MultiWorkspace>,
    Entity<crate::conversation_view::ThreadView>,
)> {
    let thread_id = metadata.thread_id;
    if let Some(found) = cx.update(|cx| find_open_thread_view(thread_id, cx)) {
        return Ok(found);
    }

    let opened = cx.update(|cx| -> Result<()> {
        for window in cx.windows() {
            let Some(window) = window.downcast::<MultiWorkspace>() else {
                continue;
            };
            let Ok(multi_workspace) = window.read(cx) else {
                continue;
            };
            let Some(workspace) = multi_workspace.workspace_for_paths(
                metadata.folder_paths(),
                metadata.remote_connection.as_ref(),
                cx,
            ) else {
                continue;
            };
            let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
                continue;
            };
            let agent = Agent::from(metadata.agent_id.clone());
            window.update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.load_agent_thread(
                        agent,
                        thread_id,
                        None,
                        None,
                        false,
                        AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    );
                });
            })?;
            return Ok(());
        }
        bail!(
            "the thread's project ({}) is not open in Zed, so it cannot be opened",
            describe_folders(metadata)
        )
    });
    opened?;

    let started = std::time::Instant::now();
    loop {
        if let Some(found) = cx.update(|cx| find_open_thread_view(thread_id, cx)) {
            return Ok(found);
        }
        if started.elapsed() > OPEN_THREAD_TIMEOUT {
            bail!("the thread did not finish opening in time");
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
}

fn find_open_thread_view(
    thread_id: ThreadId,
    cx: &gpui::App,
) -> Option<(
    WindowHandle<MultiWorkspace>,
    Entity<crate::conversation_view::ThreadView>,
)> {
    live_conversation_views(cx)
        .into_iter()
        .find(|(_, view)| view.read(cx).thread_id == thread_id)
        .and_then(|(window, view)| Some((window, view.read(cx).root_thread_view()?)))
}

#[derive(Deserialize)]
struct CreateThreadParams {
    /// A folder that is open in Zed.
    project: String,
    text: String,
    /// The id of the agent to use; the Zed agent when left out.
    agent: Option<String>,
}

pub async fn create_thread(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let params: CreateThreadParams = parse(params, "create_thread")?;
    let text = check_text(&params.text, "the message")?;
    let project = std::path::PathBuf::from(&params.project);
    if !cx.update(|cx| Policy::current(cx).includes([project.as_path()])) {
        bail!(
            "{} is outside the projects thread control is limited to",
            params.project
        );
    }

    authorize(
        Capability::CreateThread,
        format!(
            "Start a new thread in {}{}:\n\n{}",
            params.project,
            params
                .agent
                .as_deref()
                .map(|agent| format!(" with the agent \"{agent}\""))
                .unwrap_or_default(),
            truncate_for_prompt(&text)
        ),
        cx,
    )
    .await?;

    let agent = match params.agent {
        Some(agent) => Agent::from(AgentId::new(agent)),
        None => Agent::NativeAgent,
    };

    let thread_id = cx.update(|cx| -> Result<ThreadId> {
        for window in cx.windows() {
            let Some(window) = window.downcast::<MultiWorkspace>() else {
                continue;
            };
            let Ok(multi_workspace) = window.read(cx) else {
                continue;
            };
            let workspace = multi_workspace.workspaces().find(|workspace| {
                workspace
                    .read(cx)
                    .root_paths(cx)
                    .iter()
                    .any(|root| root.as_ref() == project.as_path())
            });
            let Some(panel) =
                workspace.and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
            else {
                continue;
            };
            let blocks = vec![acp::ContentBlock::from(text.as_str())];
            return window.update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.start_thread_with_content(
                        agent.clone(),
                        AgentInitialContent::ContentBlock {
                            blocks,
                            auto_submit: true,
                        },
                        window,
                        cx,
                    )
                })
            })?;
        }
        bail!("{} is not a project that is open in Zed", params.project)
    })?;
    Ok(json!({ "created": true, "id": thread_id.to_key_string() }))
}

#[derive(Deserialize)]
struct ThreadIdParams {
    id: String,
}

pub async fn cancel_turn(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let params: ThreadIdParams = parse(params, "cancel_turn")?;
    let metadata = cx.update(|cx| find_thread_metadata(&params.id, cx))?;
    let thread = cx
        .update(|cx| {
            live_conversation_views(cx)
                .into_iter()
                .find(|(_, view)| view.read(cx).thread_id == metadata.thread_id)
                .and_then(|(_, view)| view.read(cx).root_thread(cx))
        })
        .context("the thread is not open in Zed, so nothing is running in it")?;
    if cx.update(|cx| thread.read(cx).status()) == ThreadStatus::Idle {
        bail!("nothing is running in this thread");
    }

    authorize(
        Capability::CancelTurn,
        format!("Stop the agent running in \"{}\"", metadata.display_title()),
        cx,
    )
    .await?;

    let cancelled = thread.update(cx, |thread, cx| thread.cancel(cx));
    cancelled.await;
    Ok(json!({ "cancelled": true, "id": params.id }))
}

#[derive(Deserialize)]
struct ArchiveThreadParams {
    id: String,
    /// `false` restores the thread.
    #[serde(default = "archive_by_default")]
    archived: bool,
}

fn archive_by_default() -> bool {
    true
}

pub async fn archive_thread(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let params: ArchiveThreadParams = parse(params, "archive_thread")?;
    let metadata = cx.update(|cx| find_thread_metadata(&params.id, cx))?;

    authorize(
        Capability::ArchiveThread,
        format!(
            "{} the thread \"{}\"",
            if params.archived {
                "Archive"
            } else {
                "Restore"
            },
            metadata.display_title()
        ),
        cx,
    )
    .await?;

    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            if params.archived {
                store.archive(metadata.thread_id, None, cx);
            } else {
                store.unarchive(metadata.thread_id, cx);
            }
        });
    });
    Ok(json!({ "id": params.id, "archived": params.archived }))
}

#[derive(Deserialize)]
struct RenameThreadParams {
    id: String,
    title: String,
}

pub async fn rename_thread(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let params: RenameThreadParams = parse(params, "rename_thread")?;
    let title = check_text(&params.title, "the title")?;
    if title.len() > 200 || title.contains('\n') {
        bail!("the title must be one line of at most 200 characters");
    }
    let metadata = cx.update(|cx| find_thread_metadata(&params.id, cx))?;

    authorize(
        Capability::RenameThread,
        format!("Rename \"{}\" to \"{title}\"", metadata.display_title()),
        cx,
    )
    .await?;

    cx.update(|cx| {
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.set_title_override(metadata.thread_id, title.clone().into(), cx);
        });
    });
    Ok(json!({ "id": params.id, "title": title }))
}
