//! Runs the real signing tool the way the release pipeline does: make a key,
//! sign a build, then check the feed the way a client would.

// These tests run real processes outside gpui.
#![allow(clippy::disallowed_methods)]

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn tool() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_update_feed"));
    command.env_remove("UPDATE_FEED_PRIVATE_KEY");
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

struct Release {
    directory: tempfile::TempDir,
    public_key: String,
    private_key_file: PathBuf,
    build: PathBuf,
}

impl Release {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let private_key_file = directory.path().join("private.key");
        let output = tool()
            .args(["keygen", "--private-key-out"])
            .arg(&private_key_file)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        let build = directory.path().join("Zed-xetk-nightly-7-aarch64.dmg");
        std::fs::write(&build, b"the build").unwrap();
        Self {
            public_key: stdout(&output),
            private_key_file,
            build,
            directory,
        }
    }

    fn feed(&self) -> PathBuf {
        self.directory.path().join("latest.json")
    }

    fn sign(&self, channel: &str) -> Output {
        tool()
            .args(["sign", "--private-key-file"])
            .arg(&self.private_key_file)
            .args([
                "--channel",
                channel,
                "--os",
                "macos",
                "--arch",
                "aarch64",
                "--version",
                "1.22.0+nightly.7.abc",
                "--url",
                "https://zed.example/nightly/macos-aarch64/Zed-xetk-nightly-7-aarch64.dmg",
                "--file",
            ])
            .arg(&self.build)
            .arg("--out")
            .arg(self.feed())
            .output()
            .unwrap()
    }

    fn verify(&self, channel: &str, with_file: Option<&Path>) -> Output {
        let mut command = tool();
        command
            .args(["verify", "--public-key", &self.public_key])
            .args(["--channel", channel, "--os", "macos", "--arch", "aarch64"]);
        if let Some(file) = with_file {
            command.arg("--file").arg(file);
        }
        command.arg(self.feed()).output().unwrap()
    }
}

#[test]
fn test_a_signed_feed_verifies_and_a_wrong_channel_or_file_does_not() {
    let release = Release::new();
    let signed = release.sign("nightly");
    assert!(signed.status.success(), "{}", stderr(&signed));
    assert!(
        std::fs::read_to_string(release.feed())
            .unwrap()
            .contains("\"signature\"")
    );

    let verified = release.verify("nightly", Some(&release.build));
    assert!(verified.status.success(), "{}", stderr(&verified));
    assert_eq!(stdout(&verified), "ok: nightly 1.22.0+nightly.7.abc");

    // A feed for one channel is not accepted as another's.
    assert!(!release.verify("stable", None).status.success());

    // A file that is not the one that was signed is caught.
    let other = release.directory.path().join("other.dmg");
    std::fs::write(&other, b"something else").unwrap();
    let mismatch = release.verify("nightly", Some(&other));
    assert!(!mismatch.status.success());

    // A feed edited after signing is caught.
    let tampered = std::fs::read_to_string(release.feed())
        .unwrap()
        .replace("1.22.0", "9.9.9");
    std::fs::write(release.feed(), tampered).unwrap();
    assert!(!release.verify("nightly", None).status.success());
}

#[test]
fn test_the_private_key_is_owner_only_and_never_overwritten() {
    let release = Release::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&release.private_key_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let before = std::fs::read(&release.private_key_file).unwrap();
    let again = tool()
        .args(["keygen", "--private-key-out"])
        .arg(&release.private_key_file)
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(
        stderr(&again).contains("failed to create"),
        "{}",
        stderr(&again)
    );
    assert_eq!(std::fs::read(&release.private_key_file).unwrap(), before);
}

#[test]
fn test_signing_takes_the_key_from_the_environment_and_stdout_and_says_when_it_has_none() {
    let release = Release::new();
    let key = std::fs::read_to_string(&release.private_key_file).unwrap();
    let arguments = [
        "sign",
        "--channel",
        "nightly",
        "--os",
        "macos",
        "--arch",
        "aarch64",
        "--version",
        "1.22.0+nightly.7.abc",
        "--url",
        "https://zed.example/x.dmg",
        "--file",
    ];

    let from_environment = tool()
        .env("UPDATE_FEED_PRIVATE_KEY", &key)
        .args(arguments)
        .arg(&release.build)
        .output()
        .unwrap();
    assert!(
        from_environment.status.success(),
        "{}",
        stderr(&from_environment)
    );
    assert!(
        stdout(&from_environment).contains("\"signature\""),
        "the feed goes to stdout"
    );

    let none = tool().args(arguments).arg(&release.build).output().unwrap();
    assert!(!none.status.success());
    assert!(
        stderr(&none).contains("UPDATE_FEED_PRIVATE_KEY"),
        "{}",
        stderr(&none)
    );

    let missing_file = tool()
        .args(["sign", "--private-key-file"])
        .arg(release.directory.path().join("nope.key"))
        .args(&arguments[1..])
        .arg(&release.build)
        .output()
        .unwrap();
    assert!(!missing_file.status.success());
    assert!(
        stderr(&missing_file).contains("failed to read"),
        "{}",
        stderr(&missing_file)
    );
}
