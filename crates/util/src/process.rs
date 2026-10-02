use anyhow::{Context as _, Result};
use std::process::Stdio;

/// A wrapper around `smol::process::Child` that ensures all subprocesses
/// are killed when the process is terminated: on Unix by using process
/// groups, and on Windows by using job objects.
///
/// On Unix, dropping this struct kills the child's process group, plus (on
/// macOS) any descendants that moved to a different group or session, and
/// [`kill_all_process_groups`] does the same for every live child at once
/// (e.g. when the app quits, when no `Drop` would otherwise run in time).
/// Nothing can run when Zed is killed outright, so unlike on Windows a crash
/// can still leave children running.
///
/// On Windows, dropping this struct closes the job object handle, which
/// terminates all processes in the job. This also applies when the Zed
/// process exits for any reason (including crashes), since the OS closes
/// its handles, so spawned process trees can never outlive Zed.
pub struct Child {
    process: smol::process::Child,
    #[cfg(windows)]
    job: Option<windows_job::JobObject>,
    // Declared after `process` so it runs after it when dropped. A separate
    // guard (rather than `Drop for Child`) keeps `output(self)` able to move
    // `process` out.
    #[cfg(not(windows))]
    _process_tree_guard: process_tree::Guard,
}

impl std::ops::Deref for Child {
    type Target = smol::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}

impl Child {
    #[cfg(not(windows))]
    pub fn spawn(
        mut command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        crate::set_pre_exec_to_start_new_session(&mut command);
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;
        let _process_tree_guard = process_tree::Guard::new(process.id());
        Ok(Self {
            process,
            _process_tree_guard,
        })
    }

    #[cfg(windows)]
    pub fn spawn(
        command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;

        // Assign the child to a job object configured to kill the entire
        // process tree when the last job handle is closed, so descendants
        // (e.g. node workers and MCP servers spawned by agent servers) are
        // reaped even if the direct child doesn't clean them up. Any process
        // the child spawns after this assignment is automatically part of the
        // job.
        //
        // There is a small race: descendants the child spawns between the
        // `spawn()` call returning and the assignment below escape the job.
        // Closing it fully would require creating the process suspended
        // (`CREATE_SUSPENDED`), assigning it, then resuming it, which the
        // std/smol process APIs don't support without reimplementing process
        // creation. The window is microseconds, and the children we care
        // about (`npx`, `node`, etc.) take far longer to load their runtime
        // and spawn anything, so in practice nothing escapes.
        let job = windows_job::JobObject::new()
            .and_then(|job| {
                job.assign_process(process.id())?;
                Ok(job)
            })
            .map_err(|error| {
                log::error!("failed to assign spawned process to a job object: {error:#}");
            })
            .ok();

        Ok(Self { process, job })
    }

    /// Consumes the child, draining its stdout/stderr and waiting for it to
    /// exit, then returns the collected output.
    pub async fn output(self) -> Result<std::process::Output> {
        // NOTE: Keep `self` alive across this await, do not destructure it to
        // pull `process` out first. On Windows that drops the job object early,
        // which triggers `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and kills the
        // child before `output()` finishes collecting its stdout/stderr.
        Ok(self.process.output().await?)
    }

    #[cfg(not(windows))]
    pub fn kill(&mut self) -> Result<()> {
        process_tree::kill_tree(self.process.id() as i32);
        Ok(())
    }

    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<()> {
        if let Some(job) = &self.job {
            job.terminate()
        } else {
            self.process.kill()?;
            Ok(())
        }
    }
}

/// Kills every process tree started through [`Child::spawn`] that hasn't been
/// dropped yet. Meant to be called when the app is quitting. A no-op on
/// Windows, where the job objects are closed by the OS when Zed exits.
pub fn kill_all_process_groups() {
    #[cfg(not(windows))]
    process_tree::kill_all();
}

#[cfg(not(windows))]
mod process_tree {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    static LIVE_PROCESS_GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

    fn live_process_groups() -> MutexGuard<'static, Vec<i32>> {
        LIVE_PROCESS_GROUPS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a spawned child's process group, and kills its whole tree
    /// when dropped.
    pub(super) struct Guard {
        pid: i32,
    }

    impl Guard {
        pub(super) fn new(pid: u32) -> Self {
            let pid = pid as i32;
            live_process_groups().push(pid);
            Self { pid }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            live_process_groups().retain(|pid| *pid != self.pid);
            kill_tree(self.pid);
        }
    }

    pub(super) fn kill_all() {
        let pids = live_process_groups().clone();
        for pid in pids {
            kill_tree(pid);
        }
    }

    /// Kills the process group led by `pid`, and descendants of `pid` that left
    /// the group (e.g. by calling `setsid`) where they can be found.
    ///
    /// Descendants are looked up first: once their parent dies they are
    /// reparented to init and can no longer be traced back to it.
    pub(super) fn kill_tree(pid: i32) {
        // Only trust `pid` to still be our child while it exists; a leader
        // that already exited has nothing left to walk from, and its pid may
        // by now belong to something unrelated.
        let leader_exists = unsafe { libc::kill(pid, 0) } == 0;
        let descendants = if leader_exists {
            descendant_pids(pid)
        } else {
            Vec::new()
        };
        for descendant in descendants {
            unsafe {
                libc::kill(descendant, libc::SIGKILL);
            }
        }
        // Returns ESRCH, harmlessly, when the group is already empty.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }

    #[cfg(target_os = "macos")]
    fn child_pids(parent: i32) -> Vec<i32> {
        const PROC_PPID_ONLY: u32 = 6;
        let pid_size = size_of::<i32>();

        // A null buffer asks for the size needed, in bytes.
        let needed =
            unsafe { libc::proc_listpids(PROC_PPID_ONLY, parent as u32, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Vec::new();
        }
        // Children can be added between the two calls, so leave some slack.
        let capacity = needed as usize / pid_size + 16;
        let mut pids = vec![0i32; capacity];
        let written = unsafe {
            libc::proc_listpids(
                PROC_PPID_ONLY,
                parent as u32,
                pids.as_mut_ptr().cast(),
                (capacity * pid_size) as i32,
            )
        };
        if written <= 0 {
            return Vec::new();
        }
        pids.truncate(written as usize / pid_size);
        pids.retain(|pid| *pid > 0);
        pids
    }

    #[cfg(target_os = "macos")]
    fn descendant_pids(root: i32) -> Vec<i32> {
        let mut descendants = Vec::new();
        let mut pending = vec![root];
        while let Some(parent) = pending.pop() {
            for child in child_pids(parent) {
                if child != root && !descendants.contains(&child) {
                    descendants.push(child);
                    pending.push(child);
                }
            }
        }
        descendants
    }

    // Other platforms only get the process group kill.
    #[cfg(not(target_os = "macos"))]
    fn descendant_pids(_root: i32) -> Vec<i32> {
        Vec::new()
    }
}

#[cfg(windows)]
mod windows_job {
    use crate::ResultExt as _;
    use anyhow::{Context as _, Result};
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
        },
    };

    /// A Win32 job object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:
    /// all processes assigned to the job (and their descendants) are terminated
    /// when the last handle to the job is closed, which happens when this struct
    /// is dropped, or when the OS closes the owning process's handles after it
    /// exits for any reason.
    pub(crate) struct JobObject(HANDLE);

    // SAFETY: Job object handles can be used from any thread.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        pub(crate) fn new() -> Result<Self> {
            unsafe {
                let job =
                    Self(CreateJobObjectW(None, None).context("failed to create job object")?);
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .context("failed to set job object limits")?;
                Ok(job)
            }
        }

        pub(crate) fn assign_process(&self, pid: u32) -> Result<()> {
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                    .context("failed to open process")?;
                let result = AssignProcessToJobObject(self.0, process)
                    .context("failed to assign process to job object");
                CloseHandle(process).log_err();
                result
            }
        }

        pub(crate) fn terminate(&self) -> Result<()> {
            unsafe { TerminateJobObject(self.0, 1).context("failed to terminate job object") }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0).log_err();
            }
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Spawns a process tree `powershell -> ping` via `Child::spawn` and
    /// returns the `Child` along with the pid of the grandchild (`ping`).
    fn spawn_process_tree(temp_dir: &std::path::Path) -> (Child, u32) {
        let pid_file = temp_dir.join("grandchild_pid");
        let mut command = std::process::Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command"]).arg(format!(
            "$p = Start-Process -FilePath ping.exe -ArgumentList @('-n','60','127.0.0.1') -PassThru -WindowStyle Hidden; \
             Set-Content -LiteralPath '{}' -Value $p.Id; \
             Wait-Process -Id $p.Id",
            pid_file.display()
        ));
        let child = Child::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
            .expect("failed to spawn powershell");

        let deadline = Instant::now() + Duration::from_secs(5);
        let grandchild_pid = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for grandchild pid file"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            process_is_alive(grandchild_pid),
            "grandchild should be alive after spawning"
        );
        (child, grandchild_pid)
    }

    fn process_is_alive(pid: u32) -> bool {
        use windows::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut exit_code = 0u32;
            let alive = GetExitCodeProcess(handle, &mut exit_code).is_ok()
                && exit_code == STILL_ACTIVE.0 as u32;
            CloseHandle(handle).expect("failed to close process handle");
            alive
        }
    }

    fn assert_process_exits(pid: u32, message: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_alive(pid) {
            assert!(Instant::now() < deadline, "{message} (pid {pid})");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn test_kill_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (mut child, grandchild_pid) = spawn_process_tree(temp_dir.path());

        child.kill().expect("failed to kill child");

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after killing the child",
        );
    }

    #[test]
    fn test_drop_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (child, grandchild_pid) = spawn_process_tree(temp_dir.path());

        drop(child);

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after dropping the child",
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use smol::io::{AsyncBufReadExt as _, BufReader};
    use std::time::{Duration, Instant};

    fn process_is_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn wait_until_dead(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !process_is_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Spawns `script` under `sh` and returns the child plus the pid it prints
    /// on its first line of stdout (the process the test cares about).
    fn spawn_script(script: &str) -> (Child, i32) {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", script]);
        let mut child =
            Child::spawn(command, Stdio::null(), Stdio::piped(), Stdio::null()).unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();
        smol::block_on(BufReader::new(stdout).read_line(&mut line)).unwrap();
        let pid = line.trim().parse().unwrap();
        (child, pid)
    }

    #[test]
    fn test_dropping_child_kills_background_process_in_its_group() {
        let (child, grandchild) = spawn_script("sleep 300 & echo $!; wait");
        assert!(process_is_alive(grandchild));

        drop(child);

        let killed = wait_until_dead(grandchild);
        unsafe {
            libc::kill(grandchild, libc::SIGKILL);
        }
        assert!(killed, "the background process should die with its parent");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_dropping_child_kills_descendant_that_left_its_process_group() {
        // `perl` makes itself a session leader, then becomes `sleep`, so the
        // pid printed by the shell is a descendant in a different group.
        let (child, grandchild) = spawn_script(
            "perl -e 'use POSIX; POSIX::setsid(); exec q(sleep), q(300)' & echo $!; wait",
        );
        // Give perl a moment to call setsid before we check it escaped.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && unsafe { libc::getpgid(grandchild) } == unsafe { libc::getpgid(child.id() as i32) }
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let escaped = unsafe { libc::getpgid(grandchild) } != child.id() as i32;

        drop(child);

        let killed = wait_until_dead(grandchild);
        unsafe {
            libc::kill(grandchild, libc::SIGKILL);
        }
        assert!(escaped, "the test process should have left the group");
        assert!(
            killed,
            "a descendant that left the group should still be killed"
        );
    }
}
