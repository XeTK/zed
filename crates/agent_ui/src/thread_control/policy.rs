//! What other programs are allowed to do through the control server.

use agent_settings::{AgentSettings, ThreadControlPermissions};
use anyhow::{Result, anyhow, bail};
use gpui::{App, AsyncApp, PromptLevel};
use serde_json::{Value, json};
use settings::{Settings as _, ThreadControlMode, ThreadControlPermission};
use std::path::{Path, PathBuf};
use workspace::MultiWorkspace;

/// A thing that changes a thread. Each has its own permission setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    ClaimThread,
    SendMessage,
    CreateThread,
    CancelTurn,
    ArchiveThread,
    RenameThread,
}

impl Capability {
    pub const ALL: [Capability; 6] = [
        Capability::ClaimThread,
        Capability::SendMessage,
        Capability::CreateThread,
        Capability::CancelTurn,
        Capability::ArchiveThread,
        Capability::RenameThread,
    ];

    /// The name used both for the setting and for the tool offered to MCP clients.
    pub fn name(self) -> &'static str {
        match self {
            Capability::ClaimThread => "claim_thread",
            Capability::SendMessage => "send_message",
            Capability::CreateThread => "create_thread",
            Capability::CancelTurn => "cancel_turn",
            Capability::ArchiveThread => "archive_thread",
            Capability::RenameThread => "rename_thread",
        }
    }
}

/// The thread control settings as they stand right now.
#[derive(Clone, Debug)]
pub struct Policy {
    mode: ThreadControlMode,
    permissions: ThreadControlPermissions,
    projects: Vec<PathBuf>,
}

impl Policy {
    pub fn current(cx: &App) -> Self {
        let settings = AgentSettings::get_global(cx);
        Self::new(
            settings.thread_control,
            settings.thread_control_permissions,
            settings
                .thread_control_projects
                .iter()
                .map(PathBuf::from)
                .collect(),
        )
    }

    pub fn new(
        mode: ThreadControlMode,
        permissions: ThreadControlPermissions,
        projects: Vec<PathBuf>,
    ) -> Self {
        Self {
            mode,
            permissions,
            projects,
        }
    }

    /// What may be done for `capability`. Anything that changes a thread is
    /// refused unless the mode is `read_write`.
    pub fn permission(&self, capability: Capability) -> ThreadControlPermission {
        if self.mode != ThreadControlMode::ReadWrite {
            return ThreadControlPermission::Deny;
        }
        match capability {
            Capability::ClaimThread => self.permissions.claim_thread,
            Capability::SendMessage => self.permissions.send_message,
            Capability::CreateThread => self.permissions.create_thread,
            Capability::CancelTurn => self.permissions.cancel_turn,
            Capability::ArchiveThread => self.permissions.archive_thread,
            Capability::RenameThread => self.permissions.rename_thread,
        }
    }

    /// Whether anything under these folders may be listed, read or changed.
    /// With no folders configured, everything is in scope.
    pub fn includes<'a>(&self, folders: impl IntoIterator<Item = &'a Path>) -> bool {
        if self.projects.is_empty() {
            return true;
        }
        folders.into_iter().any(|folder| {
            self.projects
                .iter()
                .any(|project| folder.starts_with(project))
        })
    }

    pub fn describe(&self) -> Value {
        let permissions: serde_json::Map<String, Value> = Capability::ALL
            .iter()
            .map(|capability| {
                (
                    capability.name().to_string(),
                    json!(match self.permission(*capability) {
                        ThreadControlPermission::Deny => "deny",
                        ThreadControlPermission::Ask => "ask",
                        ThreadControlPermission::Allow => "allow",
                    }),
                )
            })
            .collect();
        json!({
            "mode": match self.mode {
                ThreadControlMode::Off => "off",
                ThreadControlMode::ReadOnly => "read_only",
                ThreadControlMode::ReadWrite => "read_write",
            },
            "permissions": permissions,
            "projects": self.projects,
        })
    }
}

/// For an action that is covered by a claim the person already approved: it
/// goes ahead unless the permission is `deny`, without asking again.
pub async fn ensure_not_denied(capability: Capability, cx: &mut AsyncApp) -> Result<()> {
    let permission = cx.update(|cx| Policy::current(cx).permission(capability));
    if permission == ThreadControlPermission::Deny {
        bail!(
            "{} is not allowed. Zed's agent.thread_control and agent.thread_control_permissions settings decide this.",
            capability.name()
        );
    }
    Ok(())
}

/// Decides whether `capability` may go ahead. `summary` says what is about to
/// happen, in words a person can judge, and is what an `ask` permission shows.
pub async fn authorize(capability: Capability, summary: String, cx: &mut AsyncApp) -> Result<()> {
    let permission = cx.update(|cx| Policy::current(cx).permission(capability));
    let outcome = match permission {
        ThreadControlPermission::Deny => Err(anyhow!(
            "{} is not allowed. Zed's agent.thread_control and agent.thread_control_permissions settings decide this.",
            capability.name()
        )),
        ThreadControlPermission::Allow => Ok(()),
        ThreadControlPermission::Ask => ask_the_user(capability, &summary, cx).await,
    };
    match &outcome {
        Ok(()) => log::info!("thread control: {} allowed: {summary}", capability.name()),
        Err(error) => log::info!(
            "thread control: {} refused ({error}): {summary}",
            capability.name()
        ),
    }
    outcome
}

async fn ask_the_user(capability: Capability, summary: &str, cx: &mut AsyncApp) -> Result<()> {
    let answer = cx.update(|cx| {
        let window = cx
            .active_window()
            .and_then(|window| window.downcast::<MultiWorkspace>())
            .or_else(|| {
                cx.windows()
                    .into_iter()
                    .find_map(|window| window.downcast::<MultiWorkspace>())
            });
        let window = window?;
        window
            .update(cx, |_, window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    "Another program wants to change a Zed thread",
                    Some(summary),
                    &["Allow", "Deny"],
                    cx,
                )
            })
            .ok()
    });
    let Some(answer) = answer else {
        bail!(
            "there is no Zed window to ask in, so {} was refused",
            capability.name()
        );
    };
    match answer.await {
        Ok(0) => Ok(()),
        _ => bail!("you denied {} in Zed", capability.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(
        mode: ThreadControlMode,
        send_message: ThreadControlPermission,
        projects: &[&str],
    ) -> Policy {
        Policy::new(
            mode,
            ThreadControlPermissions {
                send_message,
                ..Default::default()
            },
            projects.iter().map(PathBuf::from).collect(),
        )
    }

    #[test]
    fn test_nothing_changes_unless_the_mode_is_read_write() {
        for mode in [ThreadControlMode::Off, ThreadControlMode::ReadOnly] {
            let policy = policy(mode, ThreadControlPermission::Allow, &[]);
            for capability in Capability::ALL {
                assert_eq!(
                    policy.permission(capability),
                    ThreadControlPermission::Deny,
                    "{capability:?} under {mode:?}"
                );
            }
        }
    }

    #[test]
    fn test_each_capability_has_its_own_permission() {
        let policy = Policy::new(
            ThreadControlMode::ReadWrite,
            ThreadControlPermissions {
                claim_thread: ThreadControlPermission::Ask,
                send_message: ThreadControlPermission::Allow,
                create_thread: ThreadControlPermission::Deny,
                cancel_turn: ThreadControlPermission::Ask,
                archive_thread: ThreadControlPermission::Allow,
                rename_thread: ThreadControlPermission::Deny,
            },
            Vec::new(),
        );
        let granted: Vec<_> = Capability::ALL
            .iter()
            .map(|capability| (capability.name(), policy.permission(*capability)))
            .collect();
        assert_eq!(
            granted,
            [
                ("claim_thread", ThreadControlPermission::Ask),
                ("send_message", ThreadControlPermission::Allow),
                ("create_thread", ThreadControlPermission::Deny),
                ("cancel_turn", ThreadControlPermission::Ask),
                ("archive_thread", ThreadControlPermission::Allow),
                ("rename_thread", ThreadControlPermission::Deny),
            ]
        );
    }

    #[test]
    fn test_unset_permissions_ask() {
        assert_eq!(
            Policy::new(
                ThreadControlMode::ReadWrite,
                ThreadControlPermissions::default(),
                Vec::new()
            )
            .permission(Capability::SendMessage),
            ThreadControlPermission::Ask
        );
    }

    #[test]
    fn test_no_projects_means_everything_is_in_scope() {
        let policy = policy(
            ThreadControlMode::ReadOnly,
            ThreadControlPermission::Ask,
            &[],
        );
        assert!(policy.includes([Path::new("/anywhere/at/all")]));
        assert!(policy.includes(std::iter::empty::<&Path>()));
    }

    #[test]
    fn test_projects_limit_scope_to_folders_inside_them() {
        let policy = policy(
            ThreadControlMode::ReadOnly,
            ThreadControlPermission::Ask,
            &["/work/app", "/work/lib"],
        );
        assert!(policy.includes([Path::new("/work/app")]));
        assert!(policy.includes([Path::new("/work/app/crates/x")]));
        assert!(policy.includes([Path::new("/elsewhere"), Path::new("/work/lib")]));
        assert!(
            !policy.includes([Path::new("/work/application")]),
            "a longer name is not inside"
        );
        assert!(
            !policy.includes([Path::new("/work")]),
            "a parent is not inside"
        );
        assert!(!policy.includes([Path::new("/other")]));
        assert!(
            !policy.includes(std::iter::empty::<&Path>()),
            "a thread with no folders is not in scope"
        );
    }

    #[test]
    fn test_describe_reports_the_effective_permissions() {
        let described = policy(
            ThreadControlMode::ReadWrite,
            ThreadControlPermission::Allow,
            &["/work/app"],
        )
        .describe();
        assert_eq!(described["mode"], "read_write");
        assert_eq!(described["permissions"]["send_message"], "allow");
        assert_eq!(described["permissions"]["create_thread"], "ask");
        assert_eq!(described["projects"], json!(["/work/app"]));

        let read_only = policy(
            ThreadControlMode::ReadOnly,
            ThreadControlPermission::Allow,
            &[],
        )
        .describe();
        assert_eq!(read_only["permissions"]["send_message"], "deny");
    }
}
