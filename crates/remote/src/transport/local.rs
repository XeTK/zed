use crate::{
    RemoteArch, RemoteClientDelegate, RemoteOs, RemotePlatform,
    remote_client::{CommandTemplate, Interactive, RemoteConnection, RemoteConnectionOptions},
};
use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use collections::HashMap;
use futures::channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender};
use gpui::{App, AppContext as _, AsyncApp, Task};
use rpc::proto::Envelope;
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::{
    command::Stdio,
    paths::{PathStyle, RemotePathBuf},
    shell::{Shell, ShellKind},
    shell_builder::ShellBuilder,
};

/// A project whose backend runs in a `remote_server` process on this machine,
/// instead of in the editor's own process. Each one has its own data directory,
/// named by `id`, so two of them never share logs, databases or server state.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct LocalProcessConnectionOptions {
    pub id: String,
}

impl LocalProcessConnectionOptions {
    /// The options for the project made of `paths`. The same folders always give
    /// the same id, so a project keeps its data directory across restarts.
    pub fn for_paths<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Self {
        let mut paths: Vec<&Path> = paths.into_iter().collect();
        paths.sort();

        // FNV-1a, written out because `DefaultHasher` makes no promise to give
        // the same answer in another release, which would orphan the data.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for path in &paths {
            for byte in path.to_string_lossy().bytes().chain([0]) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }

        let name: String = paths
            .first()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' {
                    character
                } else {
                    '_'
                }
            })
            .take(32)
            .collect();

        Self {
            id: if name.is_empty() {
                format!("{hash:016x}")
            } else {
                format!("{name}-{hash:016x}")
            },
        }
    }

    /// Where this project's server keeps everything it writes.
    pub fn data_dir(&self) -> PathBuf {
        paths::data_dir().join("isolated_projects").join(&self.id)
    }
}

pub(crate) struct LocalProcessConnection {
    options: LocalProcessConnectionOptions,
    server_binary: PathBuf,
    data_dir: PathBuf,
    shell: String,
    shell_kind: ShellKind,
}

impl LocalProcessConnection {
    pub(crate) async fn new(
        options: LocalProcessConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        delegate.set_status(Some("Starting project process"), cx);

        let server_binary = bundled_server_binary()?;
        let data_dir = options.data_dir();
        smol::fs::create_dir_all(&data_dir)
            .await
            .with_context(|| format!("creating {}", data_dir.display()))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        let shell_kind = ShellKind::new(&shell, false);
        Ok(Self {
            options,
            server_binary,
            data_dir,
            shell,
            shell_kind,
        })
    }
}

/// The `remote_server` binary that ships next to the editor's own executable.
fn bundled_server_binary() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("finding the editor's executable")?;
    let directory = executable
        .parent()
        .context("the editor's executable has no directory")?;
    let name = if cfg!(windows) {
        "remote_server.exe"
    } else {
        "remote_server"
    };
    let binary = directory.join(name);
    if !binary.is_file() {
        bail!(
            "the project process binary was not found at {}",
            binary.display()
        );
    }
    Ok(binary)
}

fn local_platform() -> RemotePlatform {
    RemotePlatform {
        os: if cfg!(target_os = "macos") {
            RemoteOs::MacOs
        } else if cfg!(windows) {
            RemoteOs::Windows
        } else {
            RemoteOs::Linux
        },
        arch: if cfg!(target_arch = "aarch64") {
            RemoteArch::Aarch64
        } else {
            RemoteArch::X86_64
        },
    }
}

#[async_trait(?Send)]
impl RemoteConnection for LocalProcessConnection {
    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        outgoing_rx: UnboundedReceiver<Envelope>,
        connection_activity_tx: Sender<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        delegate.set_status(Some("Starting proxy"), cx);

        let mut command = util::command::new_command(&self.server_binary);
        command
            .env(paths::REMOTE_SERVER_DATA_DIR_ENV_VAR, &self.data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .arg("proxy")
            .arg("--identifier")
            .arg(unique_identifier);
        if reconnect {
            command.arg("--reconnect");
        }
        command.kill_on_drop(true);

        let proxy_process = match command.spawn() {
            Ok(process) => process,
            Err(error) => {
                return Task::ready(Err(
                    anyhow::Error::new(error).context("failed to start the project process")
                ));
            }
        };

        super::handle_rpc_messages_over_child_process_stdio(
            proxy_process,
            incoming_tx,
            outgoing_rx,
            connection_activity_tx,
            cx,
        )
    }

    fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        cx.background_spawn(async move {
            let mut command = util::command::new_command("cp");
            command
                .arg("-R")
                .arg(&src_path)
                .arg(dest_path.to_string())
                .kill_on_drop(true);
            let output = command
                .output()
                .await
                .with_context(|| format!("copying {} to {}", src_path.display(), dest_path))?;
            if !output.status.success() {
                bail!(
                    "failed to copy {} to {}: {}",
                    src_path.display(),
                    dest_path,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(())
        })
    }

    async fn kill(&self) -> Result<()> {
        Ok(())
    }

    fn has_been_killed(&self) -> bool {
        false
    }

    fn shares_network_interface(&self) -> bool {
        true
    }

    fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        _interactive: Interactive,
    ) -> Result<CommandTemplate> {
        if port_forward.is_some() {
            bail!("a project process shares the network interface with the editor");
        }

        let shell_kind = self.shell_kind;
        let mut exec = String::new();
        if let Some(working_dir) = working_dir {
            let working_dir = shell_kind
                .try_quote(&working_dir)
                .context("shell quoting")?;
            write!(exec, "cd {working_dir} && ")?;
        }
        exec.push_str("exec env ");
        for (key, value) in env {
            let assignment = format!("{key}={value}");
            let assignment = shell_kind.try_quote(&assignment).context("shell quoting")?;
            write!(exec, "{assignment} ")?;
        }
        if let Some(program) = program {
            write!(
                exec,
                "{}",
                shell_kind
                    .try_quote_prefix_aware(&program)
                    .context("shell quoting")?
            )?;
            for arg in args {
                let arg = shell_kind.try_quote(arg).context("shell quoting")?;
                write!(exec, " {arg}")?;
            }
        } else {
            write!(exec, "{} -l", self.shell)?;
        }

        let (program, args) =
            ShellBuilder::new(&Shell::Program(self.shell.clone()), false).build(Some(exec), &[]);
        Ok(CommandTemplate {
            program,
            args,
            env: HashMap::default(),
        })
    }

    fn build_forward_ports_command(&self, _: Vec<(u16, String, u16)>) -> Result<CommandTemplate> {
        Err(anyhow!(
            "a project process shares the network interface with the editor"
        ))
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::LocalProcess(self.options.clone())
    }

    fn path_style(&self) -> PathStyle {
        PathStyle::local()
    }

    fn remote_platform(&self) -> RemotePlatform {
        local_platform()
    }

    fn remote_os_version(&self) -> Option<String> {
        None
    }

    fn shell(&self) -> String {
        self.shell.clone()
    }

    fn default_system_shell(&self) -> String {
        self.shell.clone()
    }

    fn has_wsl_interop(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_same_folders_always_get_the_same_id() {
        let first = LocalProcessConnectionOptions::for_paths([Path::new("/work/app")]);
        let second = LocalProcessConnectionOptions::for_paths([Path::new("/work/app")]);
        assert_eq!(first, second);
        assert!(first.id.starts_with("app-"), "{}", first.id);
    }

    #[test]
    fn test_different_projects_never_share_an_id() {
        let app = LocalProcessConnectionOptions::for_paths([Path::new("/work/app")]);
        let other_app = LocalProcessConnectionOptions::for_paths([Path::new("/other/app")]);
        let both = LocalProcessConnectionOptions::for_paths([
            Path::new("/work/app"),
            Path::new("/work/lib"),
        ]);
        assert_ne!(app, other_app, "same folder name, different place");
        assert_ne!(app, both);
        assert_ne!(app.data_dir(), other_app.data_dir());
    }

    #[test]
    fn test_the_order_of_the_folders_does_not_matter() {
        let one = LocalProcessConnectionOptions::for_paths([
            Path::new("/work/app"),
            Path::new("/work/lib"),
        ]);
        let two = LocalProcessConnectionOptions::for_paths([
            Path::new("/work/lib"),
            Path::new("/work/app"),
        ]);
        assert_eq!(one, two);
    }

    #[test]
    fn test_the_id_is_safe_to_use_as_a_directory_name() {
        let options = LocalProcessConnectionOptions::for_paths([Path::new("/work/my app/../x:y?")]);
        assert!(
            options
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{}",
            options.id
        );
    }

    #[test]
    fn test_the_data_dir_is_inside_the_isolated_projects_directory() {
        let options = LocalProcessConnectionOptions::for_paths([Path::new("/work/app")]);
        assert!(options.data_dir().ends_with(&options.id));
        assert_eq!(
            options.data_dir().parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("isolated_projects"))
        );
    }
}
