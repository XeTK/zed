//! Runs the real `remote_server` binary the way the editor starts a project's
//! own server, and checks that each one keeps everything in its own directory.

// These tests run outside gpui and need to start and wait on real processes.
#![allow(clippy::disallowed_methods)]

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Proxy {
    child: Child,
    identifier: String,
}

impl Proxy {
    fn start(data_dir: &Path, identifier: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_remote_server"))
            .env(paths::REMOTE_SERVER_DATA_DIR_ENV_VAR, data_dir)
            // The server loads the login shell's environment on startup, which
            // can take many seconds with a heavy shell profile.
            .env("SHELL", "/bin/sh")
            .arg("proxy")
            .arg("--identifier")
            .arg(identifier)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start the proxy");
        Self {
            child,
            identifier: identifier.to_string(),
        }
    }

    fn state_dir(&self, data_dir: &Path) -> PathBuf {
        paths::isolated_server_state_dir(data_dir).join(&self.identifier)
    }

    fn server_pid(&self, data_dir: &Path) -> Option<u32> {
        std::fs::read_to_string(self.state_dir(data_dir).join("server.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(45);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn kill(pid: u32) {
    Command::new("kill").arg(pid.to_string()).status().ok();
}

#[test]
fn test_each_project_server_keeps_its_state_in_its_own_directory() {
    let root = tempfile::tempdir().expect("temp dir");
    // The server canonicalizes its data directory (on macOS /var is /private/var),
    // and the short state directory is named after the canonical path.
    let root_path = root.path().canonicalize().expect("canonical temp dir");
    let first_dir = root_path.join("first");
    let second_dir = root_path.join("second");

    let first = Proxy::start(&first_dir, "project-one");
    let second = Proxy::start(&second_dir, "project-two");

    wait_for("both servers to be ready", || {
        first.state_dir(&first_dir).join("stdin.sock").exists()
            && second.state_dir(&second_dir).join("stdin.sock").exists()
    });

    let first_pid = first.server_pid(&first_dir).expect("first server pid");
    let second_pid = second.server_pid(&second_dir).expect("second server pid");
    assert_ne!(first_pid, second_pid, "each project has its own process");

    for (dir, other, identifier) in [
        (&first_dir, &second_dir, "project-one"),
        (&second_dir, &first_dir, "project-two"),
    ] {
        let state_dir = paths::isolated_server_state_dir(dir);
        for kind in ["stdin.sock", "stdout.sock", "stderr.sock", "server.pid"] {
            assert!(
                state_dir.join(identifier).join(kind).exists(),
                "{kind} for {identifier} is in its own directory"
            );
        }
        assert_ne!(state_dir, paths::isolated_server_state_dir(other));
        assert!(
            !paths::isolated_server_state_dir(other)
                .join(identifier)
                .exists(),
            "{identifier} left nothing in the other project's directory"
        );
        assert!(
            state_dir
                .join(identifier)
                .join("stdin.sock")
                .as_os_str()
                .len()
                < 100,
            "socket paths must stay short enough to bind"
        );
        assert!(
            dir.join("logs")
                .join(format!("server-{identifier}.log"))
                .exists(),
            "the log for {identifier} is in its own directory"
        );
    }

    kill(first_pid);
    kill(second_pid);
    for dir in [&first_dir, &second_dir] {
        std::fs::remove_dir_all(paths::isolated_server_state_dir(dir)).ok();
    }
}
