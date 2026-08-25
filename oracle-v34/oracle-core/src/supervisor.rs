//! Process supervisor: the piece that turns "open five terminals" into "launch
//! one thing".
//!
//! `oracle-core run` uses this to bring up its dependencies — the local LLM
//! server and the `oracle-actd` daemon — as **hidden** background children,
//! restart them (with backoff) if they die, and kill them all when core itself
//! shuts down. On Windows the children are spawned with `CREATE_NO_WINDOW` so no
//! console flashes up; their stdout/stderr are redirected to per-process log
//! files under the runtime dir so nothing is lost when there's no console.
//!
//! The supervisor is deliberately dumb: it does not know what a "healthy" child
//! is beyond "still running". Readiness (e.g. the LLM server accepting requests)
//! is the caller's concern — core simply retries its own connections.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// `CREATE_NO_WINDOW` — spawn a console child with no console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// One managed child process.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// Label for logs (e.g. "llm", "actd").
    pub name: String,
    /// Program to run (name on PATH or a full path).
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Where to append the child's stdout/stderr.
    pub log_path: PathBuf,
}

/// Supervises a set of children until a shared cancellation token fires.
pub struct Supervisor {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Supervisor {
    pub fn new(cancel: CancellationToken) -> Self {
        Supervisor {
            cancel,
            tasks: Vec::new(),
        }
    }

    /// Start supervising a child immediately.
    pub fn supervise(&mut self, spec: ChildSpec) {
        let cancel = self.cancel.clone();
        self.tasks.push(tokio::spawn(run_child(spec, cancel)));
    }

    /// True if nothing is being supervised.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Cancel and wait for every child to be reaped. Idempotent.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

/// Keep one child alive: (re)spawn it until cancelled, then kill it.
async fn run_child(spec: ChildSpec, cancel: CancellationToken) {
    let mut backoff = Duration::from_millis(500);
    let max_backoff = Duration::from_secs(15);

    while !cancel.is_cancelled() {
        match spawn_one(&spec) {
            Ok(mut child) => {
                let pid = child.id();
                info!(name = %spec.name, ?pid, "supervised process started");
                backoff = Duration::from_millis(500); // reset after a clean launch

                tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        info!(name = %spec.name, "supervised process stopped");
                        return;
                    }
                    status = child.wait() => {
                        if cancel.is_cancelled() {
                            return;
                        }
                        warn!(
                            name = %spec.name,
                            ?status,
                            "supervised process exited unexpectedly; restarting"
                        );
                    }
                }
            }
            Err(e) => {
                warn!(name = %spec.name, program = %spec.program, "failed to launch: {e}");
            }
        }

        // Back off before restarting, but wake instantly on cancel.
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Spawn a single instance, hidden, with output redirected to its log file.
fn spawn_one(spec: &ChildSpec) -> std::io::Result<tokio::process::Child> {
    let (out, err) = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log_path)
    {
        Ok(f) => {
            let f2 = f.try_clone()?;
            (Stdio::from(f), Stdio::from(f2))
        }
        // If the log file can't be opened, don't fail the launch — just discard.
        Err(_) => (Stdio::null(), Stdio::null()),
    };

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .kill_on_drop(true);

    // No console window on Windows.
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmp_log(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oracle-sup-test-{name}-{}.log", std::process::id()))
    }

    // A cross-platform "sleep for a while" command for exercising the lifecycle.
    fn sleeper(secs: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            // ping is a reliable ~1s/iteration sleep on Windows without extra deps.
            (
                "cmd".into(),
                vec!["/C".into(), format!("ping 127.0.0.1 -n {secs} > NUL")],
            )
        } else {
            ("sleep".into(), vec![secs.into()])
        }
    }

    #[tokio::test]
    async fn supervises_then_kills_on_shutdown() {
        let (program, args) = sleeper("30");
        let cancel = CancellationToken::new();
        let mut sup = Supervisor::new(cancel.clone());
        sup.supervise(ChildSpec {
            name: "sleeper".into(),
            program,
            args,
            log_path: tmp_log("kill"),
        });
        assert!(!sup.is_empty());
        // Let it get going, then shut down. Should return promptly, not in 30s.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("shutdown must not hang");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn restarts_a_child_that_exits() {
        // A command that exits immediately should be relaunched at least twice.
        let (program, args) = if cfg!(windows) {
            (
                "cmd".to_string(),
                vec!["/C".to_string(), "exit".to_string()],
            )
        } else {
            ("true".to_string(), Vec::new())
        };
        let log = tmp_log("restart");
        let cancel = CancellationToken::new();
        let mut sup = Supervisor::new(cancel.clone());
        sup.supervise(ChildSpec {
            name: "flapper".into(),
            program,
            args,
            log_path: log,
        });
        // Within a short window the backoff loop should have cycled.
        tokio::time::sleep(Duration::from_millis(1300)).await;
        // Nothing to assert on the child directly, but shutdown must still be
        // clean and prompt even while it's mid-restart-backoff.
        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("shutdown during backoff must not hang");
    }

    #[tokio::test]
    async fn bad_program_does_not_panic() {
        let cancel = CancellationToken::new();
        let mut sup = Supervisor::new(cancel.clone());
        sup.supervise(ChildSpec {
            name: "nope".into(),
            program: "this-binary-does-not-exist-oracle".into(),
            args: vec![],
            log_path: tmp_log("bad"),
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("shutdown must not hang even when the program is missing");
    }
}
