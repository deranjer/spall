use spall_core::{ProcessRole, log_has_ready_record};
use std::{
    path::Path,
    process::{Child, Command, ExitStatus},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Debug)]
pub struct CompletedChild {
    pub pid: u32,
}

#[derive(Debug, Error)]
pub enum ProcessFailure {
    #[error("cannot start child process: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("child {pid} exited before reporting readiness ({status})")]
    ExitedBeforeReady { pid: u32, status: ExitStatus },
    #[error("child {pid} exited after readiness with {status}")]
    ExitedFailed { pid: u32, status: ExitStatus },
    #[error("child {pid} exceeded bounded run timeout of {timeout:?}")]
    TimedOut { pid: u32, timeout: Duration },
    #[error("cannot read child readiness log: {0}")]
    Readiness(std::io::Error),
    #[error("child {pid} was cancelled and terminated")]
    Cancelled { pid: u32 },
    #[error("cannot install cancellation handler: {0}")]
    CancelHandler(String),
}

impl ProcessFailure {
    pub fn pid(&self) -> Option<u32> {
        match self {
            Self::ExitedBeforeReady { pid, .. }
            | Self::ExitedFailed { pid, .. }
            | Self::TimedOut { pid, .. }
            | Self::Cancelled { pid } => Some(*pid),
            Self::Spawn(_) | Self::Readiness(_) | Self::CancelHandler(_) => None,
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::ExitedBeforeReady { status, .. } | Self::ExitedFailed { status, .. } => {
                status.code()
            }
            _ => None,
        }
    }
}

static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);
static CANCEL_HANDLER: OnceLock<Result<(), String>> = OnceLock::new();

/// Run a child to completion under a bounded timeout, killing it (and reaping
/// it) on timeout or Ctrl-C. Used for short one-shot tools that do not emit a
/// readiness record, such as the T05 renderer capture.
pub fn run_bounded(mut command: Command, timeout: Duration) -> Result<ExitStatus, ProcessFailure> {
    install_cancel_handler()?;
    CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    hide_console(&mut command);
    let child = command.spawn()?;
    let pid = child.id();
    let mut child = ChildCleanup(Some(child));
    let deadline = Instant::now() + timeout;
    loop {
        if CANCEL_REQUESTED.load(Ordering::SeqCst) {
            return Err(ProcessFailure::Cancelled { pid });
        }
        if let Some(status) = child.0.as_mut().expect("child present").try_wait()? {
            child.0.take();
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(ProcessFailure::TimedOut { pid, timeout });
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn wait_for_server(
    command: Command,
    log: &Path,
    timeout: Duration,
) -> Result<CompletedChild, ProcessFailure> {
    wait_for_role(command, log, timeout, ProcessRole::Server)
}

pub fn wait_for_client(
    command: Command,
    log: &Path,
    timeout: Duration,
) -> Result<CompletedChild, ProcessFailure> {
    wait_for_role(command, log, timeout, ProcessRole::Client)
}

fn wait_for_role(
    mut command: Command,
    log: &Path,
    timeout: Duration,
    role: ProcessRole,
) -> Result<CompletedChild, ProcessFailure> {
    install_cancel_handler()?;
    CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    hide_console(&mut command);
    let child = command.spawn()?;
    let pid = child.id();
    let mut child = ChildCleanup(Some(child));
    let deadline = Instant::now() + timeout;
    let mut was_ready = false;
    loop {
        if CANCEL_REQUESTED.load(Ordering::SeqCst) {
            return Err(ProcessFailure::Cancelled { pid });
        }
        was_ready |= log_has_ready_record(log, role, pid).map_err(ProcessFailure::Readiness)?;
        let status = child.0.as_mut().expect("child present").try_wait()?;
        if let Some(status) = status {
            // A fast child can flush readiness and exit after the first read above.
            was_ready |= log_has_ready_record(log, role, pid).map_err(ProcessFailure::Readiness)?;
            child.0.take();
            return if !was_ready {
                Err(ProcessFailure::ExitedBeforeReady { pid, status })
            } else if !status.success() {
                Err(ProcessFailure::ExitedFailed { pid, status })
            } else {
                Ok(CompletedChild { pid })
            };
        }
        if Instant::now() >= deadline {
            return Err(ProcessFailure::TimedOut { pid, timeout });
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn install_cancel_handler() -> Result<(), ProcessFailure> {
    CANCEL_HANDLER
        .get_or_init(|| {
            ctrlc::set_handler(|| CANCEL_REQUESTED.store(true, Ordering::SeqCst))
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| ProcessFailure::CancelHandler(error.clone()))
}

/// Owns only the child it spawned and kills it on every failure or timeout path.
struct ChildCleanup(Option<Child>);

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{JsonlLog, ProcessEvent, ProcessRecord};
    use std::{path::PathBuf, sync::Mutex};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("spall-xtask-{}-{name}.jsonl", std::process::id()))
    }

    fn sleepy_command() -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("ping");
            command.args(["-n", "20", "127.0.0.1"]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sleep");
            command.arg("20");
            command
        }
    }

    fn failing_command() -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("cmd");
            command.args(["/C", "exit 7"]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sh");
            command.args(["-c", "exit 7"]);
            command
        }
    }

    #[test]
    fn timeout_kills_the_owned_child() {
        let _guard = TEST_LOCK.lock().unwrap();
        let log = path("timeout");
        let result = wait_for_server(sleepy_command(), &log, Duration::from_millis(30));
        let pid = result.as_ref().err().and_then(ProcessFailure::pid).unwrap();
        assert!(matches!(result, Err(ProcessFailure::TimedOut { .. })));
        assert!(!process_is_running(pid));
        assert!(!log.exists());
    }

    #[test]
    fn cancellation_kills_the_owned_child() {
        let _guard = TEST_LOCK.lock().unwrap();
        let log = path("cancel");
        let cancel = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(30));
            CANCEL_REQUESTED.store(true, Ordering::SeqCst);
        });
        let result = wait_for_server(sleepy_command(), &log, Duration::from_secs(3));
        cancel.join().unwrap();
        let pid = result.as_ref().err().and_then(ProcessFailure::pid).unwrap();
        assert!(matches!(result, Err(ProcessFailure::Cancelled { .. })));
        assert!(!process_is_running(pid));
    }

    #[test]
    fn child_failure_before_ready_is_reported() {
        let _guard = TEST_LOCK.lock().unwrap();
        let log = path("failure");
        let result = wait_for_server(failing_command(), &log, Duration::from_secs(1));
        assert!(matches!(
            result,
            Err(ProcessFailure::ExitedBeforeReady { .. })
        ));
    }

    #[test]
    fn readiness_requires_the_spawned_child_pid_and_successful_completion() {
        let _guard = TEST_LOCK.lock().unwrap();
        let log = path("success");
        let mut command = sleepy_command();
        hide_console(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let mut jsonl = JsonlLog::create(&log).unwrap();
        jsonl
            .write(&ProcessRecord::new(
                ProcessEvent::Ready,
                ProcessRole::Server,
                None,
            ))
            .unwrap();
        drop(jsonl);
        assert!(!log_has_ready_record(&log, ProcessRole::Server, pid).unwrap());
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_file(log).unwrap();
    }

    #[cfg(windows)]
    fn process_is_running(pid: u32) -> bool {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
    }

    #[cfg(not(windows))]
    fn process_is_running(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}
