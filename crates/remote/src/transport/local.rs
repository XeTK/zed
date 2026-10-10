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
        let server_binary = bundled_server_binary()?;
        let data_dir = options.data_dir();
        Self::new_with(options, server_binary, data_dir, delegate, cx).await
    }

    async fn new_with(
        options: LocalProcessConnectionOptions,
        server_binary: PathBuf,
        data_dir: PathBuf,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        delegate.set_status(Some("Starting project process"), cx);

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
    server_binary_beside(&executable)
}

fn server_binary_beside(executable: &Path) -> Result<PathBuf> {
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
    use crate::transport::mock::MockDelegate;
    use futures::channel::mpsc;
    use gpui::TestAppContext;
    use std::os::unix::fs::PermissionsExt as _;
    use util::paths::RemotePathBuf;

    fn delegate() -> Arc<dyn RemoteClientDelegate> {
        Arc::new(MockDelegate)
    }

    async fn connection(
        directory: &Path,
        server_binary: &Path,
        cx: &mut TestAppContext,
    ) -> LocalProcessConnection {
        // Making the directory and running processes is real I/O.
        cx.executor().allow_parking();
        let options = LocalProcessConnectionOptions::for_paths([Path::new("/work/app")]);
        let mut async_cx = cx.to_async();
        LocalProcessConnection::new_with(
            options,
            server_binary.to_path_buf(),
            directory.join("data"),
            delegate(),
            &mut async_cx,
        )
        .await
        .unwrap()
    }

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn test_the_server_binary_is_found_beside_the_executable() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("zed");
        let error = server_binary_beside(&executable).unwrap_err();
        assert!(error.to_string().contains("remote_server"), "{error}");

        std::fs::write(directory.path().join("remote_server"), "").unwrap();
        assert_eq!(
            server_binary_beside(&executable).unwrap(),
            directory.path().join("remote_server")
        );
        assert!(
            server_binary_beside(Path::new("/")).is_err(),
            "no directory to look in"
        );
    }

    #[gpui::test]
    async fn test_starting_without_a_bundled_server_says_where_it_looked(cx: &mut TestAppContext) {
        let options = LocalProcessConnectionOptions::for_paths([Path::new("/work/app")]);
        let mut async_cx = cx.to_async();
        // Tests run from a build directory that has no remote_server beside them.
        let error = match LocalProcessConnection::new(options, delegate(), &mut async_cx).await {
            Ok(_) => panic!("there is no bundled server to start"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("was not found"), "{error}");
    }

    #[gpui::test]
    async fn test_a_connection_makes_its_data_directory_and_describes_itself(
        cx: &mut TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let connection = connection(directory.path(), Path::new("/bin/true"), cx).await;

        assert!(
            directory.path().join("data").is_dir(),
            "its own data directory exists"
        );
        assert_eq!(
            connection.connection_options(),
            RemoteConnectionOptions::LocalProcess(LocalProcessConnectionOptions::for_paths([
                Path::new("/work/app")
            ]))
        );
        assert_eq!(connection.path_style(), PathStyle::local());
        let platform = connection.remote_platform();
        assert!(platform.os == local_platform().os && platform.arch == local_platform().arch);
        assert!(connection.remote_os_version().is_none());
        assert!(!connection.shell().is_empty());
        assert_eq!(connection.default_system_shell(), connection.shell());
        assert!(!connection.has_wsl_interop());
        assert!(connection.shares_network_interface());
        assert!(!connection.has_been_killed());
        connection.kill().await.unwrap();
        assert!(
            !connection.has_been_killed(),
            "killing it does not stop the server"
        );
        assert!(
            connection
                .build_forward_ports_command(vec![(1, "localhost".into(), 2)])
                .is_err()
        );
    }

    #[test]
    fn test_the_local_platform_matches_this_machine() {
        let platform = local_platform();
        assert_eq!(platform.os == RemoteOs::MacOs, cfg!(target_os = "macos"));
        assert_eq!(platform.os == RemoteOs::Linux, cfg!(target_os = "linux"));
        assert_eq!(
            platform.arch == RemoteArch::Aarch64,
            cfg!(target_arch = "aarch64")
        );
    }

    #[gpui::test]
    #[allow(clippy::disallowed_methods)] // runs the command it built, outside gpui
    async fn test_a_built_command_runs_in_the_directory_with_the_environment_and_quoting(
        cx: &mut TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let connection = connection(directory.path(), Path::new("/bin/true"), cx).await;
        let working_dir = directory.path().canonicalize().unwrap();

        let template = connection
            .build_command(
                Some("sh".to_string()),
                &[
                    "-c".to_string(),
                    "printf '%s|%s|%s' \"$FOO\" \"$PWD\" \"$1\"".to_string(),
                    "ignored".to_string(),
                    "it's a \"quoted\" arg".to_string(),
                ],
                &HashMap::from_iter([("FOO".to_string(), "two words".to_string())]),
                Some(working_dir.to_string_lossy().into_owned()),
                None,
                Interactive::No,
            )
            .unwrap();
        let output = std::process::Command::new(&template.program)
            .args(&template.args)
            .envs(&template.env)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("two words|{}|it's a \"quoted\" arg", working_dir.display())
        );
    }

    #[gpui::test]
    async fn test_build_command_without_a_program_starts_a_login_shell_and_refuses_port_forwards(
        cx: &mut TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let connection = connection(directory.path(), Path::new("/bin/true"), cx).await;

        let template = connection
            .build_command(None, &[], &HashMap::default(), None, None, Interactive::Yes)
            .unwrap();
        let command = template.args.join(" ");
        assert!(command.contains("exec env"), "{command}");
        assert!(command.contains(" -l"), "a login shell: {command}");
        assert!(
            !command.contains("cd "),
            "no directory was asked for: {command}"
        );

        let error = connection
            .build_command(
                Some("ls".to_string()),
                &[],
                &HashMap::default(),
                None,
                Some((8080, "localhost".to_string(), 80)),
                Interactive::No,
            )
            .unwrap_err();
        assert!(error.to_string().contains("network"), "{error}");
    }

    #[gpui::test]
    async fn test_upload_directory_copies_a_tree_and_reports_a_failure(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let connection = connection(directory.path(), Path::new("/bin/true"), cx).await;
        let source = directory.path().join("source");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("nested/file.txt"), "contents").unwrap();
        let destination = directory.path().join("copy");

        cx.executor().allow_parking();
        let task = cx.update(|cx| {
            connection.upload_directory(
                source.clone(),
                RemotePathBuf::new(
                    destination.to_string_lossy().into_owned(),
                    PathStyle::local(),
                ),
                cx,
            )
        });
        task.await.unwrap();
        assert_eq!(
            std::fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "contents"
        );

        let task = cx.update(|cx| {
            connection.upload_directory(
                directory.path().join("missing"),
                RemotePathBuf::new(
                    destination.to_string_lossy().into_owned(),
                    PathStyle::local(),
                ),
                cx,
            )
        });
        let error = task.await.unwrap_err();
        assert!(error.to_string().contains("failed to copy"), "{error}");
    }

    #[gpui::test]
    async fn test_the_proxy_is_started_with_the_identifier_and_the_servers_own_data_directory(
        cx: &mut TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join("record");
        std::fs::create_dir_all(&record).unwrap();
        let server = directory.path().join("fake_remote_server");
        write_script(
            &server,
            &format!(
                "printf '%s\\n' \"$@\" > '{record}/args'\nprintf '%s' \"$ZED_REMOTE_SERVER_DATA_DIR\" > '{record}/data_dir'\ncat > /dev/null",
                record = record.display()
            ),
        );
        let connection = connection(directory.path(), &server, cx).await;
        let data_dir = directory.path().join("data");

        for (reconnect, expected) in [
            (false, "proxy\n--identifier\nworkspace-7\n"),
            (true, "proxy\n--identifier\nworkspace-7\n--reconnect\n"),
        ] {
            let (incoming_tx, _incoming_rx) = mpsc::unbounded();
            let (outgoing_tx, outgoing_rx) = mpsc::unbounded();
            let (activity_tx, _activity_rx) = mpsc::channel(8);
            let mut async_cx = cx.to_async();
            cx.executor().allow_parking();
            let task = connection.start_proxy(
                "workspace-7".to_string(),
                reconnect,
                incoming_tx,
                outgoing_rx,
                activity_tx,
                delegate(),
                &mut async_cx,
            );
            // Closing our end of the pipe ends the fake server, like Zed quitting.
            drop(outgoing_tx);
            let exit_code = task.await.unwrap();
            assert_eq!(exit_code, 0);
            assert_eq!(
                std::fs::read_to_string(record.join("args")).unwrap(),
                expected
            );
            assert_eq!(
                std::fs::read_to_string(record.join("data_dir")).unwrap(),
                data_dir.to_string_lossy()
            );
        }
    }

    #[gpui::test]
    async fn test_a_missing_server_binary_fails_to_start_the_proxy(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let connection =
            connection(directory.path(), &directory.path().join("not_there"), cx).await;
        let (incoming_tx, _incoming_rx) = mpsc::unbounded();
        let (_outgoing_tx, outgoing_rx) = mpsc::unbounded();
        let (activity_tx, _activity_rx) = mpsc::channel(8);
        let mut async_cx = cx.to_async();
        let error = connection
            .start_proxy(
                "workspace-1".to_string(),
                false,
                incoming_tx,
                outgoing_rx,
                activity_tx,
                delegate(),
                &mut async_cx,
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to start the project process"),
            "{error}"
        );
    }

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
