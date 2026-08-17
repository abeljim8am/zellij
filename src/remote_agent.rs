//! Persistent remote PTY agent.
//!
//! This module intentionally has no dependency on the Flock server, config or
//! plugin machinery. `connect` is a byte-for-byte bridge between SSH stdio and
//! a detached per-user daemon; `serve` owns PTYs and their replay buffers.

use anyhow::{anyhow, bail, Context, Result};
use nix::errno::Errno;
use nix::sys::termios::{self, SetArg, Termios};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;
use zellij_utils::data::{RemotePaneHealth, RemoteProtocolStatus};

pub const PROTOCOL_VERSION: u16 = 3;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const HISTORY_LIMIT_BYTES: usize = 4 * 1024 * 1024;

fn ensure_protocol_version(protocol: u16) -> Result<()> {
    if protocol != PROTOCOL_VERSION {
        bail!("incompatible protocol version {protocol}; expected {PROTOCOL_VERSION}");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        protocol: u16,
        client_version: String,
    },
    CreatePane {
        pane_id: Option<Uuid>,
        cols: u16,
        rows: u16,
        env: HashMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    AttachPane {
        pane_id: Uuid,
        after_sequence: u64,
    },
    Input {
        pane_id: Uuid,
        data: Vec<u8>,
    },
    Resize {
        pane_id: Uuid,
        cols: u16,
        rows: u16,
    },
    Acknowledge {
        pane_id: Uuid,
        sequence: u64,
    },
    ForegroundProcess {
        pane_id: Uuid,
    },
    ClosePane {
        pane_id: Uuid,
    },
    /// Ask the daemon to stand down so the next connect starts the freshly
    /// installed build. A daemon holding panes cannot exit without killing
    /// their PTYs, so it arms a flag and exits when its last pane closes; an
    /// idle daemon exits immediately. Either way the caller's bridges
    /// reconnect at their saved cursors onto the new daemon.
    Retire,
    ReportAgentState {
        pane_id: Uuid,
        state: RemoteAgentRunState,
        agent: String,
        /// The agent's own process id, when the integration can name it exactly
        /// (the OpenCode plugin runs in-process, so it reports `process.pid`).
        /// The daemon watches this pid for exit — that is how it learns the
        /// agent is gone. Absent for integrations that cannot know it; the
        /// daemon then resolves the pid from the reporter's ancestry.
        #[serde(default)]
        agent_pid: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAgentRunState {
    Working,
    Idle,
    Blocked,
    Release,
}

impl RemoteAgentRunState {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "working" => Ok(Self::Working),
            "idle" => Ok(Self::Idle),
            "blocked" => Ok(Self::Blocked),
            "release" => Ok(Self::Release),
            _ => {
                bail!("invalid agent state {value:?}; expected working, idle, blocked, or release")
            },
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Blocked => "blocked",
            Self::Release => "release",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAgentStateEvent {
    pub pane_id: Uuid,
    pub state: RemoteAgentRunState,
    pub agent: String,
    /// Whether this event is the daemon re-asserting an unchanged picture
    /// rather than the agent reporting a transition. Consumers must treat a
    /// heartbeat as proof the agent is still present without letting it count
    /// as a fresh report — otherwise the periodic re-assertion would keep
    /// resetting the screen-veto windows that arbitrate a stale hook.
    #[serde(default)]
    pub heartbeat: bool,
}

/// Provider-specific carrier for the framed remote-agent protocol. Everything
/// past process spawn (framing, replay, cursor persistence) is shared; only
/// how we reach the remote daemon differs per provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteTransport {
    Coder {
        workspace: String,
    },
    Ssh {
        destination: String,
        ssh_args: Vec<String>,
    },
    Devcontainer {
        workspace_folder: String,
    },
}

impl RemoteTransport {
    /// The provider-scoped identity string fed to `stable_remote_pane_uuid`.
    pub fn identity(&self) -> &str {
        match self {
            Self::Coder { workspace } => workspace,
            Self::Ssh { destination, .. } => destination,
            Self::Devcontainer { workspace_folder } => workspace_folder,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Coder { .. } => "Coder",
            Self::Ssh { .. } => "SSH",
            Self::Devcontainer { .. } => "devcontainer",
        }
    }

    /// A command that can revive the far end after a failed connect attempt:
    /// `devcontainer up` restarts a stopped container (or recreates it after a
    /// rebuild). Coder/ssh hosts have no local revive story — `None`.
    fn recover_command(&self) -> Option<Command> {
        match self {
            Self::Devcontainer { workspace_folder } => {
                let mut command = Command::new("devcontainer");
                command.args(["up", "--workspace-folder", workspace_folder]);
                Some(command)
            },
            _ => None,
        }
    }

    /// Whether a close-worker error means the far end is already gone for
    /// good: a stopped/removed container has killed the daemon and every pane
    /// shell with it, so the close request is moot and counts as done.
    fn close_target_gone(&self, error: &str) -> bool {
        match self {
            Self::Devcontainer { .. } => {
                let lower = error.to_ascii_lowercase();
                lower.contains("is not running")
                    || lower.contains("no such container")
                    || lower.contains("no container found")
            },
            _ => false,
        }
    }

    /// Whether the close worker should give up after a bounded number of
    /// attempts, leaving the pending file for re-arm at the next flock start.
    /// Containers can be transiently unreachable in ways that never converge
    /// (deleted project, wedged docker daemon); coder/ssh keep retrying like
    /// today.
    fn close_retries_bounded(&self) -> bool {
        matches!(self, Self::Devcontainer { .. })
    }

    fn connect_command(&self) -> Command {
        // The ssh-style transports hand the trailing words to the remote login
        // shell as one space-joined string, so the script must ride inside
        // single quotes to survive that second parse. `devcontainer exec`
        // passes argv through verbatim (docker-exec style), so its script
        // stays raw.
        let remote = r#"exec "$HOME/.local/share/flock/current/flock" remote-agent connect"#;
        match self {
            Self::Coder { workspace } => {
                // `--wait yes` holds the connect until the workspace's startup
                // scripts finish, so panes revived into a freshly auto-started
                // workspace don't spawn shells into a half-set-up environment.
                let mut command = Command::new("coder");
                command.args([
                    "ssh",
                    "--wait",
                    "yes",
                    workspace,
                    "--",
                    "sh",
                    "-c",
                    &format!("'{remote}'"),
                ]);
                command
            },
            Self::Devcontainer { workspace_folder } => {
                let mut command = Command::new("devcontainer");
                command.args([
                    "exec",
                    "--workspace-folder",
                    workspace_folder,
                    "sh",
                    "-c",
                    remote,
                ]);
                command
            },
            Self::Ssh {
                destination,
                ssh_args,
            } => {
                // BatchMode forbids password/host-key prompts: a prompt reads
                // /dev/tty while the bridge holds it raw and is already
                // draining stdin, so it could never be answered anyway.
                let mut command = Command::new("ssh");
                command.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=4",
                ]);
                command.args(ssh_args);
                command.arg("--");
                command.arg(destination);
                command.args(["sh", "-c", &format!("'{remote}'")]);
                command
            },
        }
    }

    /// Run an arbitrary POSIX `sh` script on the far end. Quoting follows the
    /// same rule as [`Self::connect_command`]: ssh-style transports join the
    /// trailing words into one string for the remote login shell, so the script
    /// must ride inside single quotes; `devcontainer exec` passes argv through
    /// verbatim. Scripts therefore may not contain single quotes — the
    /// generators assert this.
    fn script_command(&self, script: &str) -> Command {
        assert!(
            !script.contains('\''),
            "remote scripts must not contain single quotes"
        );
        match self {
            Self::Coder { workspace } => {
                let mut command = Command::new("coder");
                command.args([
                    "ssh",
                    "--wait",
                    "yes",
                    workspace,
                    "--",
                    "sh",
                    "-c",
                    &format!("'{script}'"),
                ]);
                command
            },
            Self::Devcontainer { workspace_folder } => {
                let mut command = Command::new("devcontainer");
                command.args([
                    "exec",
                    "--workspace-folder",
                    workspace_folder,
                    "sh",
                    "-c",
                    script,
                ]);
                command
            },
            Self::Ssh {
                destination,
                ssh_args,
            } => {
                let mut command = Command::new("ssh");
                command.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                ]);
                command.args(ssh_args);
                command.arg("--");
                command.arg(destination);
                command.args(["sh", "-c", &format!("'{script}'")]);
                command
            },
        }
    }

    fn close_spec(&self) -> zellij_utils::remote_session_cleanup::RemoteCloseTransport {
        use zellij_utils::remote_session_cleanup::RemoteCloseTransport;
        match self {
            Self::Coder { workspace } => RemoteCloseTransport::Coder {
                workspace: workspace.clone(),
            },
            Self::Ssh {
                destination,
                ssh_args,
            } => RemoteCloseTransport::Ssh {
                destination: destination.clone(),
                extra_args: ssh_args.clone(),
            },
            Self::Devcontainer { workspace_folder } => RemoteCloseTransport::Devcontainer {
                workspace_folder: workspace_folder.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        protocol: u16,
        agent_version: String,
    },
    Error {
        message: String,
    },
    PaneCreated {
        pane_id: Uuid,
        pid: u32,
    },
    Attached {
        pane_id: Uuid,
        next_sequence: u64,
    },
    Output {
        pane_id: Uuid,
        sequence: u64,
        data: Vec<u8>,
    },
    ReplayTruncated {
        pane_id: Uuid,
        first_available: u64,
    },
    ForegroundProcess {
        pane_id: Uuid,
        argv: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    Exited {
        pane_id: Uuid,
        status: Option<i32>,
    },
    PaneClosed {
        pane_id: Uuid,
    },
    AgentStateChanged {
        event: RemoteAgentStateEvent,
    },
    AgentStateAccepted {
        pane_id: Uuid,
    },
    /// Answer to [`ClientMessage::Retire`]. `panes_held` is how many panes must
    /// close before the daemon exits — zero means it is exiting now.
    Retiring {
        panes_held: usize,
    },
}

#[derive(Clone)]
struct OutputChunk {
    sequence: u64,
    data: Vec<u8>,
}

/// How often the daemon checks each tracked agent process for exit. A release is
/// edge-triggered off process death, so this only bounds how quickly the agent
/// leaving is noticed — it costs one `/proc` read per tracked agent.
const PRESENCE_TICK: Duration = Duration::from_secs(2);
/// How many [`PRESENCE_TICK`]s between presence re-assertions.
///
/// Deliberately much slower than the liveness check. A release must be prompt,
/// but a heartbeat is only a safety net for a client that lost the agent, and
/// every heartbeat costs a `flock pipe` process on the *local* host — the
/// bridge forwards each event to the sidebar by spawning one. Re-asserting
/// every tick would be 30 spawns a minute per remote pane to say nothing new.
const HEARTBEAT_EVERY_TICKS: u32 = 5;

/// The agent occupying a pane, as last reported by its integration hook.
///
/// `pid` is the whole point of this struct: presence is the liveness of a
/// specific process, not an inference from the pane's foreground process group.
/// Process death is a fact and is observed exactly once, which is what makes
/// presence stable — the previous design guessed from `tcgetpgrp` plus argv
/// matching and flapped whenever the agent was alive but not the foreground
/// group's leader or ancestor (nested PTYs, `setsid`-ing tools, reaped group
/// leaders).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedAgent {
    label: String,
    /// Never `Release` — a released agent is represented by no tracked agent.
    state: RemoteAgentRunState,
    /// The process to watch. `None` when resolution failed, in which case the
    /// agent is only ever released by an explicit `release` report. Holding a
    /// stale agent is the safe failure: it shows a finished agent until the
    /// pane closes, where guessing would blink a live one out of existence.
    pid: Option<libc::pid_t>,
}

struct PaneState {
    id: Uuid,
    pid: libc::pid_t,
    master: Mutex<File>,
    history: Mutex<VecDeque<OutputChunk>>,
    history_bytes: Mutex<usize>,
    next_sequence: Mutex<u64>,
    subscribers: Mutex<Vec<mpsc::Sender<ServerMessage>>>,
    agent: Mutex<Option<TrackedAgent>>,
    exit_status: Mutex<Option<Option<i32>>>,
}

impl PaneState {
    fn publish(&self, message: ServerMessage) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
    }

    fn record_output(&self, data: Vec<u8>) {
        let mut next = self.next_sequence.lock().unwrap();
        let sequence = *next;
        *next += 1;
        let mut history = self.history.lock().unwrap();
        let mut bytes = self.history_bytes.lock().unwrap();
        let mut subscribers = self.subscribers.lock().unwrap();
        *bytes += data.len();
        history.push_back(OutputChunk {
            sequence,
            data: data.clone(),
        });
        while *bytes > HISTORY_LIMIT_BYTES && history.len() > 1 {
            if let Some(removed) = history.pop_front() {
                *bytes -= removed.data.len();
            }
        }
        let message = ServerMessage::Output {
            pane_id: self.id,
            sequence,
            data,
        };
        subscribers.retain(|subscriber| subscriber.send(message.clone()).is_ok());
    }

    /// Send a coherent history snapshot before making a client live. Holding
    /// the sequence lock prevents new output from being recorded between the
    /// replay watermark and subscriber registration.
    fn subscribe_with_replay(
        &self,
        after_sequence: u64,
        tx: &mpsc::Sender<ServerMessage>,
    ) -> Result<()> {
        let next_sequence = self.next_sequence.lock().unwrap();
        let history = self.history.lock().unwrap();
        let mut subscribers = self.subscribers.lock().unwrap();
        if let Some(first) = history.front().map(|chunk| chunk.sequence) {
            if after_sequence.saturating_add(1) < first {
                tx.send(ServerMessage::ReplayTruncated {
                    pane_id: self.id,
                    first_available: first,
                })?;
            }
        }
        let replay_is_complete = history
            .front()
            .is_none_or(|chunk| after_sequence.saturating_add(1) >= chunk.sequence);
        if replay_is_complete {
            for chunk in history
                .iter()
                .filter(|chunk| chunk.sequence > after_sequence)
            {
                tx.send(ServerMessage::Output {
                    pane_id: self.id,
                    sequence: chunk.sequence,
                    data: chunk.data.clone(),
                })?;
            }
        }
        tx.send(ServerMessage::Attached {
            pane_id: self.id,
            next_sequence: *next_sequence,
        })?;
        // Replay presence as a heartbeat: the attaching client needs the
        // current picture, but this is a re-assertion of an existing report,
        // not a fresh transition.
        if let Some(event) = self.presence_event(true) {
            tx.send(ServerMessage::AgentStateChanged { event })?;
        }
        subscribers.push(tx.clone());
        Ok(())
    }

    /// Record an agent self-report. A `release` clears the tracked agent;
    /// anything else installs (or refreshes) it, resolving the process to watch
    /// from the explicit pid the integration supplied or, failing that, from the
    /// reporting process's ancestry.
    ///
    /// `reporter_pid` comes from the socket's peer credentials, so it is the
    /// kernel's answer rather than the caller's claim.
    fn record_agent_state(
        &self,
        state: RemoteAgentRunState,
        label: String,
        reporter_pid: Option<libc::pid_t>,
        explicit_pid: Option<libc::pid_t>,
    ) {
        if state == RemoteAgentRunState::Release {
            self.clear_agent(label);
            return;
        }
        let event = {
            let mut tracked = self.agent.lock().unwrap();
            // Keep an already-resolved pid across state transitions: only the
            // first report of an agent needs to pay for the ancestry walk, and
            // a later report from a short-lived helper must not overwrite a
            // good answer with a worse one.
            let pid = tracked
                .as_ref()
                .filter(|existing| existing.label == label)
                .and_then(|existing| existing.pid)
                .or_else(|| {
                    resolve_agent_pid(&label, reporter_pid, explicit_pid, self.pid, |pid| {
                        read_process_info(pid)
                    })
                });
            *tracked = Some(TrackedAgent {
                label: label.clone(),
                state,
                pid,
            });
            RemoteAgentStateEvent {
                pane_id: self.id,
                state,
                agent: label,
                heartbeat: false,
            }
        };
        self.publish(ServerMessage::AgentStateChanged { event });
    }

    /// Drop the tracked agent and tell every subscriber. Idempotent: a release
    /// for a pane with no agent still publishes, so a client that somehow holds
    /// a stale agent converges.
    fn clear_agent(&self, label: String) {
        let label = match self.agent.lock().unwrap().take() {
            Some(tracked) => tracked.label,
            None => label,
        };
        self.publish(ServerMessage::AgentStateChanged {
            event: RemoteAgentStateEvent {
                pane_id: self.id,
                state: RemoteAgentRunState::Release,
                agent: label,
                heartbeat: false,
            },
        });
    }

    /// The current presence picture as an event, or `None` when no agent is
    /// tracked. Does not check liveness — call [`Self::settle_presence`] first
    /// when that matters.
    fn presence_event(&self, heartbeat: bool) -> Option<RemoteAgentStateEvent> {
        self.agent
            .lock()
            .unwrap()
            .as_ref()
            .map(|tracked| RemoteAgentStateEvent {
                pane_id: self.id,
                state: tracked.state,
                agent: tracked.label.clone(),
                heartbeat,
            })
    }

    /// Reconcile the tracked agent against its process, releasing it if that
    /// process is gone and otherwise re-asserting presence when
    /// `broadcast_heartbeat` is set.
    ///
    /// This is the whole of presence maintenance. A watched process that has
    /// exited releases the agent exactly once — always, regardless of
    /// `broadcast_heartbeat`, because a release is news. The periodic
    /// re-announcement is what makes the local side a projection rather than an
    /// authority: a client that dropped the agent (plugin reload, a missed
    /// frame, a reconnect) recovers on the next heartbeat instead of waiting for
    /// the agent's next state transition, which for an idle agent may never come.
    fn settle_presence(&self, broadcast_heartbeat: bool) {
        let released = {
            let mut tracked = self.agent.lock().unwrap();
            match tracked.as_ref() {
                Some(agent) => match agent.pid {
                    Some(pid) if !process_is_alive(pid) => {
                        let label = agent.label.clone();
                        *tracked = None;
                        Some(label)
                    },
                    _ => None,
                },
                None => None,
            }
        };
        let event = match released {
            Some(label) => RemoteAgentStateEvent {
                pane_id: self.id,
                state: RemoteAgentRunState::Release,
                agent: label,
                heartbeat: false,
            },
            None if broadcast_heartbeat => match self.presence_event(true) {
                Some(event) => event,
                None => return,
            },
            None => return,
        };
        self.publish(ServerMessage::AgentStateChanged { event });
    }
}

/// Resolve the process whose exit means "this agent is gone".
///
/// Preference order, most exact first:
///
/// 1. `explicit_pid` — the integration named its own process (OpenCode's plugin
///    runs inside the agent, so it knows). Accepted only after confirming it
///    really is the reporter or one of its ancestors inside this pane, so a
///    wrong or stale claim cannot make the daemon watch an unrelated process.
/// 2. The ancestor closest to the pane's shell whose argv looks like `label`.
///    Argv matching is used *once*, to find a pid — not continuously to decide
///    presence, which is what made the previous design flap. Searching from the
///    shell end rather than from the reporter matters: an integration's own hook
///    script is usually installed under a path named after the agent
///    (`~/.claude/hooks/flock-agent-state.sh`), so a reporter-first search would
///    match the short-lived wrapper that invoked it and release the agent as
///    soon as that wrapper exited.
/// 3. The last ancestor before the pane's shell, i.e. whatever the shell
///    launched. Correct unless a resident wrapper (`devenv shell`, `nix
///    develop`) sits between, in which case the agent reads as present until
///    the pane closes.
///
/// `None` when the reporter is unknown or its ancestry never reaches the pane
/// shell; the agent then relies on an explicit `release` report.
fn resolve_agent_pid<F>(
    label: &str,
    reporter_pid: Option<libc::pid_t>,
    explicit_pid: Option<libc::pid_t>,
    shell_pid: libc::pid_t,
    mut process_info: F,
) -> Option<libc::pid_t>
where
    F: FnMut(libc::pid_t) -> Option<(libc::pid_t, Vec<String>)>,
{
    let reporter_pid = reporter_pid?;
    // Walk once and keep the chain; every candidate below is a position in it.
    let mut chain: Vec<(libc::pid_t, Vec<String>)> = Vec::new();
    let mut pid = reporter_pid;
    let mut reached_shell = false;
    // A pane's process tree is shallow. The bound also stops a malformed or
    // racing snapshot with a parent cycle from spinning here.
    for _ in 0..64 {
        if pid == shell_pid {
            reached_shell = true;
            break;
        }
        let (parent_pid, argv) = process_info(pid)?;
        chain.push((pid, argv));
        if parent_pid <= 1 || parent_pid == pid {
            break;
        }
        pid = parent_pid;
    }
    if !reached_shell {
        // The reporter is not inside this pane's tree — a stale hook from a
        // previous pane, or a snapshot that raced process exit. Watching a pid
        // we cannot place would be worse than watching none.
        return None;
    }
    if let Some(explicit) = explicit_pid {
        if chain.iter().any(|(pid, _)| *pid == explicit) {
            return Some(explicit);
        }
    }
    if let Some((pid, _)) = chain
        .iter()
        .rev()
        .find(|(_, argv)| argv_looks_like_agent(label, argv))
    {
        return Some(*pid);
    }
    chain.last().map(|(pid, _)| *pid)
}

/// Whether an argv looks like the named agent, used only to pick the pid to
/// watch. Matches the label as a substring of any token, covering direct
/// binaries (`opencode`), absolute paths, runtime wrappers (`node
/// …/opencode/index.js`) and nix wrapper names (`.claude-unwrapped`).
fn argv_looks_like_agent(label: &str, argv: &[String]) -> bool {
    let label = label.to_ascii_lowercase();
    argv.iter()
        .any(|token| token.to_ascii_lowercase().contains(&label))
}

/// Whether a process still exists and has not become a zombie.
///
/// Prefers `/proc`, which distinguishes a zombie from a running process —
/// `kill(pid, 0)` reports a reaped-but-unwaited child as alive. A missing
/// `/proc` entry only means "gone" when `/proc` itself is readable, so an
/// environment without it (a hardened container, a non-Linux host running the
/// tests) falls back to signal probing rather than declaring every agent dead.
/// Getting this wrong in the pessimistic direction would release live agents,
/// which is the failure the whole redesign exists to remove.
fn process_is_alive(pid: libc::pid_t) -> bool {
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        return stat
            .rsplit_once(')')
            .and_then(|(_, suffix)| suffix.split_whitespace().next())
            .is_none_or(|state| state != "Z");
    }
    if Path::new("/proc/self/stat").exists() {
        // `/proc` works and this pid has no entry: the process is gone.
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The connecting process's pid, from the kernel's socket peer credentials.
/// Unlike a pid the client sends, this cannot be forged or go stale.
///
/// `SO_PEERCRED` is Linux-only, which is also the only platform the daemon runs
/// on (see [`require_supported_platform`]). The module still compiles for the
/// local bridge and CLI on other hosts, so the lookup degrades to "unknown"
/// there rather than failing the build.
#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> Option<libc::pid_t> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut libc::ucred as *mut libc::c_void,
            &mut length,
        )
    };
    (result == 0 && credentials.pid > 0).then_some(credentials.pid)
}

#[cfg(not(target_os = "linux"))]
fn peer_pid(_stream: &UnixStream) -> Option<libc::pid_t> {
    None
}

fn read_process_info(pid: libc::pid_t) -> Option<(libc::pid_t, Vec<String>)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `/proc/<pid>/stat` wraps comm in parentheses and comm itself may contain
    // spaces or `)`, so split at the final close-paren. The suffix begins with
    // field 3 (state), followed by field 4 (parent pid).
    let (_, suffix) = stat.rsplit_once(')')?;
    let parent_pid = suffix.split_whitespace().nth(1)?.parse().ok()?;
    Some((parent_pid, read_process_argv(pid)))
}

fn read_process_argv(pid: libc::pid_t) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

type TransportWriter = Arc<Mutex<Box<dyn Write + Send>>>;

struct InputRouter {
    pane_id: Uuid,
    writer: Mutex<Option<TransportWriter>>,
    writer_ready: Condvar,
}

impl InputRouter {
    fn new(pane_id: Uuid) -> Self {
        Self {
            pane_id,
            writer: Mutex::new(None),
            writer_ready: Condvar::new(),
        }
    }

    fn activate(self: &Arc<Self>, writer: TransportWriter) -> ActiveInputWriter {
        *self.writer.lock().unwrap() = Some(writer.clone());
        self.writer_ready.notify_all();
        ActiveInputWriter {
            router: self.clone(),
            writer,
        }
    }

    fn forward(&self, data: Vec<u8>) {
        loop {
            let writer = {
                let mut writer = self.writer.lock().unwrap();
                while writer.is_none() {
                    writer = self.writer_ready.wait(writer).unwrap();
                }
                writer.as_ref().unwrap().clone()
            };
            if write_frame(
                &mut *writer.lock().unwrap(),
                &ClientMessage::Input {
                    pane_id: self.pane_id,
                    data: data.clone(),
                },
            )
            .is_ok()
            {
                return;
            }
            self.clear_if_current(&writer);
        }
    }

    fn clear_if_current(&self, writer: &TransportWriter) {
        let mut current = self.writer.lock().unwrap();
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, writer))
        {
            *current = None;
        }
    }
}

struct ActiveInputWriter {
    router: Arc<InputRouter>,
    writer: TransportWriter,
}

struct RawTerminalGuard {
    fd: i32,
    original: Termios,
}

impl RawTerminalGuard {
    fn enter(fd: i32) -> Result<Option<Self>> {
        let original = match termios::tcgetattr(fd) {
            Ok(original) => original,
            Err(error) if error == Errno::ENOTTY => return Ok(None),
            Err(error) => return Err(error).context("read local terminal settings"),
        };
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, SetArg::TCSANOW, &raw)
            .context("put local Coder bridge terminal in raw mode")?;
        Ok(Some(Self { fd, original }))
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.fd, SetArg::TCSANOW, &self.original);
    }
}

impl Drop for ActiveInputWriter {
    fn drop(&mut self) {
        self.router.clear_if_current(&self.writer);
    }
}

type Panes = Arc<Mutex<HashMap<Uuid, Arc<PaneState>>>>;

pub fn serve(socket: Option<PathBuf>, foreground: bool) -> Result<()> {
    require_supported_platform()?;
    let socket = socket.unwrap_or(default_socket_path()?);
    if !foreground {
        if daemon_is_live(&socket) {
            return Ok(());
        }
        let executable = std::env::current_exe().context("locate the Flock executable")?;
        let child = Command::new(executable)
            .args(["remote-agent", "serve", "--foreground", "--socket"])
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start the detached remote-agent daemon")?;
        let _ = child.id();
        wait_for_socket(&socket, Duration::from_secs(3))?;
        return Ok(());
    }

    if let Some(parent) = socket.parent() {
        let parent_existed = parent.exists();
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        if !parent_existed {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    if socket.exists() {
        if daemon_is_live(&socket) {
            bail!("another remote-agent daemon is already listening");
        }
        fs::remove_file(&socket).with_context(|| format!("remove stale {}", socket.display()))?;
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("bind user-only socket {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    if let Err(error) = install_opencode_state_plugin() {
        eprintln!("flock remote-agent: could not install the OpenCode state plugin: {error:#}");
    }
    let panes: Panes = Arc::new(Mutex::new(HashMap::new()));
    start_presence_ticker(panes.clone());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let panes = panes.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, panes) {
                        eprintln!("flock remote-agent client: {error:#}");
                    }
                });
            },
            Err(error) => eprintln!("flock remote-agent accept: {error}"),
        }
    }
    Ok(())
}

pub fn connect(socket: Option<PathBuf>) -> Result<()> {
    require_supported_platform()?;
    let socket = socket.unwrap_or(default_socket_path()?);
    if !daemon_is_live(&socket) {
        serve(Some(socket.clone()), false)?;
    }
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("connect to remote-agent at {}", socket.display()))?;
    proxy_stdio(&mut stream)
}

/// Proxy SSH stdio and the daemon socket from one thread. In particular, do
/// not read SSH stdin from a helper thread: Coder command sessions can leave
/// that reader asleep while stdout is waiting for the protocol handshake.
fn proxy_stdio(stream: &mut UnixStream) -> Result<()> {
    let mut stdin_open = true;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: if stdin_open { libc::POLLIN } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("poll remote-agent bridge");
        }

        let socket_events = descriptors[1].revents;
        if socket_events & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match stream.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    let mut stdout = io::stdout().lock();
                    stdout.write_all(&buffer[..count])?;
                    stdout.flush()?;
                },
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
                Err(error) => return Err(error).context("read remote-agent daemon output"),
            }
        }

        let stdin_events = descriptors[0].revents;
        if stdin_open && stdin_events & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let count =
                unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count == 0 {
                stdin_open = false;
                stream.shutdown(Shutdown::Write)?;
            } else if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error).context("read remote-agent SSH input");
                }
            } else {
                stream.write_all(&buffer[..count as usize])?;
                stream.flush()?;
            }
        }
    }
}

/// Present one remote pane as an ordinary local PTY process. The Flock server
/// can render it using its normal terminal path while the actual shell remains
/// owned by the remote daemon. Transport failure only ends the transport
/// child; the bridge reconnects and attaches to the same UUID.
pub fn remote_pty(
    transport: RemoteTransport,
    pane_id: Option<&str>,
    cwd: Option<PathBuf>,
) -> Result<()> {
    // Like ssh, the local bridge must bypass its own PTY line discipline. The
    // remote shell already owns a PTY and is solely responsible for echo,
    // completion and control-key handling.
    let _raw_terminal = RawTerminalGuard::enter(libc::STDIN_FILENO)?;
    let requested_id = pane_id
        .map(Uuid::parse_str)
        .transpose()
        .context("invalid pane UUID")?;
    let pane_id = match requested_id {
        Some(id) => id,
        None => {
            let session = std::env::var("FLOCK_SESSION_NAME").unwrap_or_else(|_| "flock".into());
            let local_pane = std::env::var("FLOCK_PANE_ID")
                .ok()
                .and_then(|id| id.parse().ok())
                .unwrap_or(0);
            Uuid::parse_str(&zellij_utils::data::stable_remote_pane_uuid(
                transport.identity(),
                &session,
                zellij_utils::data::PaneId::Terminal(local_pane),
            ))?
        },
    };
    let agent_state_tx = start_local_agent_state_forwarder();
    let input_router = Arc::new(InputRouter::new(pane_id));
    let stdin_router = input_router.clone();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buffer = [0; 8192];
        while let Ok(count) = stdin.read(&mut buffer) {
            if count == 0 {
                break;
            }
            stdin_router.forward(buffer[..count].to_vec());
        }
    });
    // Only a serialized command carrying an explicit UUID is a resurrection.
    // A new pane derives a stable UUID too, but must create first; treating that
    // UUID as proof of prior existence makes first-connect failures look like
    // reconnects and can attach a stale daemon pane after local id reuse.
    let (mut attach_first, mut has_connected) = initial_connection_mode(requested_id.is_some());
    let cursor_path = local_cursor_path(pane_id)?;
    persist_connection(&cursor_path, "connecting")?;
    let mut cursor = fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|cursor| cursor.trim().parse().ok())
        .unwrap_or(0);
    let mut delay = Duration::from_millis(250);
    // Health carries across reconnects: a protocol rejection learned at the
    // first handshake must survive the retries that follow it.
    let mut health = RemotePaneHealth::default();

    loop {
        match run_remote_transport(
            &transport,
            pane_id,
            attach_first,
            cursor,
            &cursor_path,
            cwd.as_deref(),
            input_router.clone(),
            agent_state_tx.as_ref(),
            &mut health,
        ) {
            Ok(TransportEnd::Exited(status)) => {
                persist_connection(&cursor_path, "disconnected")?;
                std::process::exit(status.unwrap_or(1))
            },
            Ok(TransportEnd::Disconnected { last_cursor, error }) => {
                has_connected = true;
                attach_first = true;
                cursor = last_cursor;
                health.retry_count = health.retry_count.saturating_add(1);
                health.last_error = Some(humanize_transport_error(&error));
                persist_health(&cursor_path, &health);
            },
            Err(error) => {
                health.retry_count = health.retry_count.saturating_add(1);
                health.last_error = Some(humanize_transport_error(&error));
                persist_health(&cursor_path, &health);
                // A stopped container can be revived locally; run the
                // transport's recovery (idempotent `devcontainer up`) before
                // retrying. Only fires on the failure path, so transient
                // daemon reconnects never pay for it.
                if let Some(mut recover) = transport.recover_command() {
                    let output = recover
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::piped())
                        .output();
                    if let Ok(output) = output {
                        if !output.status.success() {
                            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                            if !detail.is_empty() {
                                health.last_error = Some(detail);
                                persist_health(&cursor_path, &health);
                            }
                        }
                    }
                }
            },
        }
        persist_connection(&cursor_path, retry_connection_state(has_connected))?;
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

/// Submit one integration hook event to the per-user daemon. This command runs
/// inside the remote workspace and exits only after the daemon has retained the
/// event, so a short-lived hook cannot race its own process exit.
pub fn report_state(
    pane_id: &str,
    state: &str,
    agent: &str,
    agent_pid: Option<u32>,
    socket: Option<PathBuf>,
) -> Result<()> {
    let pane_id = Uuid::parse_str(pane_id).context("invalid pane UUID")?;
    let state = RemoteAgentRunState::parse(state)?;
    validate_agent_label(agent)?;
    let socket = socket.unwrap_or(default_socket_path()?);
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("connect to remote-agent at {}", socket.display()))?;
    let mut reader = stream.try_clone()?;
    write_frame(
        &mut stream,
        &ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    match read_frame::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            ..
        } => {},
        ServerMessage::Error { message } => bail!("{message}"),
        response => bail!("unexpected report-state handshake: {response:?}"),
    }
    write_frame(
        &mut stream,
        &ClientMessage::ReportAgentState {
            pane_id,
            state,
            agent: agent.to_owned(),
            agent_pid,
        },
    )?;
    loop {
        match read_frame::<_, ServerMessage>(&mut reader)? {
            ServerMessage::AgentStateAccepted { pane_id: accepted } if accepted == pane_id => {
                return Ok(())
            },
            ServerMessage::Error { message } => bail!("{message}"),
            _ => {},
        }
    }
}

fn validate_agent_label(agent: &str) -> Result<()> {
    if agent.trim().is_empty() || agent.contains([',', '=']) {
        bail!("agent label must be non-empty and cannot contain ',' or '='");
    }
    Ok(())
}

/// Bring a remote host onto this build: reinstall the agent binary, then ask
/// the running daemon to stand down once it no longer owns any live PTYs.
///
/// Nothing here closes a pane. Consequently an in-use daemon cannot activate
/// the new binary immediately: it keeps serving its existing PTYs and exits
/// only after the last pane closes. A failed reinstall leaves the old daemon
/// exactly as it was.
pub fn remote_upgrade(
    transport: RemoteTransport,
    reinstall_script: &str,
    force: bool,
) -> Result<()> {
    let mut install = transport.script_command(reinstall_script);
    let output = install
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("run the {} reinstall", transport.label()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("remote install failed");
        bail!("{detail}");
    }
    if force {
        request_force_restart(&transport)?;
        eprintln!(
            "flock: {} installed and activated; the previous daemon and its live panes were terminated",
            transport.label()
        );
        return Ok(());
    }
    // The reinstall is the part that must succeed. Retirement is best-effort:
    // if the daemon is already gone, or refuses the message because it predates
    // it, the new binary is still installed and the next connect picks it up.
    match request_retire(&transport) {
        Ok(panes_held) => {
            if panes_held > 0 {
                eprintln!(
                    "flock: {} installed; {panes_held} live pane(s) remain on the previous daemon until they close",
                    transport.label()
                );
            }
            Ok(())
        },
        Err(error) => {
            eprintln!(
                "flock: {} installed, but the running daemon did not accept the retire request ({error:#}); its live panes remain on the previous build until that daemon restarts",
                transport.label()
            );
            Ok(())
        },
    }
}

/// Run the newly installed binary on the remote host to terminate the daemon
/// behind its private socket. This command is intentionally separate from the
/// transport bridge: killing the daemon drops every bridge connection.
fn request_force_restart(transport: &RemoteTransport) -> Result<()> {
    let script = r#"exec "$HOME/.local/share/flock/current/flock" remote-agent force-restart"#;
    let mut command = transport.script_command(script);
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("force restart the {} daemon", transport.label()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("remote daemon restart failed");
    bail!("{detail}")
}

/// Terminate the daemon listening on `socket`. The peer PID comes from kernel
/// credentials on the connected Unix socket; no process-name search or pidfile
/// can accidentally target an unrelated process. Existing PTYs die with the
/// daemon, and their local bridges reconnect through the newly installed
/// binary, creating fresh remote shells.
pub fn force_restart(socket: Option<PathBuf>) -> Result<()> {
    require_supported_platform()?;
    let socket = socket.unwrap_or(default_socket_path()?);
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        },
        Err(error) => {
            return Err(error)
                .with_context(|| format!("connect to remote-agent at {}", socket.display()));
        },
    };
    let pid = peer_pid(&stream).context("remote-agent socket did not expose its daemon pid")?;
    validate_force_restart_target(&mut stream)?;
    terminate_daemon(pid)
}

fn validate_force_restart_target(stream: &mut UnixStream) -> Result<()> {
    write_frame(
        &mut *stream,
        &ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    match read_frame::<_, ServerMessage>(stream)? {
        ServerMessage::Hello { .. } => Ok(()),
        ServerMessage::Error { message } if message.contains("incompatible protocol") => Ok(()),
        response => bail!("unexpected remote-agent handshake: {response:?}"),
    }
}

fn terminate_daemon(pid: libc::pid_t) -> Result<()> {
    if pid <= 1 || pid == std::process::id() as libc::pid_t {
        bail!("refusing to terminate invalid daemon pid {pid}");
    }
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("terminate the remote-agent daemon");
        }
        return Ok(());
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        if !process_is_alive(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("kill the unresponsive remote-agent daemon");
        }
    }
    Ok(())
}

/// Ask the daemon to retire, returning how many panes it is still holding.
fn request_retire(transport: &RemoteTransport) -> Result<usize> {
    let mut child = transport
        .connect_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = exchange_retire_frames(&mut child);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn exchange_retire_frames(child: &mut std::process::Child) -> Result<usize> {
    let mut writer = child.stdin.take().context("open retire stdin")?;
    let mut reader = child.stdout.take().context("open retire stdout")?;
    write_frame(
        &mut writer,
        &ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    match read_frame::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            ..
        } => {},
        // An idle daemon that already noticed the new install retires itself at
        // hello and says so; that is the outcome we were asking for.
        ServerMessage::Error { message } if message.contains("retiring") => return Ok(0),
        response => bail!("unexpected retire handshake: {response:?}"),
    }
    write_frame(&mut writer, &ClientMessage::Retire)?;
    loop {
        match read_frame::<_, ServerMessage>(&mut reader)? {
            ServerMessage::Retiring { panes_held } => return Ok(panes_held),
            ServerMessage::Error { message } => bail!("{message}"),
            _ => {},
        }
    }
}

pub fn remote_close(transport: RemoteTransport, pane_id: &str) -> Result<()> {
    let pane_id = Uuid::parse_str(pane_id).context("invalid pane UUID")?;
    let cursor_path = local_cursor_path(pane_id)?;
    let pending_path = cursor_path.with_extension("close-pending");
    if let Some(parent) = pending_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !pending_path.exists() {
        let mut pending = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending_path)?;
        pending.write_all(serde_json::to_string(&transport.close_spec())?.as_bytes())?;
        pending.sync_all()?;
    }
    let _worker = match CloseWorkerGuard::acquire(cursor_path.with_extension("close-running"))? {
        Some(worker) => worker,
        None => return Ok(()),
    };
    // Containers can be gone for good (stopped/removed — the daemon and every
    // pane shell died with them), which counts as done, and can also fail in
    // ways that never converge, so their retries are bounded and the pending
    // file is left behind for re-arm at the next flock start.
    const BOUNDED_CLOSE_ATTEMPTS: u32 = 20;
    let mut delay = Duration::from_millis(250);
    let mut attempts = 0u32;
    loop {
        match send_remote_close(&transport, pane_id) {
            Ok(()) => {},
            Err(error) if transport.close_target_gone(&format!("{error:#}")) => {
                eprintln!("flock: remote close target is gone ({error:#}); treating as closed");
            },
            Err(error) => {
                attempts += 1;
                if transport.close_retries_bounded() && attempts >= BOUNDED_CLOSE_ATTEMPTS {
                    bail!(
                        "remote pane close did not converge after {attempts} attempts ({error:#}); leaving the pending request for the next start"
                    );
                }
                eprintln!("flock: remote pane close pending ({error:#}); retrying");
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(10));
                continue;
            },
        }
        let _ = fs::remove_file(&pending_path);
        let _ = fs::remove_file(&cursor_path);
        let _ = fs::remove_file(cursor_path.with_extension("foreground"));
        let _ = fs::remove_file(cursor_path.with_extension("cwd"));
        let _ = fs::remove_file(cursor_path.with_extension("connection"));
        return Ok(());
    }
}

struct CloseWorkerGuard {
    path: PathBuf,
}

impl CloseWorkerGuard {
    fn acquire(path: PathBuf) -> Result<Option<Self>> {
        for _ in 0..3 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    write!(file, "{}", std::process::id())?;
                    file.sync_all()?;
                    return Ok(Some(Self { path }));
                },
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let live = fs::read_to_string(&path)
                        .ok()
                        .and_then(|pid| pid.trim().parse::<i32>().ok())
                        .is_some_and(|pid| close_worker_is_live(pid, &path));
                    if live {
                        return Ok(None);
                    }
                    match fs::remove_file(&path) {
                        Ok(()) => {},
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                        Err(error) => return Err(error.into()),
                    }
                },
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }
}

fn close_worker_is_live(pid: i32, lock_path: &Path) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    let pane_uuid = lock_path.file_stem().and_then(|stem| stem.to_str());
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|command| String::from_utf8_lossy(&command).replace('\0', " "))
        .is_some_and(|command| {
            command.contains("remote-close")
                && pane_uuid.is_some_and(|pane_uuid| command.contains(pane_uuid))
        })
}

impl Drop for CloseWorkerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn send_remote_close(transport: &RemoteTransport, pane_id: Uuid) -> Result<()> {
    let mut child = transport
        .connect_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = exchange_close_frames(&mut child, pane_id);
    if result.is_err() {
        // Attach the transport's own words to the failure — the caller
        // classifies container-gone messages, and a bare EOF says nothing.
        let _ = child.kill();
        let stderr = child
            .stderr
            .take()
            .and_then(|mut stderr| {
                let mut buffer = String::new();
                stderr.read_to_string(&mut buffer).ok()?;
                Some(buffer)
            })
            .unwrap_or_default();
        let _ = child.wait();
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            return result.with_context(|| format!("transport reported: {stderr}"));
        }
    }
    result
}

fn exchange_close_frames(child: &mut std::process::Child, pane_id: Uuid) -> Result<()> {
    let mut writer = child.stdin.take().context("open remote close stdin")?;
    let mut reader = child.stdout.take().context("open remote close stdout")?;
    write_frame(
        &mut writer,
        &ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    match read_frame::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            ..
        } => {},
        response => bail!("unexpected close handshake: {response:?}"),
    }
    write_frame(&mut writer, &ClientMessage::ClosePane { pane_id })?;
    loop {
        match read_frame::<_, ServerMessage>(&mut reader)? {
            ServerMessage::PaneClosed { pane_id: closed } if closed == pane_id => return Ok(()),
            ServerMessage::Error { message } => bail!("{message}"),
            _ => {},
        }
    }
}

enum TransportEnd {
    Disconnected {
        last_cursor: u64,
        error: anyhow::Error,
    },
    Exited(Option<i32>),
}

fn initial_connection_mode(has_saved_pane_id: bool) -> (bool, bool) {
    // A saved UUID means this command came from session serialization: attach
    // first and describe failures as reconnects. A derived UUID belongs to a
    // genuinely new local pane: create first and remain "connecting" until the
    // daemon confirms it.
    (has_saved_pane_id, has_saved_pane_id)
}

fn retry_connection_state(has_connected: bool) -> &'static str {
    if has_connected {
        "reconnecting"
    } else {
        "connecting"
    }
}

const DIAGNOSTIC_TAIL_BYTES: usize = 16 * 1024;

/// Drain provider stderr without ever wiring it to the pane's slave PTY. SSH,
/// Coder and devcontainer can all emit diagnostics while stdout is carrying the
/// framed protocol; inheriting stderr splices those words into a full-screen
/// remote application and corrupts its rendered grid.
fn capture_diagnostics(mut stderr: impl Read + Send + 'static) -> Arc<Mutex<VecDeque<u8>>> {
    let captured = Arc::new(Mutex::new(VecDeque::with_capacity(DIAGNOSTIC_TAIL_BYTES)));
    let writer = captured.clone();
    thread::spawn(move || {
        let mut buffer = [0; 1024];
        while let Ok(count) = stderr.read(&mut buffer) {
            if count == 0 {
                break;
            }
            let mut tail = writer.lock().unwrap();
            tail.extend(&buffer[..count]);
            while tail.len() > DIAGNOSTIC_TAIL_BYTES {
                tail.pop_front();
            }
        }
    });
    captured
}

fn diagnostic_tail(captured: &Arc<Mutex<VecDeque<u8>>>) -> Option<String> {
    let bytes: Vec<u8> = captured.lock().unwrap().iter().copied().collect();
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn remote_attach_requests(
    pane_id: Uuid,
    after_sequence: u64,
    cols: u16,
    rows: u16,
) -> [ClientMessage; 2] {
    [
        ClientMessage::Resize {
            pane_id,
            cols,
            rows,
        },
        ClientMessage::AttachPane {
            pane_id,
            after_sequence,
        },
    ]
}

fn run_remote_transport(
    transport: &RemoteTransport,
    pane_id: Uuid,
    attach_first: bool,
    cursor: u64,
    cursor_path: &Path,
    cwd: Option<&Path>,
    input_router: Arc<InputRouter>,
    agent_state_tx: Option<&mpsc::Sender<RemoteAgentStateEvent>>,
    health: &mut RemotePaneHealth,
) -> Result<TransportEnd> {
    let mut child = transport
        .connect_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {} remote-agent transport", transport.label()))?;
    let diagnostics = child.stderr.take().map(capture_diagnostics);
    let result = run_remote_transport_io(
        &mut child,
        pane_id,
        attach_first,
        cursor,
        cursor_path,
        cwd,
        input_router,
        agent_state_tx,
        health,
    );
    if result.is_err()
        || matches!(
            &result,
            Ok(TransportEnd::Disconnected {
                last_cursor: _,
                error: _
            })
        )
    {
        let _ = child.kill();
        let _ = child.wait();
    }
    match (result, diagnostics.as_ref().and_then(diagnostic_tail)) {
        (Err(error), Some(diagnostic)) => {
            Err(error.context(format!("{} transport: {diagnostic}", transport.label())))
        },
        (Ok(TransportEnd::Disconnected { last_cursor, error }), Some(diagnostic)) => {
            Ok(TransportEnd::Disconnected {
                last_cursor,
                error: error.context(format!("{} transport: {diagnostic}", transport.label())),
            })
        },
        (result, _) => result,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_remote_transport_io(
    child: &mut std::process::Child,
    pane_id: Uuid,
    attach_first: bool,
    cursor: u64,
    cursor_path: &Path,
    cwd: Option<&Path>,
    input_router: Arc<InputRouter>,
    agent_state_tx: Option<&mpsc::Sender<RemoteAgentStateEvent>>,
    health: &mut RemotePaneHealth,
) -> Result<TransportEnd> {
    let child_stdin: TransportWriter = Arc::new(Mutex::new(Box::new(
        child.stdin.take().context("open transport stdin")?,
    )));
    let mut child_stdout = child.stdout.take().context("open transport stdout")?;
    write_frame(
        &mut *child_stdin.lock().unwrap(),
        &ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    match read_frame::<_, ServerMessage>(&mut child_stdout)? {
        ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            agent_version,
        } => {
            // Same protocol, different build: the daemon kept running because
            // it still holds panes (an idle daemon retires itself at hello).
            // Record the skew so the sidebar can offer the upgrade; the pane
            // itself keeps working, so nothing is written into the terminal.
            *health = RemotePaneHealth {
                status: if agent_version == env!("CARGO_PKG_VERSION") {
                    RemoteProtocolStatus::Ok
                } else {
                    RemoteProtocolStatus::VersionSkew
                },
                daemon_version: Some(agent_version.clone()),
                local_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                retry_count: 0,
                last_error: None,
            };
            persist_health(cursor_path, health);
        },
        ServerMessage::Error { message } => {
            // A protocol rejection is terminal: the pane cannot be served by
            // this daemon at all, and only a reinstall changes that. Record it
            // before bailing so the sidebar can offer the one useful action.
            if message.contains("incompatible protocol version") {
                *health = RemotePaneHealth {
                    status: RemoteProtocolStatus::ProtocolIncompatible,
                    daemon_version: None,
                    local_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                    retry_count: health.retry_count,
                    last_error: Some(message.clone()),
                };
                persist_health(cursor_path, health);
            }
            bail!("{message}")
        },
        message => bail!("unexpected handshake response: {message:?}"),
    }

    let (cols, rows) = local_terminal_size();
    if attach_first {
        // A resurrected local pane can have different dimensions from the PTY
        // the daemon kept alive. Resize before subscribing to replay: terminal
        // UIs emit cursor-addressed redraws for their old geometry while
        // detached, and replaying those into the new geometry leaves stacked
        // artifacts (eg. devenv's repeated "watching … files" status).
        for request in remote_attach_requests(pane_id, cursor, cols, rows) {
            write_frame(&mut *child_stdin.lock().unwrap(), &request)?;
        }
    } else {
        write_frame(
            &mut *child_stdin.lock().unwrap(),
            &ClientMessage::CreatePane {
                pane_id: Some(pane_id),
                cols,
                rows,
                env: minimal_terminal_env(),
                cwd: cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
            },
        )?;
    }
    // Do not forward queued keystrokes until the daemon confirms that this
    // pane exists. Otherwise an eager Input can overtake the unknown-pane
    // response to AttachPane, producing duplicate CreatePane requests and
    // permanently discarding the first keystrokes.
    let mut create_sent = !attach_first;
    let mut attach_sent = attach_first;
    let mut last_cursor = cursor;
    let mut truncated_replay = None;
    loop {
        match read_frame::<_, ServerMessage>(&mut child_stdout)? {
            ServerMessage::Attached {
                pane_id: attached,
                next_sequence,
            } if attached == pane_id => {
                // A truncated byte stream starts in the middle of arbitrary VT
                // state and cannot safely reconstruct a screen. Older daemons
                // still send their retained chunks after ReplayTruncated, so
                // the bridge skips them and advances to the coherent live
                // watermark. New daemons omit them at the source.
                last_cursor = next_sequence.saturating_sub(1);
                persist_cursor(cursor_path, last_cursor)?;
                write_frame(
                    &mut *child_stdin.lock().unwrap(),
                    &ClientMessage::Acknowledge {
                        pane_id,
                        sequence: last_cursor,
                    },
                )?;
                if let Some(first_available) = truncated_replay {
                    // Reset terminal state after skipping a byte stream that may
                    // begin mid-control-sequence. The diagnostic belongs in
                    // health/UI; never splice human-readable bridge text into
                    // the remote application's PTY stream.
                    write!(io::stdout().lock(), "\x1b[!p\x1b[2J\x1b[H",)?;
                    io::stdout().lock().flush()?;
                    health.last_error = Some(format!(
                        "detached output before sequence {first_available} was truncated"
                    ));
                    persist_health(cursor_path, health);
                }
                break;
            },
            ServerMessage::PaneCreated {
                pane_id: created, ..
            } if created == pane_id => break,
            ServerMessage::Output {
                pane_id: output_pane,
                sequence,
                data,
            } if output_pane == pane_id => {
                if truncated_replay.is_none() {
                    io::stdout().lock().write_all(&data)?;
                    io::stdout().lock().flush()?;
                }
                last_cursor = sequence;
                write_frame(
                    &mut *child_stdin.lock().unwrap(),
                    &ClientMessage::Acknowledge { pane_id, sequence },
                )?;
                persist_cursor(cursor_path, sequence)?;
            },
            ServerMessage::ReplayTruncated {
                first_available, ..
            } => {
                truncated_replay = Some(first_available);
            },
            ServerMessage::Error { message }
                if attach_first && !create_sent && message.contains("unknown pane") =>
            {
                write_frame(
                    &mut *child_stdin.lock().unwrap(),
                    &ClientMessage::CreatePane {
                        pane_id: Some(pane_id),
                        cols,
                        rows,
                        env: minimal_terminal_env(),
                        cwd: cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
                    },
                )?;
                create_sent = true;
            },
            // Creation is idempotent across an ambiguous transport failure: if
            // the daemon created the pane but the acknowledgement was lost, a
            // fresh bridge retries CreatePane and receives this response. Attach
            // to that exact UUID rather than spawning a duplicate or retrying
            // forever. This is deliberately not the normal new-pane path.
            ServerMessage::Error { message }
                if create_sent && !attach_sent && message.contains("already exists") =>
            {
                for request in remote_attach_requests(pane_id, cursor, cols, rows) {
                    write_frame(&mut *child_stdin.lock().unwrap(), &request)?;
                }
                attach_sent = true;
            },
            ServerMessage::Error { message } => bail!("{message}"),
            ServerMessage::Exited { status, .. } => {
                return Ok(TransportEnd::Exited(status));
            },
            ServerMessage::AgentStateChanged { event } if event.pane_id == pane_id => {
                if let Some(agent_state_tx) = agent_state_tx {
                    let _ = agent_state_tx.send(event);
                }
            },
            _ => {},
        }
    }
    persist_connection(cursor_path, "connected")?;
    write_frame(
        &mut *child_stdin.lock().unwrap(),
        &ClientMessage::ForegroundProcess { pane_id },
    )?;

    let _active_input_writer = input_router.activate(child_stdin.clone());

    let resize_writer = child_stdin.clone();
    let resize_active = Arc::new(AtomicBool::new(true));
    let resize_thread_active = resize_active.clone();
    thread::spawn(move || {
        let mut last_size = (cols, rows);
        let mut foreground_tick = 0u8;
        while resize_thread_active.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
            let size = local_terminal_size();
            if size != last_size {
                last_size = size;
                if write_frame(
                    &mut *resize_writer.lock().unwrap(),
                    &ClientMessage::Resize {
                        pane_id,
                        cols: size.0,
                        rows: size.1,
                    },
                )
                .is_err()
                {
                    break;
                }
            }
            foreground_tick = foreground_tick.wrapping_add(1);
            if foreground_tick % 4 == 0
                && write_frame(
                    &mut *resize_writer.lock().unwrap(),
                    &ClientMessage::ForegroundProcess { pane_id },
                )
                .is_err()
            {
                break;
            }
        }
    });

    loop {
        match read_frame::<_, ServerMessage>(&mut child_stdout) {
            Ok(ServerMessage::Output {
                pane_id: output_pane,
                sequence,
                data,
            }) if output_pane == pane_id => {
                io::stdout().lock().write_all(&data)?;
                io::stdout().lock().flush()?;
                last_cursor = sequence;
                write_frame(
                    &mut *child_stdin.lock().unwrap(),
                    &ClientMessage::Acknowledge { pane_id, sequence },
                )?;
                persist_cursor(cursor_path, sequence)?;
            },
            Ok(ServerMessage::ReplayTruncated {
                first_available, ..
            }) => {
                health.last_error = Some(format!(
                    "remote output before sequence {first_available} was truncated"
                ));
                persist_health(cursor_path, health);
            },
            Ok(ServerMessage::Exited { status, .. }) => {
                resize_active.store(false, Ordering::Relaxed);
                // The local pane is held on bridge exit and Enter re-runs this
                // same UUID. Remove the completed remote PTY before returning
                // so that re-run's attach-first request receives "unknown
                // pane" and creates a fresh shell instead of attaching to the
                // same terminal Exited response forever.
                let _ = write_frame(
                    &mut *child_stdin.lock().unwrap(),
                    &ClientMessage::ClosePane { pane_id },
                );
                return Ok(TransportEnd::Exited(status));
            },
            Ok(ServerMessage::ForegroundProcess { argv, cwd, .. }) => {
                persist_foreground(cursor_path, &argv)?;
                if let Some(cwd) = cwd {
                    persist_cwd(cursor_path, &cwd)?;
                }
            },
            Ok(ServerMessage::AgentStateChanged { event }) if event.pane_id == pane_id => {
                if let Some(agent_state_tx) = agent_state_tx {
                    let _ = agent_state_tx.send(event);
                }
            },
            Ok(ServerMessage::Error { message }) => bail!("{message}"),
            Ok(_) => {},
            Err(error) => {
                resize_active.store(false, Ordering::Relaxed);
                let _ = child.kill();
                return Ok(TransportEnd::Disconnected { last_cursor, error });
            },
        }
    }
}

fn local_cursor_path(pane_id: Uuid) -> Result<PathBuf> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| anyhow!("HOME or XDG_CACHE_HOME is required"))?;
    Ok(root
        .join("flock")
        .join("remote-panes")
        .join(format!("{pane_id}.cursor")))
}

fn persist_cursor(path: &Path, cursor: u64) -> Result<()> {
    let parent = path.parent().context("remote cursor parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension(format!("cursor.{}", std::process::id()));
    fs::write(&temporary, cursor.to_string())?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn persist_foreground(cursor_path: &Path, argv: &[String]) -> Result<()> {
    let path = cursor_path.with_extension("foreground");
    let parent = path.parent().context("remote foreground parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("foreground.{}", std::process::id()));
    fs::write(&temporary, argv.join("\0"))?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn persist_cwd(cursor_path: &Path, cwd: &str) -> Result<()> {
    let path = cursor_path.with_extension("cwd");
    let parent = path.parent().context("remote cwd parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("cwd.{}", std::process::id()));
    fs::write(&temporary, cwd.as_bytes())?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn persist_connection(cursor_path: &Path, state: &str) -> Result<()> {
    let path = cursor_path.with_extension("connection");
    let parent = path.parent().context("remote connection parent")?;
    fs::create_dir_all(parent)?;
    fs::write(path, state)?;
    Ok(())
}

/// Write this pane's health picture next to its cursor. The server reads it
/// back on every session-info tick, so the sidebar's view self-clears the
/// moment the bridge writes a healthy record — no revocation message needed.
/// Health is advisory: a write failure must never break the transport, so
/// callers ignore the result.
fn persist_health(cursor_path: &Path, health: &RemotePaneHealth) {
    let path = cursor_path.with_extension("health");
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(encoded) = serde_json::to_string(health) else {
        return;
    };
    // Same write-then-rename the cursor uses: the server may read this file at
    // any moment and must never see a half-written record.
    let temporary = path.with_extension(format!("health.{}", std::process::id()));
    if fs::write(&temporary, encoded).is_ok() {
        let _ = fs::rename(&temporary, &path);
    }
}

fn persist_bridge_diagnostic(pane_id: Uuid, detail: String) {
    let Ok(cursor_path) = local_cursor_path(pane_id) else {
        return;
    };
    let health_path = cursor_path.with_extension("health");
    let mut health = fs::read_to_string(health_path)
        .ok()
        .and_then(|encoded| serde_json::from_str::<RemotePaneHealth>(&encoded).ok())
        .unwrap_or_default();
    health.last_error = Some(detail);
    persist_health(&cursor_path, &health);
}

/// Strip the noise off an `anyhow` chain so the sidebar can render the cause on
/// one narrow row. Keeps the outermost message, which is the one written for a
/// human.
fn humanize_transport_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    let message = message.trim();
    if message.is_empty() {
        "connection failed".to_owned()
    } else {
        message.to_owned()
    }
}

fn start_local_agent_state_forwarder() -> Option<mpsc::Sender<RemoteAgentStateEvent>> {
    let pane_id = std::env::var("FLOCK_PANE_ID").ok()?;
    pane_id.parse::<u32>().ok()?;
    let executable = std::env::var_os("FLOCK_EXECUTABLE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())?;
    let (tx, rx) = mpsc::channel::<RemoteAgentStateEvent>();
    thread::spawn(move || {
        for event in rx {
            if let Err(error) = forward_agent_state_to_local_plugin(&executable, &pane_id, &event) {
                persist_bridge_diagnostic(
                    event.pane_id,
                    format!("failed to forward remote agent state: {error:#}"),
                );
            }
        }
    });
    Some(tx)
}

fn local_agent_state_args(pane_id: &str, event: &RemoteAgentStateEvent) -> String {
    format!(
        "pane_id={pane_id},state={},agent={},source=flock:coder-remote,presence={}",
        event.state.as_str(),
        event.agent,
        // The sidebar must distinguish the daemon re-asserting an unchanged
        // picture from the agent reporting a transition; see `HookReport`.
        if event.heartbeat {
            "heartbeat"
        } else {
            "report"
        },
    )
}

fn forward_agent_state_to_local_plugin(
    executable: &Path,
    pane_id: &str,
    event: &RemoteAgentStateEvent,
) -> Result<()> {
    let args = local_agent_state_args(pane_id, event);
    publish_local_pipe(executable, "flock-state", &args)
}

fn publish_local_pipe(executable: &Path, pipe_name: &str, args: &str) -> Result<()> {
    let mut child = Command::new(executable)
        .args(["pipe", "--name", pipe_name, "--args", args])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start local {pipe_name} publisher"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("local {pipe_name} publisher exited with {status}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("local {pipe_name} publisher timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn minimal_terminal_env() -> HashMap<String, String> {
    ["TERM", "COLORTERM", "LANG", "LC_ALL", "LC_CTYPE"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect()
}

fn local_terminal_size() -> (u16, u16) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
        && size.ws_row > 0
    {
        (size.ws_col, size.ws_row)
    } else {
        (80, 24)
    }
}

fn require_supported_platform() -> Result<()> {
    if std::env::consts::OS != "linux" || !matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
        bail!("unsupported platform: remote sessions require Linux x86_64 or aarch64");
    }
    Ok(())
}

/// Idempotently install the bundled OpenCode state plugin into the daemon
/// host's user config (compare-and-swap, like the old devcontainer wrapper
/// did per pane). The daemon is the one process guaranteed to run on every
/// remote host regardless of transport, so hook installation lives here and
/// uniformly benefits coder, ssh, and devcontainer sessions. Claude/codex
/// hooks stay dotfiles-managed.
fn install_opencode_state_plugin() -> Result<()> {
    const PLUGIN: &str =
        include_str!("../default-plugins/flock-sidebar/assets/opencode/flock-agent-state.js");
    let config_root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("HOME or XDG_CONFIG_HOME is required")?;
    let directory = config_root.join("opencode").join("plugins");
    let target = directory.join("flock-agent-state.js");
    if fs::read_to_string(&target).is_ok_and(|current| current == PLUGIN) {
        return Ok(());
    }
    fs::create_dir_all(&directory)?;
    let temporary = directory.join(format!(".flock-agent-state.js.tmp.{}", std::process::id()));
    fs::write(&temporary, PLUGIN)?;
    fs::rename(&temporary, &target)?;
    Ok(())
}

fn default_socket_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| anyhow!("HOME or XDG_RUNTIME_DIR is required"))?;
    Ok(runtime
        .join("flock")
        .join(format!("remote-agent-v{PROTOCOL_VERSION}.sock")))
}

fn daemon_is_live(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// Whether the bootstrap install (`~/.local/share/flock/current/flock`, the
/// path every transport connects through) is a different file than the
/// running daemon. Deliberately not a version-string comparison against the
/// client: when the remote install is stale, retiring on a client mismatch
/// would respawn the same old binary and retire again, forever. Comparing
/// against the file on disk only retires when a respawn actually changes
/// something. Device+inode identity survives the in-place `mv` replace that
/// leaves `/proc/self/exe` pointing at a deleted path.
/// Set once a client has asked this daemon to stand down. A daemon holding
/// panes cannot exit without killing their PTYs, so retirement waits for the
/// last one to close.
static RETIRE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Exit if a retirement is pending and the last pane has just gone. Called
/// wherever a pane leaves the map.
fn exit_if_retiring_and_idle(panes: &Panes) {
    if RETIRE_REQUESTED.load(Ordering::SeqCst) && panes.lock().unwrap().is_empty() {
        std::process::exit(0);
    }
}

fn installed_binary_differs() -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let installed = Path::new(&home)
        .join(".local/share/flock")
        .join("current")
        .join("flock");
    match (
        file_identity(&installed),
        file_identity(Path::new("/proc/self/exe")),
    ) {
        (Some(installed), Some(running)) => installed != running,
        // Missing install symlink or unreadable self: unknown territory
        // (dev daemons, non-bootstrap installs) — never retire on a guess.
        _ => false,
    }
}

fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if daemon_is_live(socket) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!("remote-agent daemon did not become ready")
}

fn handle_client(stream: UnixStream, panes: Panes) -> Result<()> {
    let mut reader = stream.try_clone()?;
    // Captured before any framing: an agent state report identifies the process
    // to watch by its own ancestry, and the kernel's answer for who is on the
    // other end of this socket is the trustworthy starting point.
    let reporter_pid = peer_pid(&stream);
    let mut direct_writer = stream;

    let hello: ClientMessage = read_frame(&mut reader)?;
    match hello {
        ClientMessage::Hello { protocol, .. } if protocol == PROTOCOL_VERSION => {
            // With no panes there is nothing to lose: retire so the client's
            // reconnect respawns the daemon from the newly installed binary.
            // A daemon with panes must keep running (its PTYs die with it);
            // those clients get a version notice instead.
            if panes.lock().unwrap().is_empty() && installed_binary_differs() {
                let _ = write_frame(
                    &mut direct_writer,
                    &ServerMessage::Error {
                        message: format!(
                            "remote daemon v{} retiring: a different flock build is installed and reconnecting starts it",
                            env!("CARGO_PKG_VERSION"),
                        ),
                    },
                );
                std::process::exit(0);
            }
            write_frame(
                &mut direct_writer,
                &ServerMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                },
            )?;
        },
        ClientMessage::Hello { protocol, .. } => {
            let error = ensure_protocol_version(protocol).unwrap_err().to_string();
            write_frame(&mut direct_writer, &ServerMessage::Error { message: error })?;
            bail!("incompatible protocol version {protocol}");
        },
        _ => bail!("the first frame must be a hello"),
    }

    let (tx, rx) = mpsc::channel::<ServerMessage>();
    thread::spawn(move || -> Result<()> {
        for message in rx {
            write_frame(&mut direct_writer, &message)?;
        }
        Ok(())
    });

    while let Ok(message) = read_frame::<_, ClientMessage>(&mut reader) {
        match message {
            ClientMessage::Hello { .. } => {
                tx.send(ServerMessage::Error {
                    message: "duplicate hello".into(),
                })?;
            },
            ClientMessage::CreatePane {
                pane_id,
                cols,
                rows,
                env,
                cwd,
            } => {
                let id = pane_id.unwrap_or_else(Uuid::new_v4);
                let mut panes = panes.lock().unwrap();
                if panes.contains_key(&id) {
                    tx.send(ServerMessage::Error {
                        message: format!("pane {id} already exists"),
                    })?;
                    continue;
                }
                let pane = spawn_pane(id, cols, rows, env, cwd.as_deref().map(Path::new))?;
                pane.subscribers.lock().unwrap().push(tx.clone());
                panes.insert(id, pane.clone());
                drop(panes);
                tx.send(ServerMessage::PaneCreated {
                    pane_id: id,
                    pid: pane.pid as u32,
                })?;
                start_pane_threads(pane);
            },
            ClientMessage::AttachPane {
                pane_id,
                after_sequence,
            } => {
                let Some(pane) = panes.lock().unwrap().get(&pane_id).cloned() else {
                    tx.send(ServerMessage::Error {
                        message: format!("unknown pane {pane_id}"),
                    })?;
                    continue;
                };
                // Settle presence against the agent's process before replaying
                // it, so a client attaching after the agent exited is told the
                // truth rather than the last state the agent managed to report.
                // No heartbeat: `subscribe_with_replay` sends the picture below.
                pane.settle_presence(false);
                pane.subscribe_with_replay(after_sequence, &tx)?;
                let exit_status = *pane.exit_status.lock().unwrap();
                if let Some(status) = exit_status {
                    tx.send(ServerMessage::Exited { pane_id, status })?;
                }
            },
            ClientMessage::Input { pane_id, data } => with_pane(&panes, pane_id, &tx, |pane| {
                pane.master
                    .lock()
                    .unwrap()
                    .write_all(&data)
                    .context("write PTY input")
            })?,
            ClientMessage::Resize {
                pane_id,
                cols,
                rows,
            } => with_pane(&panes, pane_id, &tx, |pane| {
                set_winsize(pane.master.lock().unwrap().as_raw_fd(), cols, rows)
            })?,
            ClientMessage::Acknowledge { .. } => {},
            // Purely informational: the bridge persists the pane's argv and cwd
            // so a reattach can restore them. Agent presence deliberately does
            // not consult the foreground process group — see `settle_presence`.
            ClientMessage::ForegroundProcess { pane_id } => {
                with_pane(&panes, pane_id, &tx, |pane| {
                    let (argv, cwd) = foreground_process(pane.master.lock().unwrap().as_raw_fd());
                    tx.send(ServerMessage::ForegroundProcess { pane_id, argv, cwd })?;
                    Ok(())
                })?
            },
            ClientMessage::ClosePane { pane_id } => {
                let pane = panes.lock().unwrap().remove(&pane_id);
                if let Some(pane) = pane {
                    unsafe { libc::kill(-pane.pid, libc::SIGHUP) };
                    tx.send(ServerMessage::PaneClosed { pane_id })?;
                } else {
                    tx.send(ServerMessage::PaneClosed { pane_id })?;
                }
                exit_if_retiring_and_idle(&panes);
            },
            ClientMessage::Retire => {
                RETIRE_REQUESTED.store(true, Ordering::SeqCst);
                let panes_held = panes.lock().unwrap().len();
                tx.send(ServerMessage::Retiring { panes_held })?;
                if panes_held == 0 {
                    // The reply goes out on the writer thread, so give it a
                    // moment to flush before the process disappears under it.
                    thread::spawn(|| {
                        thread::sleep(Duration::from_millis(200));
                        std::process::exit(0);
                    });
                }
            },
            ClientMessage::ReportAgentState {
                pane_id,
                state,
                agent,
                agent_pid,
            } => {
                if let Err(error) = validate_agent_label(&agent) {
                    tx.send(ServerMessage::Error {
                        message: error.to_string(),
                    })?;
                    continue;
                }
                with_pane(&panes, pane_id, &tx, |pane| {
                    pane.record_agent_state(
                        state,
                        agent,
                        reporter_pid,
                        agent_pid.map(|pid| pid as libc::pid_t),
                    );
                    tx.send(ServerMessage::AgentStateAccepted { pane_id })?;
                    Ok(())
                })?
            },
        }
    }
    drop(tx);
    Ok(())
}

fn with_pane<F>(panes: &Panes, id: Uuid, tx: &mpsc::Sender<ServerMessage>, f: F) -> Result<()>
where
    F: FnOnce(&Arc<PaneState>) -> Result<()>,
{
    if let Some(pane) = panes.lock().unwrap().get(&id).cloned() {
        f(&pane)
    } else {
        tx.send(ServerMessage::Error {
            message: format!("unknown pane {id}"),
        })?;
        Ok(())
    }
}

fn spawn_pane(
    id: Uuid,
    cols: u16,
    rows: u16,
    env: HashMap<String, String>,
    cwd: Option<&Path>,
) -> Result<Arc<PaneState>> {
    spawn_pane_with_shell(id, cols, rows, env, cwd, None)
}

fn spawn_pane_with_shell(
    id: Uuid,
    cols: u16,
    rows: u16,
    env: HashMap<String, String>,
    cwd: Option<&Path>,
    shell_override: Option<&Path>,
) -> Result<Arc<PaneState>> {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut winsize,
        )
    } < 0
    {
        return Err(io::Error::last_os_error()).context("open PTY");
    }
    let master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let shell = shell_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("SHELL").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"));
    let mut command = Command::new(&shell);
    command.arg("-l");
    command.envs(env.into_iter().filter(|(key, _)| allowed_env(key)));
    command.envs(controlled_remote_pane_env(id)?);
    if let Some(cwd) = cwd.filter(|cwd| cwd.is_dir()) {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(move || {
            if libc::close(master_fd) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY.into(), 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .with_context(|| format!("spawn PTY shell {:?}", shell))?;
    let pid = child.id() as libc::pid_t;
    drop(child);
    Ok(Arc::new(PaneState {
        id,
        pid,
        master: Mutex::new(master),
        history: Mutex::new(VecDeque::new()),
        history_bytes: Mutex::new(0),
        next_sequence: Mutex::new(1),
        subscribers: Mutex::new(Vec::new()),
        agent: Mutex::new(None),
        exit_status: Mutex::new(None),
    }))
}

/// Drive [`PaneState::settle_presence`] for every live pane.
///
/// One daemon-wide thread rather than one per pane: the work is a `/proc` stat
/// per tracked agent, and a single loop cannot leak a thread when a pane is
/// removed. Presence maintenance deliberately does *not* depend on a client
/// being attached — an agent that exits during a detach is released here, so
/// nothing stale is left to replay on the next attach.
fn start_presence_ticker(panes: Panes) {
    thread::spawn(move || {
        let mut tick: u32 = 0;
        loop {
            thread::sleep(PRESENCE_TICK);
            tick = tick.wrapping_add(1);
            // Every pane heartbeats on the same tick, so the forwarding cost is
            // one burst rather than a steady trickle.
            let broadcast_heartbeat = tick % HEARTBEAT_EVERY_TICKS == 0;
            let live: Vec<Arc<PaneState>> = panes.lock().unwrap().values().cloned().collect();
            for pane in live {
                pane.settle_presence(broadcast_heartbeat);
            }
        }
    });
}

use std::os::unix::process::CommandExt;

fn allowed_env(key: &str) -> bool {
    matches!(key, "TERM" | "COLORTERM" | "LANG" | "LC_ALL" | "LC_CTYPE")
}

fn controlled_remote_pane_env(id: Uuid) -> Result<HashMap<String, String>> {
    let executable = std::env::current_exe()
        .context("resolve remote Flock executable")?
        .to_string_lossy()
        .into_owned();
    Ok(HashMap::from_iter([
        ("FLOCK_PANE_ID".into(), id.to_string()),
        ("FLOCK_STATE_CHANNEL".into(), "remote-agent".into()),
        ("FLOCK_EXECUTABLE".into(), executable),
    ]))
}

fn start_pane_threads(pane: Arc<PaneState>) {
    let reader_pane = pane.clone();
    thread::spawn(move || {
        let mut reader = match reader_pane.master.lock().unwrap().try_clone() {
            Ok(reader) => reader,
            Err(_) => return,
        };
        let mut buf = vec![0; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => reader_pane.record_output(buf[..count].to_vec()),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }
        let mut status = 0;
        let waited = unsafe { libc::waitpid(reader_pane.pid, &mut status, 0) };
        let exit = if waited > 0 && libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else if waited > 0 && libc::WIFSIGNALED(status) {
            Some(128 + libc::WTERMSIG(status))
        } else {
            None
        };
        *reader_pane.exit_status.lock().unwrap() = Some(exit);
        reader_pane.publish(ServerMessage::Exited {
            pane_id: reader_pane.id,
            status: exit,
        });
    });
}

fn set_winsize(fd: i32, cols: u16, rows: u16) -> Result<()> {
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize) } < 0 {
        return Err(io::Error::last_os_error()).context("resize PTY");
    }
    Ok(())
}

fn foreground_process(fd: i32) -> (Vec<String>, Option<String>) {
    let pgrp = unsafe { libc::tcgetpgrp(fd) };
    if pgrp <= 0 {
        return (Vec::new(), None);
    }
    let argv = read_process_argv(pgrp);
    let cwd = fs::read_link(format!("/proc/{pgrp}/cwd"))
        .ok()
        .map(|cwd| cwd.to_string_lossy().into_owned());
    (argv, cwd)
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_BYTES {
        bail!("frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        bail!("invalid frame length {length}");
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).context("decode remote-agent frame")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::termios::{InputFlags, LocalFlags, OutputFlags};
    use std::fs::OpenOptions;

    /// A `PaneState` with no real PTY behind it. `shell_pid` stands in for the
    /// pane's login shell, the anchor every agent-pid resolution walks toward.
    fn test_pane(id: Uuid, shell_pid: libc::pid_t) -> PaneState {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        PaneState {
            id,
            pid: shell_pid,
            master: Mutex::new(file),
            history: Mutex::new(VecDeque::new()),
            history_bytes: Mutex::new(0),
            next_sequence: Mutex::new(1),
            subscribers: Mutex::new(Vec::new()),
            agent: Mutex::new(None),
            exit_status: Mutex::new(None),
        }
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    struct BrokenWriter;

    impl Write for BrokenWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn frames_round_trip_and_reject_malformed_lengths() {
        let message = ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: "26.0.0".into(),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &message).unwrap();
        assert_eq!(
            read_frame::<_, ClientMessage>(&mut bytes.as_slice()).unwrap(),
            message
        );
        assert!(read_frame::<_, ClientMessage>(&mut [0, 0, 0, 0].as_slice()).is_err());
        assert!(read_frame::<_, ClientMessage>(&mut u32::MAX.to_be_bytes().as_slice()).is_err());

        let state_message = ServerMessage::AgentStateChanged {
            event: RemoteAgentStateEvent {
                pane_id: Uuid::new_v4(),
                state: RemoteAgentRunState::Working,
                agent: "opencode".into(),
                heartbeat: false,
            },
        };
        let mut state_bytes = Vec::new();
        write_frame(&mut state_bytes, &state_message).unwrap();
        assert_eq!(
            read_frame::<_, ServerMessage>(&mut state_bytes.as_slice()).unwrap(),
            state_message
        );
    }

    #[test]
    fn force_restart_only_accepts_a_remote_agent_handshake() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let daemon = thread::spawn(move || {
            let hello: ClientMessage = read_frame(&mut server).unwrap();
            assert!(matches!(
                hello,
                ClientMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    ..
                }
            ));
            write_frame(
                &mut server,
                &ServerMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    agent_version: "26.10.0".into(),
                },
            )
            .unwrap();
        });
        validate_force_restart_target(&mut client).unwrap();
        daemon.join().unwrap();

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let impostor = thread::spawn(move || {
            let _: ClientMessage = read_frame(&mut server).unwrap();
            write_frame(
                &mut server,
                &ServerMessage::PaneClosed {
                    pane_id: Uuid::new_v4(),
                },
            )
            .unwrap();
        });
        assert!(validate_force_restart_target(&mut client).is_err());
        impostor.join().unwrap();
        assert!(terminate_daemon(1).is_err());
    }

    #[test]
    fn an_existing_remote_pty_is_resized_before_output_replay() {
        let pane_id = Uuid::new_v4();
        assert_eq!(
            remote_attach_requests(pane_id, 42, 120, 40),
            [
                ClientMessage::Resize {
                    pane_id,
                    cols: 120,
                    rows: 40,
                },
                ClientMessage::AttachPane {
                    pane_id,
                    after_sequence: 42,
                },
            ]
        );
    }

    #[test]
    fn fresh_remote_panes_create_before_they_ever_reconnect() {
        assert_eq!(initial_connection_mode(false), (false, false));
        assert_eq!(retry_connection_state(false), "connecting");

        assert_eq!(initial_connection_mode(true), (true, true));
        assert_eq!(retry_connection_state(true), "reconnecting");
    }

    #[test]
    fn provider_diagnostics_are_bounded_to_the_last_nonempty_line() {
        let captured = capture_diagnostics(
            b"warning that should not enter the pty\nfinal transport error\n".as_slice(),
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while diagnostic_tail(&captured).as_deref() != Some("final transport error")
            && Instant::now() < deadline
        {
            thread::yield_now();
        }
        assert_eq!(
            diagnostic_tail(&captured).as_deref(),
            Some("final transport error")
        );
    }

    #[test]
    fn bounded_history_drops_oldest_complete_chunks() {
        let pane = test_pane(Uuid::new_v4(), 1);
        pane.record_output(vec![1; HISTORY_LIMIT_BYTES]);
        pane.record_output(vec![2; 16]);
        let history = pane.history.lock().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].sequence, 2);
    }

    #[test]
    fn replay_is_delivered_once_before_live_output() {
        let pane = test_pane(Uuid::new_v4(), 1);
        pane.record_output(b"replay".to_vec());
        let (tx, rx) = mpsc::channel();
        pane.subscribe_with_replay(0, &tx).unwrap();
        pane.record_output(b"live".to_vec());

        let messages: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            &messages[0],
            ServerMessage::Output { sequence: 1, data, .. } if data == b"replay"
        ));
        assert!(matches!(
            messages[1],
            ServerMessage::Attached {
                next_sequence: 2,
                ..
            }
        ));
        assert!(matches!(
            &messages[2],
            ServerMessage::Output { sequence: 2, data, .. } if data == b"live"
        ));
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn truncated_replay_skips_the_incomplete_terminal_stream() {
        let pane = test_pane(Uuid::new_v4(), 1);
        *pane.history.lock().unwrap() = VecDeque::from([OutputChunk {
            sequence: 7,
            data: b"middle of a vt redraw".to_vec(),
        }]);
        *pane.history_bytes.lock().unwrap() = 21;
        *pane.next_sequence.lock().unwrap() = 8;
        let (tx, rx) = mpsc::channel();

        pane.subscribe_with_replay(0, &tx).unwrap();

        let messages: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            messages[0],
            ServerMessage::ReplayTruncated {
                first_available: 7,
                ..
            }
        ));
        assert!(matches!(
            messages[1],
            ServerMessage::Attached {
                next_sequence: 8,
                ..
            }
        ));
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn a_released_agent_is_not_replayed_after_attach() {
        let pane_id = Uuid::new_v4();
        let pane = test_pane(pane_id, 1);
        pane.record_agent_state(RemoteAgentRunState::Working, "opencode".into(), None, None);
        pane.record_agent_state(RemoteAgentRunState::Release, "opencode".into(), None, None);

        let (tx, rx) = mpsc::channel();
        pane.subscribe_with_replay(0, &tx).unwrap();
        let messages: Vec<_> = rx.try_iter().collect();
        assert!(matches!(messages[0], ServerMessage::Attached { .. }));
        // A release clears the tracked agent outright, so there is nothing left
        // to replay — the old design kept `Release` as a sticky state and
        // re-sent it on every attach.
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn a_live_agent_is_replayed_after_attach_as_a_heartbeat() {
        let pane_id = Uuid::new_v4();
        let pane = test_pane(pane_id, 1);
        // No resolvable reporter, so no pid is watched: presence then rests on
        // explicit reports alone and must never be released by inference.
        pane.record_agent_state(RemoteAgentRunState::Blocked, "opencode".into(), None, None);

        let (tx, rx) = mpsc::channel();
        pane.subscribe_with_replay(0, &tx).unwrap();
        let messages: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            &messages[1],
            ServerMessage::AgentStateChanged {
                event: RemoteAgentStateEvent {
                    state: RemoteAgentRunState::Blocked,
                    agent,
                    heartbeat: true,
                    ..
                }
            } if agent == "opencode"
        ));
    }

    #[test]
    fn presence_survives_an_agent_that_is_not_in_the_foreground() {
        // The failure the previous design could not express: the agent is alive
        // but owns neither the foreground process group nor an ancestry path to
        // it. Presence now watches the agent's own pid, so nothing about the
        // foreground can release it.
        let pane = test_pane(Uuid::new_v4(), 1);
        let live_pid = std::process::id() as libc::pid_t;
        *pane.agent.lock().unwrap() = Some(TrackedAgent {
            label: "opencode".into(),
            state: RemoteAgentRunState::Working,
            pid: Some(live_pid),
        });
        let (tx, rx) = mpsc::channel();
        pane.subscribers.lock().unwrap().push(tx);

        pane.settle_presence(true);

        assert!(matches!(
            rx.try_iter().next(),
            Some(ServerMessage::AgentStateChanged {
                event: RemoteAgentStateEvent {
                    state: RemoteAgentRunState::Working,
                    heartbeat: true,
                    ..
                }
            })
        ));
        assert!(pane.agent.lock().unwrap().is_some());
    }

    #[test]
    fn a_dead_agent_process_releases_the_agent_exactly_once() {
        let pane = test_pane(Uuid::new_v4(), 1);
        // Reap a real child so the pid is genuinely gone rather than guessed at.
        let mut child = Command::new("true").spawn().unwrap();
        let dead_pid = child.id() as libc::pid_t;
        child.wait().unwrap();
        *pane.agent.lock().unwrap() = Some(TrackedAgent {
            label: "opencode".into(),
            state: RemoteAgentRunState::Working,
            pid: Some(dead_pid),
        });
        let (tx, rx) = mpsc::channel();
        pane.subscribers.lock().unwrap().push(tx);

        pane.settle_presence(true);
        assert!(matches!(
            rx.try_iter().next(),
            Some(ServerMessage::AgentStateChanged {
                event: RemoteAgentStateEvent {
                    state: RemoteAgentRunState::Release,
                    heartbeat: false,
                    ..
                }
            })
        ));
        assert!(pane.agent.lock().unwrap().is_none());

        // Nothing is tracked any more, so further ticks are silent — no
        // repeated release, and nothing for a client to flicker on.
        pane.settle_presence(true);
        assert!(rx.try_iter().next().is_none());
    }

    #[test]
    fn file_identity_distinguishes_files_and_tolerates_missing_paths() {
        let scratch = std::env::temp_dir().join(format!("flock-identity-{}", std::process::id()));
        fs::write(&scratch, b"x").unwrap();
        assert_eq!(file_identity(&scratch), file_identity(&scratch));
        assert!(file_identity(&scratch).is_some());
        assert_ne!(
            file_identity(&scratch),
            file_identity(Path::new("/dev/null"))
        );
        assert_eq!(file_identity(Path::new("/nonexistent/flock")), None);
        fs::remove_file(&scratch).unwrap();
    }

    #[test]
    fn agent_label_matches_common_argv_shapes() {
        assert!(argv_looks_like_agent("opencode", &argv(&["opencode"])));
        assert!(argv_looks_like_agent(
            "opencode",
            &argv(&["/home/coder/.opencode/bin/opencode", "--continue"])
        ));
        assert!(argv_looks_like_agent(
            "opencode",
            &argv(&["node", "/usr/lib/opencode/index.js"])
        ));
        assert!(argv_looks_like_agent(
            "claude",
            &argv(&["/nix/store/abc123-claude-code/bin/.claude-unwrapped"])
        ));
        assert!(!argv_looks_like_agent("opencode", &argv(&["-fish"])));
        assert!(!argv_looks_like_agent(
            "opencode",
            &argv(&["vim", "notes.md"])
        ));
    }

    #[test]
    fn agent_pid_is_resolved_from_the_reporter_ancestry() {
        // The shape a hook actually produces: the pane's shell launched the
        // agent, and the agent spawned the short-lived reporter.
        //   100 shell -> 200 opencode -> 300 report-state
        let processes = HashMap::from([
            (300, (200, argv(&["flock", "remote-agent", "report-state"]))),
            (
                200,
                (100, argv(&["node", "/home/coder/.opencode/bin/opencode"])),
            ),
        ]);
        let info = |pid| processes.get(&pid).cloned();

        // Named explicitly by an in-process integration: taken as-is once
        // confirmed to sit in the reporter's chain.
        assert_eq!(
            resolve_agent_pid("opencode", Some(300), Some(200), 100, info),
            Some(200)
        );
        // Not named: the label locates it in the chain.
        assert_eq!(
            resolve_agent_pid("opencode", Some(300), None, 100, info),
            Some(200)
        );
        // An explicit pid outside the reporter's chain is not trusted; the
        // label still finds the right process.
        assert_eq!(
            resolve_agent_pid("opencode", Some(300), Some(999), 100, info),
            Some(200)
        );
        // An unrecognized label falls back to whatever the shell launched.
        assert_eq!(
            resolve_agent_pid("mystery", Some(300), None, 100, info),
            Some(200)
        );
    }

    #[test]
    fn agent_pid_resolution_skips_the_hooks_own_wrapper() {
        // Claude's hook is installed under a path named after the agent and is
        // invoked through a shell, so several links in the chain match the
        // label. Only the one that *is* claude outlives the report; picking the
        // wrapper would release the agent as soon as the hook returned.
        //   100 shell -> 150 devenv -> 200 claude -> 280 sh(hook) -> 300 python
        let processes = HashMap::from([
            (300, (280, argv(&["python3", "-"]))),
            (
                280,
                (
                    200,
                    argv(&[
                        "sh",
                        "/home/coder/.claude/hooks/flock-agent-state.sh",
                        "working",
                    ]),
                ),
            ),
            (
                200,
                (
                    150,
                    argv(&["/nix/store/abc-claude-code/bin/.claude-unwrapped"]),
                ),
            ),
            (150, (100, argv(&["devenv", "shell"]))),
        ]);
        let info = |pid| processes.get(&pid).cloned();

        assert_eq!(
            resolve_agent_pid("claude", Some(300), None, 100, info),
            Some(200)
        );
    }

    #[test]
    fn agent_pid_resolution_declines_reporters_outside_the_pane() {
        let processes = HashMap::from([
            (300, (200, argv(&["report-state"]))),
            (200, (7, argv(&["opencode"]))),
        ]);
        let info = |pid| processes.get(&pid).cloned();

        // The chain terminates at pid 7, not the pane's shell (100): this
        // reporter belongs to some other pane, so watching anything it names
        // would attach presence to an unrelated process.
        assert_eq!(
            resolve_agent_pid("opencode", Some(300), None, 100, info),
            None
        );
        // An unknown reporter is equally undecidable.
        assert_eq!(
            resolve_agent_pid("opencode", Some(400), None, 100, info),
            None
        );
        assert_eq!(
            resolve_agent_pid("opencode", None, Some(200), 100, info),
            None
        );
    }

    #[test]
    fn a_resolved_agent_pid_survives_later_state_reports() {
        // Only the first report of an agent pays for resolution. A later
        // transition reported without a pid — or from a helper the daemon can no
        // longer place — must not downgrade a good answer to `None`.
        let pane = test_pane(Uuid::new_v4(), 1);
        *pane.agent.lock().unwrap() = Some(TrackedAgent {
            label: "opencode".into(),
            state: RemoteAgentRunState::Working,
            pid: Some(4242),
        });

        pane.record_agent_state(RemoteAgentRunState::Idle, "opencode".into(), None, None);

        let tracked = pane.agent.lock().unwrap().clone().unwrap();
        assert_eq!(tracked.pid, Some(4242));
        assert_eq!(tracked.state, RemoteAgentRunState::Idle);
    }

    #[test]
    fn a_release_is_published_even_on_a_non_heartbeat_tick() {
        // Liveness is checked far more often than presence is re-asserted, so
        // the release path must not be gated behind the heartbeat cadence —
        // otherwise a finished agent would linger for several ticks.
        let pane = test_pane(Uuid::new_v4(), 1);
        let mut child = Command::new("true").spawn().unwrap();
        let dead_pid = child.id() as libc::pid_t;
        child.wait().unwrap();
        *pane.agent.lock().unwrap() = Some(TrackedAgent {
            label: "opencode".into(),
            state: RemoteAgentRunState::Working,
            pid: Some(dead_pid),
        });
        let (tx, rx) = mpsc::channel();
        pane.subscribers.lock().unwrap().push(tx);

        pane.settle_presence(false);

        assert!(matches!(
            rx.try_iter().next(),
            Some(ServerMessage::AgentStateChanged {
                event: RemoteAgentStateEvent {
                    state: RemoteAgentRunState::Release,
                    ..
                }
            })
        ));
    }

    #[test]
    fn a_live_agent_is_silent_on_a_non_heartbeat_tick() {
        let pane = test_pane(Uuid::new_v4(), 1);
        *pane.agent.lock().unwrap() = Some(TrackedAgent {
            label: "opencode".into(),
            state: RemoteAgentRunState::Working,
            pid: Some(std::process::id() as libc::pid_t),
        });
        let (tx, rx) = mpsc::channel();
        pane.subscribers.lock().unwrap().push(tx);

        pane.settle_presence(false);

        assert!(rx.try_iter().next().is_none());
        assert!(pane.agent.lock().unwrap().is_some());
    }

    #[test]
    fn a_pane_with_no_agent_ticks_silently() {
        let pane = test_pane(Uuid::new_v4(), 1);
        let (tx, rx) = mpsc::channel();
        pane.subscribers.lock().unwrap().push(tx);
        pane.settle_presence(true);
        assert!(rx.try_iter().next().is_none());
    }

    #[test]
    fn daemon_acknowledges_and_broadcasts_agent_state_reports() {
        let pane_id = Uuid::new_v4();
        let pane = Arc::new(test_pane(pane_id, 1));
        let panes = Arc::new(Mutex::new(HashMap::from_iter([(pane_id, pane.clone())])));
        let (mut client, server) = UnixStream::pair().unwrap();
        let daemon = thread::spawn(move || handle_client(server, panes));
        let mut reader = client.try_clone().unwrap();
        write_frame(
            &mut client,
            &ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                client_version: "test".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            read_frame::<_, ServerMessage>(&mut reader).unwrap(),
            ServerMessage::Hello {
                protocol: PROTOCOL_VERSION,
                ..
            }
        ));
        let (subscriber_tx, subscriber_rx) = mpsc::channel();
        pane.subscribers.lock().unwrap().push(subscriber_tx);
        write_frame(
            &mut client,
            &ClientMessage::ReportAgentState {
                pane_id,
                state: RemoteAgentRunState::Blocked,
                agent: "claude".into(),
                agent_pid: None,
            },
        )
        .unwrap();
        assert_eq!(
            read_frame::<_, ServerMessage>(&mut reader).unwrap(),
            ServerMessage::AgentStateAccepted { pane_id }
        );
        assert!(matches!(
            subscriber_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::AgentStateChanged {
                event: RemoteAgentStateEvent {
                    state: RemoteAgentRunState::Blocked,
                    ref agent,
                    ..
                }
            } if agent == "claude"
        ));
        client.shutdown(Shutdown::Both).unwrap();
        daemon.join().unwrap().unwrap();
    }

    #[test]
    fn agent_state_values_and_local_pipe_args_are_stable() {
        assert_eq!(
            RemoteAgentRunState::parse("BLOCKED").unwrap(),
            RemoteAgentRunState::Blocked
        );
        assert!(RemoteAgentRunState::parse("unknown").is_err());
        assert!(validate_agent_label("").is_err());
        assert!(validate_agent_label("bad,label").is_err());
        let event = RemoteAgentStateEvent {
            pane_id: Uuid::new_v4(),
            state: RemoteAgentRunState::Idle,
            agent: "opencode".into(),
            heartbeat: false,
        };
        assert_eq!(
            local_agent_state_args("7", &event),
            "pane_id=7,state=idle,agent=opencode,source=flock:coder-remote,presence=report"
        );
        assert_eq!(
            local_agent_state_args(
                "7",
                &RemoteAgentStateEvent {
                    heartbeat: true,
                    ..event
                }
            ),
            "pane_id=7,state=idle,agent=opencode,source=flock:coder-remote,presence=heartbeat"
        );
    }

    #[test]
    fn health_records_survive_a_json_round_trip() {
        // The bridge writes this file and the server parses it; a serde change
        // that broke the pair would silently degrade every remote to healthy.
        let health = RemotePaneHealth {
            status: RemoteProtocolStatus::VersionSkew,
            daemon_version: Some("26.5.0".into()),
            local_version: Some("26.7.0".into()),
            retry_count: 3,
            last_error: Some("connection lost".into()),
        };
        let encoded = serde_json::to_string(&health).unwrap();
        assert_eq!(
            serde_json::from_str::<RemotePaneHealth>(&encoded).unwrap(),
            health
        );
        // An absent or unreadable file must read as healthy, never as a fault
        // the bridge never reported.
        assert_eq!(RemotePaneHealth::default().status, RemoteProtocolStatus::Ok);
    }

    #[test]
    fn a_protocol_rejection_is_recognised_from_the_daemon_message() {
        // `run_remote_transport` keys the terminal ProtocolIncompatible status
        // off this wording, which `ensure_protocol_version` produces.
        let error = ensure_protocol_version(PROTOCOL_VERSION + 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("incompatible protocol version"));
    }

    #[test]
    fn bundled_integrations_use_one_asset_with_runtime_transport_selection() {
        let opencode =
            include_str!("../default-plugins/flock-sidebar/assets/opencode/flock-agent-state.js");
        let codex =
            include_str!("../default-plugins/flock-sidebar/assets/codex/flock-agent-state.sh");
        let claude =
            include_str!("../default-plugins/flock-sidebar/assets/claude/flock-agent-state.sh");
        for integration in [opencode, codex, claude] {
            assert!(integration.contains("FLOCK_STATE_CHANNEL"));
            assert!(integration.contains("remote-agent"));
            assert!(integration.contains("report-state"));
            assert!(integration.contains("flock-state"));
        }
        assert!(opencode.contains("useFileChannel"));
    }

    #[test]
    fn input_router_retries_a_chunk_on_the_replacement_transport() {
        let pane_id = Uuid::new_v4();
        let router = Arc::new(InputRouter::new(pane_id));
        let stale: TransportWriter = Arc::new(Mutex::new(Box::new(BrokenWriter)));
        let _stale = router.activate(stale);

        let forwarding_router = router.clone();
        let forwarding = thread::spawn(move || forwarding_router.forward(b"kept".to_vec()));
        let deadline = Instant::now() + Duration::from_secs(3);
        while router.writer.lock().unwrap().is_some() && Instant::now() < deadline {
            // Avoid continuously reacquiring the router mutex and starving the
            // forwarding thread on heavily loaded CI runners.
            thread::sleep(Duration::from_millis(10));
        }
        assert!(router.writer.lock().unwrap().is_none());

        let mut replacement = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let replacement_stdin: TransportWriter =
            Arc::new(Mutex::new(Box::new(replacement.stdin.take().unwrap())));
        let active = router.activate(replacement_stdin.clone());
        forwarding.join().unwrap();
        drop(active);
        drop(replacement_stdin);
        let output = replacement.wait_with_output().unwrap().stdout;
        let message = read_frame::<_, ClientMessage>(&mut output.as_slice()).unwrap();
        assert_eq!(
            message,
            ClientMessage::Input {
                pane_id,
                data: b"kept".to_vec(),
            }
        );
    }

    #[test]
    fn incompatible_protocol_is_rejected() {
        let error = ensure_protocol_version(PROTOCOL_VERSION + 1).unwrap_err();
        assert!(error.to_string().contains("incompatible protocol version"));
        assert!(ensure_protocol_version(PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn pty_survives_detached_output_and_panes_are_isolated() {
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let first = spawn_pane(first_id, 80, 24, HashMap::new(), None).unwrap();
        let second = spawn_pane(second_id, 100, 30, HashMap::new(), None).unwrap();
        let first_pid = first.pid;
        start_pane_threads(first.clone());
        start_pane_threads(second.clone());
        first
            .master
            .lock()
            .unwrap()
            .write_all(b"printf flock-first\\n")
            .unwrap();
        second
            .master
            .lock()
            .unwrap()
            .write_all(b"printf flock-second\\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let first_output = first
                .history
                .lock()
                .unwrap()
                .iter()
                .flat_map(|c| c.data.clone())
                .collect::<Vec<_>>();
            let second_output = second
                .history
                .lock()
                .unwrap()
                .iter()
                .flat_map(|c| c.data.clone())
                .collect::<Vec<_>>();
            if first_output.windows(11).any(|w| w == b"flock-first")
                && second_output.windows(12).any(|w| w == b"flock-second")
            {
                assert_eq!(first.pid, first_pid);
                assert!(!first_output.windows(12).any(|w| w == b"flock-second"));
                set_winsize(first.master.lock().unwrap().as_raw_fd(), 120, 40).unwrap();
                unsafe {
                    libc::kill(-first.pid, libc::SIGHUP);
                    libc::kill(-second.pid, libc::SIGHUP);
                }
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        unsafe {
            libc::kill(-first.pid, libc::SIGKILL);
            libc::kill(-second.pid, libc::SIGKILL);
        }
        panic!("PTY shells did not produce isolated output before timeout");
    }

    #[test]
    fn pty_shell_starts_in_requested_remote_cwd() {
        let cwd = std::env::temp_dir().join(format!("flock-remote-cwd-{}", Uuid::new_v4()));
        fs::create_dir(&cwd).unwrap();
        let pane = spawn_pane_with_shell(
            Uuid::new_v4(),
            80,
            24,
            HashMap::new(),
            Some(&cwd),
            Some(Path::new("/bin/sh")),
        )
        .unwrap();
        start_pane_threads(pane.clone());
        pane.master.lock().unwrap().write_all(b"pwd\n").unwrap();
        let expected = cwd.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let output = pane
                .history
                .lock()
                .unwrap()
                .iter()
                .flat_map(|chunk| chunk.data.clone())
                .collect::<Vec<_>>();
            if String::from_utf8_lossy(&output).contains(&expected) {
                unsafe { libc::kill(-pane.pid, libc::SIGHUP) };
                let _ = fs::remove_dir(&cwd);
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        unsafe { libc::kill(-pane.pid, libc::SIGKILL) };
        let _ = fs::remove_dir(&cwd);
        panic!("PTY shell did not start in {}", cwd.display());
    }

    #[test]
    fn environment_forwarding_is_minimal() {
        assert!(allowed_env("TERM"));
        assert!(allowed_env("LANG"));
        assert!(!allowed_env("ZELLIJ_CONFIG_DIR"));
        assert!(!allowed_env("HOME"));
        assert!(!allowed_env("FLOCK_PANE_ID"));
        assert!(!allowed_env("FLOCK_EXECUTABLE"));
        assert!(!allowed_env("FLOCK_STATE_CHANNEL"));

        let pane_id = Uuid::new_v4();
        let controlled = controlled_remote_pane_env(pane_id).unwrap();
        assert_eq!(controlled.get("FLOCK_PANE_ID"), Some(&pane_id.to_string()));
        assert_eq!(
            controlled.get("FLOCK_STATE_CHANNEL").map(String::as_str),
            Some("remote-agent")
        );
        assert!(controlled
            .get("FLOCK_EXECUTABLE")
            .is_some_and(|executable| !executable.is_empty()));
    }

    #[test]
    fn coder_bridge_uses_raw_terminal_mode_and_restores_it() {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let original = termios::tcgetattr(slave_fd).unwrap();
        let guard = RawTerminalGuard::enter(slave_fd).unwrap().unwrap();
        let raw = termios::tcgetattr(slave_fd).unwrap();
        assert!(!raw.local_flags.intersects(
            LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::IEXTEN
        ));
        assert!(!raw
            .input_flags
            .intersects(InputFlags::ICRNL | InputFlags::IXON));
        assert!(!raw.output_flags.contains(OutputFlags::OPOST));
        drop(guard);
        let restored = termios::tcgetattr(slave_fd).unwrap();
        assert_eq!(restored.input_flags, original.input_flags);
        assert_eq!(restored.output_flags, original.output_flags);
        assert_eq!(restored.control_flags, original.control_flags);
        // macOS may add PENDIN while applying otherwise identical settings.
        assert_eq!(
            restored.local_flags - LocalFlags::PENDIN,
            original.local_flags - LocalFlags::PENDIN
        );
        assert_eq!(restored.control_chars, original.control_chars);
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
    }
}
