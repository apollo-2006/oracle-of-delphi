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
//!
//! Children can also be paused and resumed at runtime via [`ChildHandle`], which
//! is what lets the LLM server be unloaded while idle and brought back on the
//! next wake word. A paused child is killed, not merely stopped being watched:
//! the whole point is to give its VRAM back.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::watch;
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

/// A runtime control for one supervised child.
///
/// Cloneable and cheap: it is a handle on a watch channel the child's own loop
/// is listening to. Dropping every clone does NOT stop the child — only the
/// supervisor's cancellation does that — so a caller can hold one for as long
/// as it likes without owning the child's lifetime.
#[derive(Clone)]
pub struct ChildHandle {
    name: String,
    /// Shared with the [`Supervisor`], which holds its own reference for the
    /// whole of its life. That is what makes dropping every caller-side handle
    /// harmless: the watch channel stays open, so the child's loop keeps
    /// waiting on it instead of seeing the sender disappear.
    desired_running: std::sync::Arc<watch::Sender<bool>>,
}

impl ChildHandle {
    /// Kill the child and keep it down until [`start`](Self::start).
    ///
    /// Returns true if this actually changed the desired state, so callers can
    /// avoid logging "unloading" every time an idle timer ticks.
    pub fn stop(&self) -> bool {
        let changed = *self.desired_running.borrow();
        if changed {
            let _ = self.desired_running.send(false);
        }
        changed
    }

    /// Bring the child back up if it is down. Returns true if it was down.
    pub fn start(&self) -> bool {
        let was_down = !*self.desired_running.borrow();
        if was_down {
            let _ = self.desired_running.send(true);
        }
        was_down
    }

    /// Whether the child is *meant* to be running. Not a health check: a child
    /// that just crashed still reads true while it waits to be restarted.
    pub fn is_running(&self) -> bool {
        *self.desired_running.borrow()
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Supervises a set of children until a shared cancellation token fires.
pub struct Supervisor {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// One reference per supervised child, held for the supervisor's whole life.
    ///
    /// Without this the only `Sender` lived in the [`ChildHandle`] returned to
    /// the caller — so a caller that ignored the return value (as the actd
    /// launch does: it never needs to pause the daemon) dropped the sender
    /// immediately. The child's loop then saw `desired.changed()` return `Err`,
    /// took its "nothing can ever resume us" path, and killed the process it
    /// had just spawned. No log line was emitted on that path, so the visible
    /// symptom was a daemon that "started" and was never heard from again.
    keepalive: Vec<std::sync::Arc<watch::Sender<bool>>>,
}

impl Supervisor {
    pub fn new(cancel: CancellationToken) -> Self {
        Supervisor {
            cancel,
            tasks: Vec::new(),
            keepalive: Vec::new(),
        }
    }

    /// Start supervising a child immediately. The returned handle can pause and
    /// resume it later; ignoring it just means the child runs until shutdown.
    pub fn supervise(&mut self, spec: ChildSpec) -> ChildHandle {
        self.supervise_with_state(spec, true)
    }

    /// Supervise a child that starts out paused — registered and controllable,
    /// but not launched until someone calls [`ChildHandle::start`].
    pub fn supervise_paused(&mut self, spec: ChildSpec) -> ChildHandle {
        self.supervise_with_state(spec, false)
    }

    fn supervise_with_state(&mut self, spec: ChildSpec, running: bool) -> ChildHandle {
        let cancel = self.cancel.clone();
        let (tx, rx) = watch::channel(running);
        let tx = std::sync::Arc::new(tx);
        // The supervisor's own reference: the child outlives the caller's handle.
        self.keepalive.push(tx.clone());
        let handle = ChildHandle {
            name: spec.name.clone(),
            desired_running: tx,
        };
        self.tasks.push(tokio::spawn(run_child(spec, cancel, rx)));
        handle
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
async fn run_child(spec: ChildSpec, cancel: CancellationToken, mut desired: watch::Receiver<bool>) {
    let mut backoff = Duration::from_millis(500);
    let max_backoff = Duration::from_secs(15);

    while !cancel.is_cancelled() {
        // Park while the child is meant to be down. `borrow()` is scoped tightly
        // because its guard is not Send and must not be held across an await.
        while !*desired.borrow() {
            tokio::select! {
                _ = cancel.cancelled() => return,
                changed = desired.changed() => {
                    // The sender is gone: nothing can ever resume us.
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }

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
                    changed = desired.changed() => {
                        if changed.is_err() {
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                            return;
                        }
                        if !*desired.borrow() {
                            // Paused: kill it now. Releasing the resources IS the
                            // feature, so this must not wait for a natural exit.
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                            info!(name = %spec.name, "supervised process paused");
                        }
                        // Either way, loop back: the park above re-checks state,
                        // and a resume simply falls through to a fresh spawn.
                        continue;
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
    async fn a_child_survives_its_handle_being_dropped() {
        // The regression this pins: `supervise()` returns a ChildHandle, and a
        // caller with no reason to pause the child (the actd launch) discards
        // it. That used to drop the only watch::Sender, so the child's loop saw
        // `changed()` return Err, took its "nothing can ever resume us" path,
        // and killed the process microseconds after spawning it -- emitting no
        // log line at all, so actd appeared to start and was simply never
        // there. ChildHandle's own documentation promises the opposite.
        let marker = tmp_log("handle-drop-marker");
        let _ = std::fs::remove_file(&marker);
        let (program, args) = appender(&marker);

        let cancel = CancellationToken::new();
        let mut sup = Supervisor::new(cancel.clone());
        // Deliberately discard the handle, exactly as the actd launch does.
        let _ = sup.supervise(ChildSpec {
            name: "handle-drop".into(),
            program,
            args,
            log_path: tmp_log("handle-drop"),
        });

        // Long enough for a killed child to have been restarted several times
        // (backoff starts at 500ms) if the bug were still present.
        tokio::time::sleep(Duration::from_millis(1600)).await;

        let starts = std::fs::read_to_string(&marker)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        assert_eq!(
            starts, 1,
            "the child should have started exactly once and still be running; \
             0 means it was killed before it got going, >1 that it was killed \
             and restarted -- got {starts}"
        );

        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("shutdown must not hang");
        let _ = std::fs::remove_file(&marker);
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

    /// Count how many times a child has (re)started by having it append to a
    /// file. More reliable than watching process tables.
    fn appender(marker: &std::path::Path) -> (String, Vec<String>) {
        let m = marker.to_string_lossy().to_string();
        if cfg!(windows) {
            (
                "cmd".into(),
                vec![
                    "/C".into(),
                    format!("echo x >> \"{m}\" & ping 127.0.0.1 -n 30 > NUL"),
                ],
            )
        } else {
            (
                "sh".into(),
                vec!["-c".into(), format!("echo x >> '{m}'; sleep 30")],
            )
        }
    }

    fn launches(marker: &std::path::Path) -> usize {
        std::fs::read_to_string(marker)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn a_paused_child_is_killed_and_a_resumed_one_relaunches() {
        // This is the whole basis of idle unload: pausing must actually release
        // the process, and resuming must bring a fresh one back.
        let marker = tmp_log("pauseresume");
        let _ = std::fs::remove_file(&marker);
        let (program, args) = appender(&marker);

        let cancel = CancellationToken::new();
        let mut sup = Supervisor::new(cancel.clone());
        let handle = sup.supervise(ChildSpec {
            name: "pausable".into(),
            program,
            args,
            log_path: tmp_log("pauseresume-log"),
        });

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(launches(&marker), 1, "should have launched once");
        assert!(handle.is_running());

        assert!(handle.stop(), "stop should report a state change");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!handle.is_running());
        assert_eq!(
            launches(&marker),
            1,
            "a paused child must not be restarted by the supervise loop"
        );

        assert!(handle.start(), "start should report it was down");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(launches(&marker), 2, "resuming must spawn a fresh child");

        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("shutdown must not hang");
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn stop_and_start_are_idempotent() {
        let cancel = CancellationToken::new();
        let mut sup = Supervisor::new(cancel.clone());
        let (program, args) = sleeper("30");
        let handle = sup.supervise(ChildSpec {
            name: "idem".into(),
            program,
            args,
            log_path: tmp_log("idem"),
        });
        assert!(handle.stop(), "first stop changes state");
        assert!(!handle.stop(), "second stop is a no-op");
        assert!(handle.start(), "first start changes state");
        assert!(!handle.start(), "second start is a no-op");
        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("shutdown must not hang");
    }

    #[tokio::test]
    async fn a_child_supervised_paused_never_launches_until_started() {
        let marker = tmp_log("startpaused");
        let _ = std::fs::remove_file(&marker);
        let (program, args) = appender(&marker);

        let cancel = CancellationToken::new();
        let mut sup = Supervisor::new(cancel.clone());
        let handle = sup.supervise_paused(ChildSpec {
            name: "lazy".into(),
            program,
            args,
            log_path: tmp_log("startpaused-log"),
        });

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(launches(&marker), 0, "must not launch while paused");
        assert!(!handle.is_running());

        handle.start();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(launches(&marker), 1);

        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("shutdown must not hang");
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn shutdown_is_prompt_while_a_child_is_paused() {
        // The park loop must wake on cancellation, not only on a resume.
        let cancel = CancellationToken::new();
        let mut sup = Supervisor::new(cancel.clone());
        let (program, args) = sleeper("30");
        let handle = sup.supervise(ChildSpec {
            name: "parked".into(),
            program,
            args,
            log_path: tmp_log("parked"),
        });
        handle.stop();
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("shutdown must not hang on a parked child");
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
