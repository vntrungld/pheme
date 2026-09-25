//! Supervises the core process (`pheme server` or `pheme client`) on behalf
//! of a GUI front-end.
//!
//! One task watches the child for an unprompted exit and enforces the
//! stop-then-kill sequence; a second owns the accepted [`IpcConnection`] and
//! turns incoming [`Status`] values into the supervisor's published state.
//! Splitting the two means neither ever blocks on the other: a child that
//! never connects (it could not bind its own listen port) is still noticed
//! by the first, and a connection that outlives its usefulness is closed by
//! the second without touching the process directly.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command as ProcessCommand};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::{Config, Role};
use crate::ipc::{Command, IpcListener, Status};

/// How long [`Supervisor::shutdown`] and the stop half of
/// [`Supervisor::apply_config`] wait for the child to exit on its own before
/// killing it. The core already exits once its socket closes, so this is
/// the ordinary case's budget, not the expected wait.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// What the supervised core is doing right now.
#[derive(Debug, Clone)]
pub enum CoreState {
    /// No configuration yet: nothing to run. The first run for every new
    /// user, every time.
    NoConfig,
    /// A child is running and this is its most recent report.
    Running(Status),
    /// The child exited. The string is why, as far as we can tell: the exit
    /// status and, if it said anything, the tail of its stderr.
    Stopped(String),
}

/// One spawned child and the tasks managing it.
struct Generation {
    pid: u32,
    ipc_cmd_tx: mpsc::Sender<Command>,
    child_cmd_tx: mpsc::Sender<ChildCmd>,
    ipc_task: JoinHandle<()>,
    child_task: JoinHandle<()>,
}

/// A message the stop path sends to the task that owns the child.
enum ChildCmd {
    /// Wait up to the given duration for the child to exit; kill it if it
    /// has not, then reply once it is confirmed gone.
    Stop(Duration, oneshot::Sender<()>),
}

/// Spawns the child the front-end's own role implies and watches it.
pub struct Supervisor {
    exe: PathBuf,
    cfg: Option<Config>,
    state: Arc<Mutex<CoreState>>,
    current: Option<Generation>,
}

impl Supervisor {
    /// Spawns the core `cfg.role` implies, or nothing at all if `cfg` is
    /// `None` — the state a brand-new install starts in.
    ///
    /// `exe` is the executable to run as the child: `std::env::current_exe()`
    /// in production, since the front-end and the core are the same `pheme`
    /// binary told apart by its subcommand, and a stub in tests.
    pub async fn start(exe: PathBuf, cfg: Option<Config>) -> anyhow::Result<Supervisor> {
        let mut sup = Supervisor {
            exe,
            cfg,
            state: Arc::new(Mutex::new(CoreState::NoConfig)),
            current: None,
        };
        sup.spawn_current().await?;
        Ok(sup)
    }

    /// The state as of the most recent report; never blocks on the child.
    pub fn state(&self) -> CoreState {
        self.state.lock().expect("state mutex poisoned").clone()
    }

    /// Sends a command to the running child, if any. Silently dropped if
    /// nothing is connected yet — there is no queue of commands waiting for
    /// a core that has not shown up.
    pub async fn send(&mut self, c: Command) {
        if let Some(gen) = &self.current {
            let _ = gen.ipc_cmd_tx.send(c).await;
        }
    }

    /// Stops the child, writes `cfg` to `path`, and starts it again with the
    /// new configuration.
    pub async fn apply_config(&mut self, cfg: Config, path: &Path) -> anyhow::Result<()> {
        self.stop_current().await;
        cfg.save(path)?;
        self.cfg = Some(cfg);
        self.spawn_current().await?;
        Ok(())
    }

    /// Stops the child and waits up to two seconds before killing it.
    pub async fn shutdown(&mut self) {
        self.stop_current().await;
    }

    /// Stops the child if one is running and starts it again from the
    /// configuration already held. Unlike [`apply_config`][Self::apply_config],
    /// this never touches disk: starting a child back up is not a
    /// configuration change, and `restart` takes no `path` at all because
    /// there is nothing for it to write. `NoConfig` if nothing has ever been
    /// configured, same as a fresh [`Supervisor::start`].
    pub async fn restart(&mut self) -> anyhow::Result<()> {
        self.stop_current().await;
        self.spawn_current().await
    }

    /// The child's process id while one is running.
    pub fn child_pid(&self) -> Option<u32> {
        self.current.as_ref().map(|gen| gen.pid)
    }

    /// Spawns a child for the current configuration, or reports `NoConfig`
    /// if there is none.
    async fn spawn_current(&mut self) -> anyhow::Result<()> {
        let Some(cfg) = self.cfg.clone() else {
            *self.state.lock().expect("state mutex poisoned") = CoreState::NoConfig;
            return Ok(());
        };
        let gen = spawn_generation(&self.exe, &cfg, self.state.clone()).await?;
        self.current = Some(gen);
        Ok(())
    }

    /// Stops whatever is currently running, if anything, and waits for it to
    /// be gone before returning.
    async fn stop_current(&mut self) {
        let Some(gen) = self.current.take() else {
            return;
        };
        // A courtesy the child may or may not act on. What actually ends it
        // is the socket closing, which the task below forces regardless.
        let _ = gen.ipc_cmd_tx.send(Command::Stop).await;
        gen.ipc_task.abort();

        let (done_tx, done_rx) = oneshot::channel();
        let _ = gen
            .child_cmd_tx
            .send(ChildCmd::Stop(STOP_TIMEOUT, done_tx))
            .await;
        // If the child had already exited on its own, the send above found
        // no receiver and `done_tx` was dropped with it: `done_rx` resolves
        // to an error immediately rather than hanging.
        let _ = done_rx.await;
        let _ = gen.child_task.await;
    }
}

/// Spawns one child and the two tasks that watch it.
async fn spawn_generation(
    exe: &Path,
    cfg: &Config,
    state: Arc<Mutex<CoreState>>,
) -> anyhow::Result<Generation> {
    let listener = IpcListener::bind()
        .await
        .context("binding the front-end's ipc listener")?;
    let ipc_path = listener.path().to_string();
    let role = match cfg.role {
        Role::Server => "server",
        Role::Client => "client",
    };

    let mut command = ProcessCommand::new(exe);
    command
        .arg(role)
        .arg("--ipc")
        .arg(&ipc_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("the child exited before it could be identified"))?;

    let stderr_rx = capture_stderr(&mut child);

    let (child_cmd_tx, child_cmd_rx) = mpsc::channel(1);
    let child_task = tokio::spawn(run_child(child, stderr_rx, state.clone(), child_cmd_rx));

    let (ipc_cmd_tx, ipc_cmd_rx) = mpsc::channel(4);
    let ipc_task = tokio::spawn(run_ipc(listener, state, ipc_cmd_rx));

    Ok(Generation {
        pid,
        ipc_cmd_tx,
        child_cmd_tx,
        ipc_task,
        child_task,
    })
}

/// Drains the child's stderr in the background and reports the whole of it
/// once the pipe closes, so the reason a core failed to start can be built
/// without racing the read against `wait()` noticing the exit.
fn capture_stderr(child: &mut Child) -> oneshot::Receiver<String> {
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        let _ = tx.send(buf);
    });
    rx
}

/// Owns the child. Reports `Stopped` the moment it exits on its own, and
/// never waits for a status that a dead process will never send. Also
/// carries out a stop request: wait up to the given duration, then kill.
async fn run_child(
    mut child: Child,
    stderr_rx: oneshot::Receiver<String>,
    state: Arc<Mutex<CoreState>>,
    mut cmd_rx: mpsc::Receiver<ChildCmd>,
) {
    tokio::select! {
        status = child.wait() => {
            let stderr = stderr_rx.await.unwrap_or_default();
            let reason = describe_exit(status, &stderr);
            *state.lock().expect("state mutex poisoned") = CoreState::Stopped(reason);
        }
        cmd = cmd_rx.recv() => {
            let Some(ChildCmd::Stop(deadline, done)) = cmd else {
                return;
            };
            let status = match tokio::time::timeout(deadline, child.wait()).await {
                Ok(status) => status,
                Err(_) => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let stderr = stderr_rx.await.unwrap_or_default();
            let reason = describe_exit(status, &stderr);
            *state.lock().expect("state mutex poisoned") = CoreState::Stopped(reason);
            let _ = done.send(());
        }
    }
}

/// Owns the accepted [`IpcConnection`][crate::ipc::IpcConnection]. Turns
/// incoming statuses into published state and outgoing commands from
/// `cmd_rx` into frames on the wire, until the connection ends.
async fn run_ipc(
    mut listener: IpcListener,
    state: Arc<Mutex<CoreState>>,
    mut cmd_rx: mpsc::Receiver<Command>,
) {
    let mut conn = match listener.accept().await {
        Ok(conn) => conn,
        Err(_) => return,
    };
    loop {
        tokio::select! {
            status = conn.recv_status() => {
                match status {
                    Ok(Some(status)) => {
                        *state.lock().expect("state mutex poisoned") = CoreState::Running(status);
                    }
                    _ => return,
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(c) => { let _ = conn.send_command(c).await; }
                    None => return,
                }
            }
        }
    }
}

/// A human-readable reason the child is no longer running.
fn describe_exit(status: std::io::Result<std::process::ExitStatus>, stderr: &str) -> String {
    let mut reason = match status {
        Ok(status) => match status.code() {
            Some(0) => "exited normally".to_string(),
            Some(code) => format!("exited with status {code}"),
            None => "terminated by a signal".to_string(),
        },
        Err(e) => format!("could not wait for the child: {e}"),
    };
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        reason.push_str(": ");
        reason.push_str(stderr);
    }
    reason
}
