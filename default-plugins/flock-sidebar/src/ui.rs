//! Sidebar rendering, ported from herdr's `ui/sidebar.rs` + `ui/status.rs` and
//! re-targeted from `ratatui` onto raw-ANSI output.
//!
//! The sidebar has two sections, matching herdr's split:
//!
//! - **sessions** — one row per zellij session with a single status dot that
//!   rolls its agents up to the most attention-worthy one, by herdr's priority
//!   (Blocked > Done-unseen > Working > Idle > none): a session waiting on the
//!   user shows the same red ◉ as a blocked agent, a background completion shows
//!   teal, a working agent green. The *current* session's rollup is computed
//!   from live per-pane state; other sessions' rollups come from the state they
//!   publish to the cross-session bus (Phase 7), carried on
//!   `SessionInfo.agent_states`, so a blocked or working agent in another
//!   workspace surfaces here in full fidelity. Sessions with no published state
//!   fall back to a coarse "agents present" marker derived from their pane
//!   commands.
//! - **agents** — one row per agent pane *in the current session*: a state icon
//!   and a label. The icon alone carries the state (color + glyph), so there is
//!   no status word.
//!
//! The two sections are stacked vertically: the workspaces overview fills the
//! top half and the agents section is pinned at the vertical midpoint, so the
//! split stays put as sessions come and go. Each half scrolls independently.
//!
//! Navigation is keyboard-first: a single selection cursor moves over the
//! sessions then the agents (Up/Down or k/j), and Enter activates the selected
//! row (switch session / focus pane). Mouse click and scroll mirror the same
//! actions but are not required.
//!
//! Colors come from the user's active zellij theme (see [`Theme`](crate::palette)),
//! rendered as raw ANSI so backgrounds, the scrollbar, and the spinner stay
//! precise while still matching whatever theme is configured.

use std::collections::BTreeMap;

use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::{
    AgentRunState, DockMode, PaletteColor, PaneAgentStatus, PaneId, PaneManifest, RemoteBackend,
    RemoteConnectionState, RemoteProtocolStatus, SessionInfo, TabInfo,
};

use crate::detect::{identify_agent_from_command, AgentState};
use crate::palette::{bg, fg, goto, Theme, BOLD, DIM, NORMAL_INTENSITY, RESET};
use crate::state::PaneAgentState;

// Braille spinner frames — smooth rotation. Ported verbatim from herdr's
// `ui.rs`. The plugin advances `spinner_tick` once per animation timer fire
// (~8/sec) so it indexes the frames directly rather than herdr's /8 at 60fps.
const SPINNERS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Pane width (columns) below which the sidebar renders as a clean icon-only
/// rail instead of the full text layout.
///
/// This is the **only** thing that decides which view is drawn. The dock's mode
/// lives in the server, which resolves it to a width and hands us the columns; we
/// render whatever we were given. Consulting a mode here as well would be a second
/// source of truth that can disagree with the geometry.
///
/// A layout's `closed_size` therefore has to be below this for the rail to appear
/// when collapsed.
pub(crate) const THIN_WIDTH: usize = 16;

/// Blank rows kept above and below the sidebar content (both the thin/mini rail
/// and the full labeled view), so it gets a little breathing room from the
/// pane's top and bottom edges and the two views line up.
const RAIL_VPAD: usize = 1;

/// Blank columns kept to the right of the mini rail's divider, so the divider
/// doesn't sit flush against the content pane beside it. With the slim rail at 5
/// cols this leaves a centered dot, a gap, the divider, then this padding.
const RAIL_HPAD: usize = 1;

/// The user's requested sidebar presentation, shared by every rendered section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarMode {
    Open,
    Closed,
}

impl Default for SidebarMode {
    fn default() -> Self {
        Self::Open
    }
}

impl SidebarMode {
    pub fn toggled(self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::Open,
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::Open)
    }
}

impl From<DockMode> for SidebarMode {
    fn from(mode: DockMode) -> Self {
        match mode {
            DockMode::Open => Self::Open,
            DockMode::Closed => Self::Closed,
        }
    }
}

impl From<SidebarMode> for DockMode {
    fn from(mode: SidebarMode) -> Self {
        match mode {
            SidebarMode::Open => Self::Open,
            SidebarMode::Closed => Self::Closed,
        }
    }
}

/// Map the animation tick to a spinner frame.
pub fn spinner_frame(tick: u32) -> &'static str {
    SPINNERS[(tick as usize) % SPINNERS.len()]
}

/// Per-session activity for the sessions-overview dot. This rolls the session's
/// agents up to the single most attention-worthy one, following herdr's
/// `pane_attention_priority`: Blocked > Done-unseen > Working > Idle(stopped) >
/// none. Ordered by ascending priority so the highest discriminant wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SessionActivity {
    /// No agents in the session.
    None,
    /// One or more agents present, all idle and already seen — nothing to do.
    Stopped,
    /// At least one agent is actively working.
    Running,
    /// At least one agent finished in the background and hasn't been looked at
    /// yet (and none is blocked) — worth a glance.
    DoneUnseen,
    /// At least one agent is blocked waiting on the user — the most
    /// attention-worthy state, so it wins over everything else.
    Blocked,
}

/// Roll a set of `(state, seen)` agent signals into the single session dot
/// bucket by herdr's attention priority. Empty input ⇒ [`SessionActivity::None`].
fn rollup_activity(agents: impl Iterator<Item = (AgentState, bool)>) -> SessionActivity {
    let mut activity = SessionActivity::None;
    for (state, seen) in agents {
        let this = match state {
            AgentState::Blocked => SessionActivity::Blocked,
            AgentState::Working => SessionActivity::Running,
            AgentState::Idle if !seen => SessionActivity::DoneUnseen,
            // Idle-seen or Unknown: an agent is present but needs no attention.
            _ => SessionActivity::Stopped,
        };
        activity = activity.max(this);
    }
    activity
}

/// The current session's activity, from its live per-pane state.
fn current_session_activity(agents: &BTreeMap<PaneId, PaneAgentState>) -> SessionActivity {
    rollup_activity(
        agents
            .values()
            .filter(|st| st.is_agent())
            .map(|st| (st.state, st.seen)),
    )
}

/// A session's overview-dot activity: the live per-pane state for our own
/// session (fresher than what we publish), else the cross-session published
/// state, falling back to a coarse "agents present" marker from pane commands.
fn session_activity(
    session: &SessionInfo,
    agents: &BTreeMap<PaneId, PaneAgentState>,
) -> SessionActivity {
    if matches!(
        crate::session_remote_binding(session),
        Some(crate::RemoteBinding::Coder | crate::RemoteBinding::Ssh)
    ) && !session.agent_states.is_empty()
    {
        return session_activity_from_states(&session.agent_states);
    }
    if session.is_current_session {
        let activity = current_session_activity(agents);
        if activity == SessionActivity::None {
            session_activity_from_states(&session.agent_states)
        } else {
            activity
        }
    } else {
        let activity = session_activity_from_states(&session.agent_states);
        if activity == SessionActivity::None && session_agent_count(session) > 0 {
            SessionActivity::Stopped
        } else {
            activity
        }
    }
}

/// The dot glyph + color for a session's activity. Blocked is the red ◉ that
/// also marks a blocked agent in the detail list, so a session waiting on the
/// user stands out at a glance; done-unseen is teal, running green, idle yellow,
/// nothing a dim dot.
/// Remote-badge color from the session's daemon connection state: live blue,
/// in-flight yellow, lost dim.
fn connection_state_color(state: Option<RemoteConnectionState>, p: &Theme) -> PaletteColor {
    match state {
        Some(RemoteConnectionState::Connected) => p.blue,
        Some(RemoteConnectionState::Connecting | RemoteConnectionState::Reconnecting) => p.yellow,
        // Unknown is not the same claim as connected, and must not look like it.
        Some(RemoteConnectionState::Disconnected) | None => p.muted,
    }
}

fn activity_dot(activity: SessionActivity, p: &Theme) -> (&'static str, PaletteColor) {
    match activity {
        SessionActivity::Blocked => ("◉", p.red),
        SessionActivity::DoneUnseen => ("●", p.teal),
        SessionActivity::Running => ("●", p.green),
        SessionActivity::Stopped => ("●", p.yellow),
        SessionActivity::None => ("○", p.muted),
    }
}

/// The animated agent icon + its color, ported from herdr's `status::agent_icon`.
fn agent_icon(state: AgentState, seen: bool, tick: u32, p: &Theme) -> (&'static str, PaletteColor) {
    match (state, seen) {
        (AgentState::Blocked, _) => ("◉", p.red),
        (AgentState::Working, _) => (spinner_frame(tick), p.yellow),
        (AgentState::Idle, false) => ("●", p.teal),
        (AgentState::Idle, true) => ("✓", p.green),
        (AgentState::Unknown, _) => ("○", p.muted),
    }
}

/// Roll another session's published per-pane agent state (the Phase 7
/// cross-session bus, carried on `SessionInfo.agent_states`) into the session
/// dot, using the same attention priority as our own session — so a *blocked*
/// agent in another workspace shows its red ◉ here, not a generic "stopped" dot.
fn session_activity_from_states(states: &BTreeMap<PaneId, PaneAgentStatus>) -> SessionActivity {
    rollup_activity(
        states
            .values()
            .map(|status| (run_state_to_agent_state(status.state), status.seen)),
    )
}

/// Map the serializable cross-session [`AgentRunState`] back to the detector's
/// [`AgentState`] so both rollup paths share one priority function.
fn run_state_to_agent_state(state: AgentRunState) -> AgentState {
    match state {
        AgentRunState::Idle => AgentState::Idle,
        AgentRunState::Working => AgentState::Working,
        AgentRunState::Blocked => AgentState::Blocked,
        AgentRunState::Unknown => AgentState::Unknown,
    }
}

/// Count the panes in another session that look like agents, from their command
/// metadata alone. Used as a fallback for sessions whose flock-sidebar isn't
/// running (so they publish no `agent_states`): the running command is still
/// enough to know an agent is present, even without live state.
fn session_agent_count(session: &SessionInfo) -> usize {
    if !session.remote_panes.is_empty() {
        return session
            .remote_panes
            .values()
            .filter(|pane| identify_agent_from_command(&pane.foreground_argv).is_some())
            .count();
    }
    session
        .panes
        .panes
        .values()
        .flatten()
        .filter(|pane| !pane.is_plugin)
        .filter(|pane| {
            pane.terminal_command.as_deref().is_some_and(|cmd| {
                let argv: Vec<String> = cmd.split_whitespace().map(String::from).collect();
                identify_agent_from_command(&argv).is_some()
            })
        })
        .count()
}

/// A single agent row in the panel: a state icon and a label. No status word —
/// the icon's glyph and color carry the state.
pub struct AgentEntry {
    pub target: Target,
    /// Display label: the agent name, or `tab·agent` when the session has more
    /// than one tab (matching herdr's multi-tab `pane_details` labelling).
    pub label: String,
    pub state: AgentState,
    /// Whether the user has looked at this pane since it last finished in the
    /// background. A Done pane that hasn't been seen renders with the teal
    /// "done-unseen" icon until focused.
    pub seen: bool,
    /// Whether this is the focused pane in the focused tab.
    pub is_active: bool,
}

/// One entry in the unified sidebar list: a workspace (session) header, or an
/// agent that belongs to the session listed above it. The list interleaves each
/// session with its own agents so every agent is visible regardless of which
/// session is currently focused.
pub(crate) enum Row {
    Session {
        name: String,
        activity: SessionActivity,
        is_current: bool,
        /// The session's remote binding, if any (parsed from its
        /// `default_command`) — badged in the row: ☁ codespace/Coder or ⬢
        /// devcontainer.
        binding: Option<crate::RemoteBinding>,
        connection_state: Option<RemoteConnectionState>,
    },
    /// A remote problem belonging to the session listed directly above it. It
    /// is a list row rather than a banner so that navigation, clicking and
    /// scrolling all work without a second code path.
    RemoteIssue(RemoteIssue),
    Agent(AgentEntry),
}

/// What a navigable row points at — used for keyboard Enter and mouse clicks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Switch to (or focus) the session with this name.
    Session(String),
    /// Act on the remote problem belonging to this session.
    RemoteIssue(String),
    /// Focus this agent pane.
    Pane(PaneId),
}

/// Why a session's remote panes need attention, worst case first. Ordering is
/// the priority used when a session's panes disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RemoteIssueKind {
    /// A bridge-side warning that did not break the remote PTY itself.
    Diagnostic,
    /// A newly-created pane has not completed its first connection yet.
    Connecting,
    /// A transport that keeps dropping. Informational — retrying is already
    /// happening and there is nothing for the user to decide.
    Reconnecting,
    /// A daemon on a different build of flock. The panes still work.
    VersionSkew,
    /// Bootstrapping the remote binary failed.
    InstallFailed,
    /// A daemon speaking a protocol we cannot talk to. The panes are dead.
    ProtocolIncompatible,
}

impl RemoteIssueKind {
    /// Whether the user can do something about this. Connection retries resolve
    /// themselves or do not; offering a button for them would be a lie.
    pub fn is_actionable(&self) -> bool {
        !matches!(
            self,
            RemoteIssueKind::Connecting
                | RemoteIssueKind::Reconnecting
                | RemoteIssueKind::Diagnostic
        )
    }
}

/// One session's remote problem, rolled up from its panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIssue {
    pub session: String,
    pub kind: RemoteIssueKind,
    pub daemon_version: Option<String>,
    pub local_version: Option<String>,
    /// Remote panes belonging to this session — the count that would reconnect.
    pub pane_count: usize,
    /// Highest consecutive-failure count across the session's panes.
    pub retry_count: u32,
    /// Most recent bounded diagnostic from the pane that raised this issue.
    pub last_error: Option<String>,
}

/// How far an in-flight upgrade has got. Absent means the row is at rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeProgress {
    Working,
    Activated {
        version: String,
    },
    /// The binary is installed, but the old daemon still owns live PTYs. This
    /// is not an error, and it is not an activated upgrade either.
    InstalledPending {
        version: String,
        panes: Option<usize>,
    },
    Failed {
        reason: String,
    },
}

/// Roll a session's per-pane health into at most one issue. Returns `None` for
/// a session with no remote panes or nothing wrong with them — the common case,
/// which must add no row.
pub fn session_remote_issue(session: &SessionInfo) -> Option<RemoteIssue> {
    if session.remote_panes.is_empty() {
        return None;
    }
    let mut kind: Option<RemoteIssueKind> = None;
    let mut daemon_version = None;
    let mut local_version = None;
    let mut retry_count = 0;
    let mut last_error = None;
    for pane in session.remote_panes.values() {
        let health = &pane.health;
        let pane_kind = match health.status {
            RemoteProtocolStatus::ProtocolIncompatible => {
                Some(RemoteIssueKind::ProtocolIncompatible)
            },
            RemoteProtocolStatus::InstallFailed => Some(RemoteIssueKind::InstallFailed),
            RemoteProtocolStatus::VersionSkew => Some(RemoteIssueKind::VersionSkew),
            // A healthy pane still reports a problem while it is retrying.
            RemoteProtocolStatus::Ok if health.retry_count > 0 => Some(
                if session.remote_connection_state == RemoteConnectionState::Connecting {
                    RemoteIssueKind::Connecting
                } else {
                    RemoteIssueKind::Reconnecting
                },
            ),
            RemoteProtocolStatus::Ok if health.last_error.is_some() => {
                Some(RemoteIssueKind::Diagnostic)
            },
            RemoteProtocolStatus::Ok => None,
        };
        retry_count = retry_count.max(health.retry_count);
        // Keep the versions from whichever pane raised the worst issue, so the
        // row's numbers describe the problem it is naming.
        if let Some(pane_kind) = pane_kind {
            if kind.is_none_or(|current| pane_kind > current) {
                daemon_version = health.daemon_version.clone();
                local_version = health.local_version.clone();
                last_error = health.last_error.clone();
            }
            kind = Some(kind.map_or(pane_kind, |current| current.max(pane_kind)));
        }
    }
    Some(RemoteIssue {
        session: session.name.clone(),
        kind: kind?,
        daemon_version,
        local_version,
        pane_count: session.remote_panes.len(),
        retry_count,
        last_error,
    })
}

/// A rendered row's click target: which absolute pane row it occupies and which
/// selection index it corresponds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickTarget {
    pub row: usize,
    pub index: usize,
}

/// The glyph and text for a remote-issue row, given how far any upgrade of it
/// has got. Kept narrow on purpose: the sidebar is routinely 28 content columns
/// wide, and a row that truncates has failed at the one job it has.
///
/// The dock is not keyboard-focusable, so the armed state asks for the only
/// reachable confirmation: a second click.
pub fn remote_issue_text(
    issue: &RemoteIssue,
    armed: bool,
    progress: Option<&UpgradeProgress>,
    spinner: &'static str,
) -> (String, IssueTone) {
    if let Some(progress) = progress {
        return match progress {
            UpgradeProgress::Working => (
                format!("{spinner} installing {}…", version_or(&issue.local_version)),
                IssueTone::Busy,
            ),
            UpgradeProgress::Activated { version } => {
                (format!("✓ {version} activated"), IssueTone::Good)
            },
            UpgradeProgress::InstalledPending { version, panes } => (
                match panes {
                    Some(panes) => format!("✓ {version} · {panes} panes old"),
                    None => format!("✓ {version} · restart needed"),
                },
                IssueTone::Warn,
            ),
            UpgradeProgress::Failed { reason } => (format!("✗ {reason}  ⏎ retry"), IssueTone::Bad),
        };
    }
    if armed {
        return ("⇪ click again to install".to_owned(), IssueTone::Armed);
    }
    match issue.kind {
        RemoteIssueKind::VersionSkew => (
            format!(
                "⇪ v{} → {}",
                version_or(&issue.daemon_version),
                version_or(&issue.local_version)
            ),
            IssueTone::Warn,
        ),
        RemoteIssueKind::ProtocolIncompatible => {
            ("✗ reinstall needed  ⏎".to_owned(), IssueTone::Bad)
        },
        RemoteIssueKind::InstallFailed => ("✗ install failed  ⏎".to_owned(), IssueTone::Bad),
        RemoteIssueKind::Connecting => (
            format!("{spinner} connecting · try {}", issue.retry_count),
            IssueTone::Busy,
        ),
        RemoteIssueKind::Reconnecting => (
            format!("{spinner} reconnecting · try {}", issue.retry_count),
            IssueTone::Busy,
        ),
        RemoteIssueKind::Diagnostic => ("⚠ remote diagnostic".to_owned(), IssueTone::Warn),
    }
}

/// How a remote-issue row is coloured. Separate from the palette so the text
/// builder stays testable without a theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueTone {
    Warn,
    Bad,
    Busy,
    Good,
    Armed,
}

impl IssueTone {
    fn color(self, p: &Theme) -> PaletteColor {
        match self {
            IssueTone::Warn | IssueTone::Busy => p.yellow,
            IssueTone::Bad => p.red,
            IssueTone::Good => p.green,
            IssueTone::Armed => p.blue,
        }
    }
}

fn version_or(version: &Option<String>) -> String {
    version.clone().unwrap_or_else(|| "?".to_owned())
}

/// Build the agent list for the *current* session from its live panes, in tab
/// then pane order, one entry per pane that detection has tagged as an agent.
pub fn build_entries(
    panes: &PaneManifest,
    tabs: &[TabInfo],
    agents: &BTreeMap<PaneId, PaneAgentState>,
    published_agent_states: &BTreeMap<PaneId, PaneAgentStatus>,
) -> Vec<AgentEntry> {
    let multi_tab = tabs.len() > 1;
    let tab_active: BTreeMap<usize, bool> =
        tabs.iter().map(|tab| (tab.position, tab.active)).collect();
    let tab_name: BTreeMap<usize, String> = tabs
        .iter()
        .map(|tab| (tab.position, tab.name.clone()))
        .collect();

    let mut entries = Vec::new();
    // `panes.panes` is a BTreeMap keyed by tab position, so iteration is already
    // in tab order.
    for (tab_idx, panes_in_tab) in &panes.panes {
        for pane in panes_in_tab {
            let pane_id = if pane.is_plugin {
                PaneId::Plugin(pane.id)
            } else {
                PaneId::Terminal(pane.id)
            };
            let published_status = published_agent_states.get(&pane_id);
            let live_state = agents.get(&pane_id);
            let live_signal = live_state.filter(|st| st.is_agent()).map(|st| {
                (
                    st.effective_agent_label()
                        .unwrap_or_else(|| "?".to_string()),
                    st.state,
                    st.seen,
                )
            });
            let published_signal = published_status.map(|status| {
                (
                    if status.label.is_empty() {
                        "?".to_string()
                    } else {
                        status.label.clone()
                    },
                    run_state_to_agent_state(status.state),
                    status.seen,
                )
            });
            let Some((agent_label, state, seen)) = entry_signal(live_signal, published_signal)
            else {
                continue;
            };
            let label = if multi_tab {
                let tab = tab_name
                    .get(tab_idx)
                    .filter(|name| !name.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("tab {}", tab_idx + 1));
                format!("{tab}·{agent_label}")
            } else {
                agent_label
            };
            let is_active = pane.is_focused && tab_active.get(tab_idx).copied().unwrap_or(false);
            entries.push(AgentEntry {
                target: Target::Pane(pane_id),
                label,
                state,
                seen,
                is_active,
            });
        }
    }
    entries
}

fn entry_signal(
    live: Option<(String, AgentState, bool)>,
    published: Option<(String, AgentState, bool)>,
) -> Option<(String, AgentState, bool)> {
    match (live, published) {
        (
            Some((live_label, AgentState::Unknown, _)),
            Some((published_label, published_state, published_seen)),
        ) if published_state != AgentState::Unknown
            && labels_compatible(&live_label, &published_label) =>
        {
            let label = if live_label == "?" {
                published_label
            } else {
                live_label
            };
            Some((label, published_state, published_seen))
        },
        (Some(live), _) => Some(live),
        (None, published) => published,
    }
}

fn labels_compatible(live_label: &str, published_label: &str) -> bool {
    live_label == "?" || published_label.is_empty() || live_label == published_label
}

/// The unified, ordered sidebar list: every session (in [`ordered_sessions`]
/// order) as a dot-only overview row, followed by the *current* session's own
/// agent rows. The two runs map onto the two stacked render sections — sessions
/// up top, agents at the midpoint — while staying a single flat list so a
/// selection index maps consistently across the full view, the rail, keypresses
/// and clicks. [`render`], [`render_thin`] and [`navigable_targets`] all derive
/// from this.
pub(crate) fn build_rows(
    panes: &PaneManifest,
    tabs: &[TabInfo],
    agents: &BTreeMap<PaneId, PaneAgentState>,
    sessions: &[SessionInfo],
) -> Vec<Row> {
    let mut rows = Vec::new();
    // Top section: the workspaces overview — one dot-only row per session, no
    // per-agent rows. Cross-session agent detail is carried entirely by each
    // session's rollup dot.
    for session in ordered_sessions(sessions) {
        rows.push(Row::Session {
            name: session.name.clone(),
            activity: session_activity(session, agents),
            is_current: session.is_current_session,
            binding: crate::session_remote_binding(session),
            connection_state: matches!(
                &session.remote_backend,
                Some(RemoteBackend::Coder { .. } | RemoteBackend::Ssh { .. })
            )
            .then_some(session.remote_connection_state),
        });
        // A remote problem hangs directly off the session it belongs to, so
        // position identifies the host and the row's text does not have to
        // spend its scarce columns repeating the name.
        if let Some(issue) = session_remote_issue(session) {
            rows.push(Row::RemoteIssue(issue));
        }
    }
    // Bottom section: the current session's own agents, one row each. Only the
    // current session's panes are observable from here, so this is the live
    // detail view for the workspace you're in.
    if let Some(current) = sessions.iter().find(|s| s.is_current_session) {
        let entries = build_entries(panes, tabs, agents, &current.agent_states);
        rows.extend(entries.into_iter().map(Row::Agent));
    }
    rows
}

/// The activation target for a row: switch to a session, or focus an agent
/// pane. Agent rows only exist for the current session (see [`build_rows`]),
/// so their panes are always directly focusable.
fn row_target(row: &Row) -> Target {
    match row {
        Row::Session { name, .. } => Target::Session(name.clone()),
        Row::RemoteIssue(issue) => Target::RemoteIssue(issue.session.clone()),
        Row::Agent(entry) => entry.target.clone(),
    }
}

/// Sessions in a stable display order: one row per session (each session is its
/// own workspace). Ordered by `workspace_root` path so the layout is stable
/// frame to frame — sessions sharing a path keep their original order, and those
/// whose server reported no workspace root (empty path) sort last.
pub fn ordered_sessions(sessions: &[SessionInfo]) -> Vec<&SessionInfo> {
    let mut ordered: Vec<&SessionInfo> = sessions.iter().collect();
    // sort_by is stable, so equal keys preserve the original list order.
    ordered.sort_by(|a, b| {
        let ka = a.workspace_root.display().to_string();
        let kb = b.workspace_root.display().to_string();
        ka.is_empty().cmp(&kb.is_empty()).then(ka.cmp(&kb))
    });
    ordered
}

/// The ordered list of navigable targets, one per [`build_rows`] entry (each
/// session followed by its agents). The same ordering drives [`render`] and
/// [`render_thin`], so a selection index maps consistently whether it came from
/// a keypress or a click.
pub fn navigable_targets(
    panes: &PaneManifest,
    tabs: &[TabInfo],
    agents: &BTreeMap<PaneId, PaneAgentState>,
    sessions: &[SessionInfo],
) -> Vec<Target> {
    build_rows(panes, tabs, agents, sessions)
        .iter()
        .map(row_target)
        .collect()
}

/// Clamp a selection index to the navigable target count.
pub fn clamp_selection(selected: usize, total: usize) -> usize {
    selected.min(total.saturating_sub(1))
}

/// Length of the leading workspaces run in [`build_rows`] output: session rows
/// plus the issue rows hanging off them. Everything after it is an agent row.
pub(crate) fn workspace_section_len(rows: &[Row]) -> usize {
    rows.iter()
        .take_while(|row| matches!(row, Row::Session { .. } | Row::RemoteIssue(_)))
        .count()
}

/// One styled run of text within a rendered row.
struct Span {
    text: String,
    fg: PaletteColor,
    bold: bool,
    dim: bool,
}

impl Span {
    fn new(text: impl Into<String>, fg: PaletteColor) -> Self {
        Self {
            text: text.into(),
            fg,
            bold: false,
            dim: false,
        }
    }
    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    fn dim(mut self) -> Self {
        self.dim = true;
        self
    }
}

/// Emit one row of styled spans at `(x, y)`, padded to `width` with `row_bg`
/// (when set) and terminated with a full reset. A leading background is held
/// across spans (an intensity reset doesn't clear it) so a selected row's
/// highlight fills the whole width.
fn render_row(
    out: &mut String,
    x: usize,
    y: usize,
    width: usize,
    row_bg: Option<PaletteColor>,
    spans: &[Span],
) {
    out.push_str(&goto(x, y));
    if let Some(row_bg) = row_bg {
        out.push_str(&bg(row_bg));
    }
    let mut used = 0usize;
    for span in spans {
        out.push_str(NORMAL_INTENSITY);
        if span.bold {
            out.push_str(BOLD);
        }
        if span.dim {
            out.push_str(DIM);
        }
        if let Some(row_bg) = row_bg {
            out.push_str(&bg(row_bg));
        }
        out.push_str(&fg(span.fg));
        out.push_str(&span.text);
        used += span.text.width();
    }
    if used < width {
        out.push_str(NORMAL_INTENSITY);
        if let Some(row_bg) = row_bg {
            out.push_str(&bg(row_bg));
        }
        out.push_str(&" ".repeat(width - used));
    }
    out.push_str(RESET);
}

/// Truncate `text` to `max_width` display columns, with an ellipsis. Ported
/// from herdr's `sidebar::truncate_text`.
fn truncate_text(text: &str, max_width: usize) -> String {
    let len = text.width();
    if len <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = ch.to_string().width();
        if w + cw > max_width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// The full sidebar render input.
pub struct RenderInput<'a> {
    pub permissions_granted: bool,
    pub panes: &'a PaneManifest,
    pub tabs: &'a [TabInfo],
    pub agents: &'a BTreeMap<PaneId, PaneAgentState>,
    pub sessions: &'a [SessionInfo],
    pub palette: &'a Theme,
    /// Whether the sidebar pane is focused. The selection cursor is only drawn
    /// when focused, so an unfocused ambient rail shows status without a cursor.
    pub focused: bool,
    /// Unified selection cursor over sessions-then-agents.
    pub selected: usize,
    /// Scroll offset into the workspaces (sessions) section.
    pub scroll_sessions: usize,
    /// Scroll offset into the agents section.
    pub scroll_agents: usize,
    pub spinner_tick: u32,
    pub rows: usize,
    pub cols: usize,
    /// The session whose remote-issue row is waiting for a confirming second click.
    pub armed_issue: Option<&'a str>,
    /// In-flight or just-finished upgrades, keyed by session name.
    pub upgrade_progress: &'a BTreeMap<String, UpgradeProgress>,
}

/// The full sidebar render output.
pub struct RenderOutput {
    /// The raw-ANSI frame to print.
    pub ansi: String,
    /// Selection index after clamping to the target count.
    pub selected: usize,
    /// Workspaces-section scroll offset after clamping to keep the selection visible.
    pub scroll_sessions: usize,
    /// Agents-section scroll offset after clamping to keep the selection visible.
    pub scroll_agents: usize,
    /// Click targets for the rows drawn this frame.
    pub click_map: Vec<ClickTarget>,
}

/// Render the whole sidebar to a raw-ANSI string plus the click map.
pub fn render(input: RenderInput) -> RenderOutput {
    let p = input.palette;
    let cols = input.cols;
    let rows = input.rows;
    let mut out = String::new();
    let mut click_map = Vec::new();

    // Clear the pane explicitly by painting every row blank. A bare `\u{1b}[2J`
    // proved unreliable when the pane shrinks (e.g. collapsing from the full
    // labeled view to the thin rail): rows the new frame no longer draws kept
    // their stale content. Blanking the full height up front guarantees a clean
    // canvas regardless of how few rows the frame then draws over it.
    out.push_str("\u{1b}[2J");
    for y in 0..rows {
        render_row(&mut out, 0, y, cols, None, &[]);
    }

    if !input.permissions_granted {
        render_row(
            &mut out,
            0,
            0,
            cols,
            None,
            &[Span::new("waiting for permissions…", p.yellow)],
        );
        return RenderOutput {
            ansi: out,
            selected: 0,
            scroll_sessions: 0,
            scroll_agents: 0,
            click_map,
        };
    }

    let rows_data = build_rows(input.panes, input.tabs, input.agents, input.sessions);
    let selected = clamp_selection(input.selected, rows_data.len());

    // The columns the server gave us are the single source of truth: a collapsed
    // dock is narrow, so it lands here, and a genuinely narrow pane falls back to
    // the rail rather than trying to draw labels into too few columns.
    if cols < THIN_WIDTH {
        return render_thin(out, &input, &rows_data, selected);
    }

    let divider_x = cols.saturating_sub(1);
    let content_cols = divider_x;

    // Match the thin rail's breathing room: keep RAIL_VPAD blank rows above and
    // below the content, so the full view and the rail line up at the same top
    // offset and neither sits flush against the pane edges.
    let top = RAIL_VPAD.min(rows);
    let bottom_limit = rows.saturating_sub(RAIL_VPAD);
    // The agents header sits on the vertical midpoint so the split between the
    // two sections stays put as sessions come and go. On a pane too short to
    // hold both halves it collapses toward the bottom and the bodies empty out.
    let mid = if bottom_limit > top + 1 {
        (rows / 2).clamp(top + 1, bottom_limit)
    } else {
        bottom_limit
    };

    // The workspaces section is the leading run of session rows and the issue
    // rows attached to them; every agent row follows it.
    let session_count = workspace_section_len(&rows_data);
    let agent_count = rows_data.len() - session_count;

    // ---- top section: workspaces overview ----
    let sessions_body_start = top + 1;
    let sessions_body_height = mid.saturating_sub(sessions_body_start);
    let sessions_sel = (selected < session_count).then_some(selected);
    let scroll_sessions = keep_visible(
        input.scroll_sessions,
        sessions_sel,
        session_count,
        sessions_body_height,
    );
    if top < bottom_limit {
        render_row(
            &mut out,
            0,
            top,
            content_cols,
            None,
            &[Span::new(" workspaces", p.muted).bold()],
        );
    }
    render_section(
        &mut out,
        &mut click_map,
        SectionInput {
            rows: &rows_data[..session_count],
            index_offset: 0,
            body_start: sessions_body_start,
            body_height: sessions_body_height,
            scroll: scroll_sessions,
            selected,
            focused: input.focused,
            spinner_tick: input.spinner_tick,
            cols: content_cols,
            p,
            armed_issue: input.armed_issue,
            upgrade_progress: input.upgrade_progress,
        },
    );

    // ---- bottom section: current session's agents ----
    let agents_body_start = mid + 1;
    let agents_body_height = bottom_limit.saturating_sub(agents_body_start);
    let agents_sel = (selected >= session_count).then(|| selected - session_count);
    let scroll_agents = keep_visible(
        input.scroll_agents,
        agents_sel,
        agent_count,
        agents_body_height,
    );
    if mid < bottom_limit {
        render_row(
            &mut out,
            0,
            mid,
            content_cols,
            None,
            &[Span::new(" agents", p.muted).bold()],
        );
    }
    render_section(
        &mut out,
        &mut click_map,
        SectionInput {
            rows: &rows_data[session_count..],
            index_offset: session_count,
            body_start: agents_body_start,
            body_height: agents_body_height,
            scroll: scroll_agents,
            selected,
            focused: input.focused,
            spinner_tick: input.spinner_tick,
            cols: content_cols,
            p,
            armed_issue: input.armed_issue,
            upgrade_progress: input.upgrade_progress,
        },
    );
    render_divider(&mut out, divider_x, rows, p);
    RenderOutput {
        ansi: out,
        selected,
        scroll_sessions,
        scroll_agents,
        click_map,
    }
}

/// Clamp a section's scroll offset, then — if its selection cursor lives in this
/// section — nudge the offset so the selected row stays within the visible
/// window. `selected` is the row's index *within the section* (None when the
/// cursor is in the other section, so only the clamp applies).
fn keep_visible(scroll: usize, selected: Option<usize>, total: usize, visible: usize) -> usize {
    let mut scroll = scroll.min(total.saturating_sub(visible));
    if let Some(sel) = selected {
        if sel < scroll {
            scroll = sel;
        } else if visible > 0 && sel >= scroll + visible {
            scroll = sel + 1 - visible;
        }
    }
    scroll
}

/// Inputs to [`render_section`] — one stacked section's slice of rows and the
/// geometry it draws into.
struct SectionInput<'a> {
    /// This section's rows (a contiguous slice of `rows_data`).
    rows: &'a [Row],
    /// Flat selection index of `rows[0]`: 0 for the sessions section, the
    /// session count for the agents section. Added to each row's local index so
    /// the cursor and click map line up with [`navigable_targets`].
    index_offset: usize,
    body_start: usize,
    body_height: usize,
    scroll: usize,
    /// The global selection cursor (already clamped).
    selected: usize,
    focused: bool,
    spinner_tick: u32,
    cols: usize,
    p: &'a Theme,
    /// Session whose issue row is armed for a confirming second click.
    armed_issue: Option<&'a str>,
    upgrade_progress: &'a BTreeMap<String, UpgradeProgress>,
}

/// Render one stacked section's body: its rows from `scroll` down, an empty
/// `(none)` line when it has none, and a scrollbar when they overflow.
fn render_section(out: &mut String, click_map: &mut Vec<ClickTarget>, s: SectionInput) {
    let p = s.p;
    let total = s.rows.len();
    let body_bottom = s.body_start + s.body_height;
    let scrollbar = total > s.body_height && s.body_height > 0;
    let content_width = s.cols.saturating_sub(usize::from(scrollbar));

    if total == 0 {
        if s.body_start < body_bottom {
            render_row(
                out,
                0,
                s.body_start,
                content_width,
                None,
                &[Span::new(" (none)", p.muted).dim()],
            );
        }
        return;
    }

    let mut row_y = s.body_start;
    for (local, row) in s.rows.iter().enumerate().skip(s.scroll) {
        if row_y >= body_bottom {
            break;
        }
        let index = s.index_offset + local;
        // The cursor only shows while the sidebar is focused.
        let cursor = index == s.selected && s.focused;
        draw_row(
            out,
            row,
            row_y,
            content_width,
            cursor,
            s.spinner_tick,
            p,
            s.armed_issue,
            s.upgrade_progress,
        );
        click_map.push(ClickTarget { row: row_y, index });
        row_y += 1;
    }

    if scrollbar {
        render_scrollbar(
            out,
            s.cols.saturating_sub(1),
            s.body_start,
            s.body_height,
            total,
            s.body_height,
            s.scroll,
            p,
        );
    }
}

/// Draw one sidebar row — a session overview dot or an agent state icon — at
/// `row_y`. A current session and the focused agent pane stay emphasized even
/// without the cursor; the selected row also gets the selection background.
#[allow(clippy::too_many_arguments)]
fn draw_row(
    out: &mut String,
    row: &Row,
    row_y: usize,
    content_width: usize,
    cursor: bool,
    spinner_tick: u32,
    p: &Theme,
    armed_issue: Option<&str>,
    upgrade_progress: &BTreeMap<String, UpgradeProgress>,
) {
    let row_bg = cursor.then_some(p.selection_bg);
    match row {
        Row::Session {
            name,
            activity,
            is_current,
            binding,
            connection_state,
        } => {
            let (dot, dot_color) = activity_dot(*activity, p);
            let emphasized = cursor || *is_current;
            let name_color = if emphasized { p.text } else { p.muted };
            // Cloud badges get two cells of breathing room before the session
            // name; the devcontainer badge keeps the standard one-cell gap.
            let badge_width = match binding {
                Some(
                    crate::RemoteBinding::Codespace
                    | crate::RemoteBinding::Coder
                    | crate::RemoteBinding::Ssh,
                ) => 3,
                Some(crate::RemoteBinding::Devcontainer) => 2,
                None => 0,
            };
            let name_budget = content_width.saturating_sub(3 + badge_width);
            let label = truncate_text(name, name_budget);
            let mut name_span = Span::new(label, name_color);
            name_span = if emphasized {
                name_span.bold()
            } else {
                name_span.dim()
            };
            let mut spans = vec![
                Span::new(" ", p.text),
                Span::new(dot, dot_color),
                Span::new(" ", p.text),
            ];
            match binding {
                Some(crate::RemoteBinding::Codespace) => {
                    spans.push(Span::new("☁︎  ", p.blue));
                },
                Some(crate::RemoteBinding::Devcontainer) => {
                    spans.push(Span::new("⬢", p.teal));
                    spans.push(Span::new(" ", p.text));
                },
                Some(crate::RemoteBinding::Coder) => {
                    let color = connection_state_color(*connection_state, p);
                    spans.push(Span::new("☁︎  ", color));
                },
                Some(crate::RemoteBinding::Ssh) => {
                    let color = connection_state_color(*connection_state, p);
                    spans.push(Span::new("⇅  ", color));
                },
                None => {},
            }
            spans.push(name_span);
            render_row(out, 0, row_y, content_width, row_bg, &spans);
        },
        Row::RemoteIssue(issue) => {
            let armed = armed_issue == Some(issue.session.as_str());
            let (text, tone) = remote_issue_text(
                issue,
                armed,
                upgrade_progress.get(&issue.session),
                spinner_frame(spinner_tick),
            );
            // Indented one cell past the session dot so the row reads as
            // belonging to the session above it, the way agent rows do.
            let budget = content_width.saturating_sub(3);
            render_row(
                out,
                0,
                row_y,
                content_width,
                row_bg,
                &[
                    Span::new("   ", p.text),
                    Span::new(truncate_text(&text, budget), tone.color(p)).bold(),
                ],
            );
        },
        Row::Agent(entry) => {
            let (icon, icon_color) = agent_icon(entry.state, entry.seen, spinner_tick, p);
            let emphasized = cursor || entry.is_active;
            let name_color = if emphasized { p.text } else { p.muted };
            // Agent icons align under the session dots (their own section now,
            // not nested), so they share the session row's one-space indent.
            let label = truncate_text(&entry.label, content_width.saturating_sub(3));
            let mut name_span = Span::new(label, name_color);
            name_span = if emphasized {
                name_span.bold()
            } else {
                name_span.dim()
            };
            render_row(
                out,
                0,
                row_y,
                content_width,
                row_bg,
                &[
                    Span::new(" ", p.text),
                    Span::new(icon, icon_color),
                    Span::new(" ", p.text),
                    name_span,
                ],
            );
        },
    }
}

/// Render the compact icon rail used when the pane is too narrow for labels: a
/// centered vertical column of session activity dots. Agent detail stays in the
/// expanded view; mini mode is a workspace overview only.
fn render_thin(
    mut out: String,
    input: &RenderInput,
    rows_data: &[Row],
    selected: usize,
) -> RenderOutput {
    let p = input.palette;
    let cols = input.cols;
    let rows = input.rows;
    let mut click_map = Vec::new();

    // Lay the rail out as: glyph | divider | right padding. The divider sits
    // `RAIL_HPAD` columns in from the right edge so it gets a little breathing
    // room from the content pane rather than butting against it; the glyph lives
    // in the columns to its left.
    let (rail_width, divider_x) = divider_geometry(cols);

    // One glyph per session, carrying its flat selection index so a rail click
    // maps onto the same target list the expanded view uses. A session with a
    // remote problem shows the problem's glyph in place of its activity dot:
    // one cell can carry one message, so it carries the one needing a decision.
    let workspace_len = workspace_section_len(rows_data);
    let selected = clamp_selection(selected, rows_data.len());
    let mut glyphs: Vec<(usize, &'static str, PaletteColor)> = Vec::new();
    for (index, row) in rows_data.iter().enumerate().take(workspace_len) {
        match row {
            Row::Session { activity, .. } => {
                let (glyph, color) = activity_dot(*activity, p);
                glyphs.push((index, glyph, color));
            },
            Row::RemoteIssue(issue) => {
                if let Some(entry) = glyphs.last_mut() {
                    let (glyph, tone) = rail_issue_glyph(issue, input.spinner_tick);
                    entry.1 = glyph;
                    entry.2 = tone.color(p);
                }
            },
            Row::Agent(_) => {},
        }
    }

    // The rail cursor sits on a session. When the flat selection is on an issue
    // or agent row, highlight the session that owns it rather than nothing, and
    // report that session back as the selection so the rail's own Up/Down keeps
    // moving between workspaces instead of through rows it cannot draw.
    let rail_selected = glyphs
        .iter()
        .rposition(|(index, _, _)| *index <= selected)
        .unwrap_or(0);
    let selected = glyphs.get(rail_selected).map_or(0, |(index, _, _)| *index);

    // Keep a little breathing room above and below the glyphs so they don't sit
    // flush against the pane's top and bottom edges.
    let top = RAIL_VPAD.min(rows);
    let body_height = rows.saturating_sub(RAIL_VPAD * 2);

    // Scroll the rail so the selected session glyph stays within the padded body.
    let mut scroll = input
        .scroll_sessions
        .min(glyphs.len().saturating_sub(body_height.max(1)));
    if rail_selected < scroll {
        scroll = rail_selected;
    } else if body_height > 0 && rail_selected >= scroll + body_height {
        scroll = rail_selected + 1 - body_height;
    }

    // Center the single glyph within the rail (the columns left of the divider).
    let pad = rail_width.saturating_sub(1) / 2;
    for (position, &(index, glyph, color)) in
        glyphs.iter().enumerate().skip(scroll).take(body_height)
    {
        let y = top + (position - scroll);
        let cursor = position == rail_selected && input.focused;
        let row_bg = cursor.then_some(p.selection_bg);
        let mut glyph_span = Span::new(glyph, color);
        if cursor {
            glyph_span = glyph_span.bold();
        }
        let mut spans = Vec::new();
        if pad > 0 {
            spans.push(Span::new(" ".repeat(pad), p.text));
        }
        spans.push(glyph_span);
        render_row(&mut out, 0, y, rail_width, row_bg, &spans);
        click_map.push(ClickTarget { row: y, index });
    }

    render_divider(&mut out, divider_x, rows, p);

    RenderOutput {
        ansi: out,
        selected,
        scroll_sessions: scroll,
        scroll_agents: input.scroll_agents,
        click_map,
    }
}

/// The single glyph standing in for a session's remote problem on the rail.
fn rail_issue_glyph(issue: &RemoteIssue, spinner_tick: u32) -> (&'static str, IssueTone) {
    match issue.kind {
        RemoteIssueKind::VersionSkew => ("⇪", IssueTone::Warn),
        RemoteIssueKind::ProtocolIncompatible | RemoteIssueKind::InstallFailed => {
            ("✗", IssueTone::Bad)
        },
        RemoteIssueKind::Connecting | RemoteIssueKind::Reconnecting => {
            (spinner_frame(spinner_tick), IssueTone::Busy)
        },
        RemoteIssueKind::Diagnostic => ("!", IssueTone::Warn),
    }
}

fn divider_geometry(cols: usize) -> (usize, usize) {
    let divider_x = cols.saturating_sub(1 + RAIL_HPAD);
    (divider_x.max(1), divider_x)
}

/// Draw a continuous vertical divider down the right edge, inset by
/// [`RAIL_HPAD`] so it has breathing room from the content pane beside it.
fn render_divider(out: &mut String, divider_x: usize, rows: usize, p: &Theme) {
    if divider_x >= 1 {
        for y in 0..rows {
            render_row(
                out,
                divider_x,
                y,
                1,
                None,
                &[Span::new("│", p.separator).dim()],
            );
        }
    }
}

/// Draw a top-down scrollbar in column `x` over `body_height` rows. Thumb size
/// and position follow herdr's `scrollbar::scrollbar_thumb` math, simplified for
/// the plugin's scroll-from-top model.
#[allow(clippy::too_many_arguments)]
fn render_scrollbar(
    out: &mut String,
    x: usize,
    body_start: usize,
    body_height: usize,
    total: usize,
    visible: usize,
    scroll: usize,
    p: &Theme,
) {
    if body_height == 0 || total <= visible {
        return;
    }
    let thumb_len = ((visible * body_height) as f32 / total as f32)
        .round()
        .max(1.0)
        .min(body_height as f32) as usize;
    let max_thumb_top = body_height.saturating_sub(thumb_len);
    let max_scroll = total.saturating_sub(visible);
    let thumb_top = if max_thumb_top == 0 || max_scroll == 0 {
        0
    } else {
        ((scroll * max_thumb_top) as f32 / max_scroll as f32)
            .round()
            .min(max_thumb_top as f32) as usize
    };

    for i in 0..body_height {
        let is_thumb = i >= thumb_top && i < thumb_top + thumb_len;
        let (symbol, color) = if is_thumb {
            ("▐", p.accent)
        } else {
            ("▕", p.separator)
        };
        let mut span = Span::new(symbol, color);
        if !is_thumb {
            // The track stays subtle; only the thumb is full-strength accent.
            span = span.dim();
        }
        render_row(out, x, body_start + i, 1, None, &[span]);
    }
}

/// Map a clicked row to the selection index whose row it occupies, if any.
pub fn index_at_row(click_map: &[ClickTarget], row: usize) -> Option<usize> {
    click_map
        .iter()
        .find(|hit| hit.row == row)
        .map(|hit| hit.index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_selection_bounds_to_target_count() {
        assert_eq!(clamp_selection(0, 5), 0);
        assert_eq!(clamp_selection(9, 5), 4);
        assert_eq!(clamp_selection(2, 5), 2);
        assert_eq!(clamp_selection(0, 0), 0);
    }

    #[test]
    fn index_at_row_finds_matching_click_target() {
        let map = vec![
            ClickTarget { row: 1, index: 0 },
            ClickTarget { row: 5, index: 3 },
        ];
        assert_eq!(index_at_row(&map, 1), Some(0));
        assert_eq!(index_at_row(&map, 5), Some(3));
        assert_eq!(index_at_row(&map, 2), None);
    }

    fn skew_issue() -> RemoteIssue {
        RemoteIssue {
            session: "api-dev".into(),
            kind: RemoteIssueKind::VersionSkew,
            daemon_version: Some("26.5.0".into()),
            local_version: Some("26.7.0".into()),
            pane_count: 3,
            retry_count: 0,
            last_error: None,
        }
    }

    #[test]
    fn issue_row_walks_the_whole_upgrade_sequence() {
        let issue = skew_issue();
        assert_eq!(
            remote_issue_text(&issue, false, None, "⠙").0,
            "⇪ v26.5.0 → 26.7.0"
        );
        // Installing cannot replace the daemon while it owns these PTYs.
        assert_eq!(
            remote_issue_text(&issue, true, None, "⠙").0,
            "⇪ click again to install"
        );
        assert_eq!(
            remote_issue_text(&issue, false, Some(&UpgradeProgress::Working), "⠙").0,
            "⠙ installing 26.7.0…"
        );
        assert_eq!(
            remote_issue_text(
                &issue,
                false,
                Some(&UpgradeProgress::InstalledPending {
                    version: "26.7.0".into(),
                    panes: Some(3)
                }),
                "⠙"
            )
            .0,
            "✓ 26.7.0 · 3 panes old"
        );
        assert_eq!(
            remote_issue_text(
                &issue,
                false,
                Some(&UpgradeProgress::Failed {
                    reason: "host unreachable".into()
                }),
                "⠙"
            )
            .0,
            "✗ host unreachable  ⏎ retry"
        );
    }

    #[test]
    fn every_issue_row_fits_the_narrowest_sidebar() {
        // A 30-column sidebar leaves 29 content columns and the row is indented
        // three. The old banner was truncated mid-sentence at this width, which
        // is what made it unusable; nothing here may repeat that.
        const BUDGET: usize = 29 - 3;
        let mut issue = skew_issue();
        let mut texts = vec![
            remote_issue_text(&issue, false, None, "⠙").0,
            remote_issue_text(&issue, true, None, "⠙").0,
            remote_issue_text(&issue, false, Some(&UpgradeProgress::Working), "⠙").0,
            remote_issue_text(
                &issue,
                false,
                Some(&UpgradeProgress::InstalledPending {
                    version: "26.7.0".into(),
                    panes: Some(3),
                }),
                "⠙",
            )
            .0,
        ];
        for kind in [
            RemoteIssueKind::Connecting,
            RemoteIssueKind::Diagnostic,
            RemoteIssueKind::ProtocolIncompatible,
            RemoteIssueKind::InstallFailed,
            RemoteIssueKind::Reconnecting,
        ] {
            issue.kind = kind;
            issue.retry_count = 3;
            texts.push(remote_issue_text(&issue, false, None, "⠙").0);
        }
        for text in texts {
            assert!(
                text.width() <= BUDGET,
                "{text:?} is {} cols, over the {BUDGET} available",
                text.width()
            );
        }
    }

    #[test]
    fn reconnecting_offers_no_action_but_every_fault_does() {
        assert!(!RemoteIssueKind::Connecting.is_actionable());
        assert!(!RemoteIssueKind::Reconnecting.is_actionable());
        assert!(!RemoteIssueKind::Diagnostic.is_actionable());
        assert!(RemoteIssueKind::VersionSkew.is_actionable());
        assert!(RemoteIssueKind::ProtocolIncompatible.is_actionable());
        assert!(RemoteIssueKind::InstallFailed.is_actionable());
    }

    #[test]
    fn truncate_text_adds_ellipsis_when_too_wide() {
        assert_eq!(truncate_text("claude", 10), "claude");
        assert_eq!(truncate_text("claude-code", 6), "claud…");
        assert_eq!(truncate_text("x", 1), "x");
        assert_eq!(truncate_text("xy", 1), "…");
    }

    #[test]
    fn agent_icon_uses_spinner_for_working() {
        let p = Theme::default();
        let (icon, color) = agent_icon(AgentState::Working, true, 0, &p);
        assert_eq!(icon, SPINNERS[0]);
        assert_eq!(color, p.yellow);
        let (done_icon, done_color) = agent_icon(AgentState::Idle, false, 0, &p);
        assert_eq!(done_icon, "●");
        assert_eq!(done_color, p.teal);
    }

    #[test]
    fn sidebar_mode_toggles_between_open_and_closed() {
        assert_eq!(SidebarMode::default(), SidebarMode::Open);
        assert_eq!(SidebarMode::Open.toggled(), SidebarMode::Closed);
        assert_eq!(SidebarMode::Closed.toggled(), SidebarMode::Open);
    }

    fn sess(name: &str, root: &str) -> SessionInfo {
        let mut s = SessionInfo::new(name.to_string());
        s.workspace_root = std::path::PathBuf::from(root);
        s
    }

    #[test]
    fn build_rows_badges_remote_bindings() {
        let mut codespace = sess("cs", "");
        codespace.default_command = Some(
            ["gh", "codespace", "ssh", "-c", "my-cs"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        let mut devcontainer = sess("dc", "/work/app");
        devcontainer.default_command = Some(
            [
                "flock",
                "remote-agent",
                "remote-pty",
                "--provider",
                "devcontainer",
                "--workspace-folder",
                "/work/app",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );
        let mut coder = sess("coder", "");
        coder.remote_backend = Some(RemoteBackend::Coder {
            workspace: "alice/api".to_string(),
            local_session_id: "coder".to_string(),
        });
        let plain = sess("plain", "/work/other");

        let rows = build_rows(
            &PaneManifest::default(),
            &[],
            &BTreeMap::new(),
            &[codespace, devcontainer, coder, plain],
        );
        let binding_of = |wanted: &str| {
            rows.iter()
                .find_map(|row| match row {
                    Row::Session { name, binding, .. } if name == wanted => Some(*binding),
                    _ => None,
                })
                .expect("session row present")
        };
        assert_eq!(binding_of("cs"), Some(crate::RemoteBinding::Codespace));
        assert_eq!(binding_of("dc"), Some(crate::RemoteBinding::Devcontainer));
        assert_eq!(binding_of("coder"), Some(crate::RemoteBinding::Coder));
        assert_eq!(binding_of("plain"), None);
    }

    #[test]
    fn cloud_workspace_badge_is_used_for_coder_and_codespaces() {
        for binding in [crate::RemoteBinding::Coder, crate::RemoteBinding::Codespace] {
            let row = Row::Session {
                name: "x".into(),
                activity: SessionActivity::None,
                is_current: false,
                binding: Some(binding),
                connection_state: None,
            };
            let mut output = String::new();
            draw_row(
                &mut output,
                &row,
                0,
                20,
                false,
                0,
                &Theme::default(),
                None,
                &BTreeMap::new(),
            );
            assert!(output.contains("☁︎  "));
        }
    }

    #[test]
    fn ordered_sessions_sort_by_workspace_root_with_unknown_last() {
        let sessions = vec![
            sess("a", "/home/u/proj"),
            sess("b", "/home/u/proj"),
            sess("c", ""),
            sess("d", "/home/u/other"),
        ];
        let ordered = ordered_sessions(&sessions);
        // Non-empty paths sort lexically; same-path keeps original order; the
        // unknown (empty) workspace trails.
        let names: Vec<&str> = ordered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["d", "a", "b", "c"]);
    }

    #[test]
    fn a_collapsed_dock_width_renders_the_rail() {
        // The columns we are given are the only signal: the server resolved the
        // collapsed mode to a narrow band, so we draw the icon rail. There is no
        // separate mode flag that could disagree with the geometry.
        let panes = PaneManifest::default();
        let tabs = Vec::new();
        let agents = BTreeMap::new();
        let sessions = vec![sess("workspace-a", "/home/u/proj")];
        let palette = Theme::default();

        let output = render(RenderInput {
            permissions_granted: true,
            panes: &panes,
            tabs: &tabs,
            agents: &agents,
            sessions: &sessions,
            palette: &palette,
            focused: false,
            selected: 0,
            scroll_sessions: 0,
            scroll_agents: 0,
            spinner_tick: 0,
            rows: 8,
            cols: THIN_WIDTH - 1,
            armed_issue: None,
            upgrade_progress: &BTreeMap::new(),
        });

        assert!(!output.ansi.contains("workspaces"));
        assert!(!output.ansi.contains("workspace-a"));
        assert!(output.click_map.iter().any(|hit| hit.index == 0));
    }

    #[test]
    fn an_expanded_dock_width_renders_labels() {
        let panes = PaneManifest::default();
        let tabs = Vec::new();
        let agents = BTreeMap::new();
        let sessions = vec![sess("workspace-a", "/home/u/proj")];
        let palette = Theme::default();

        let output = render(RenderInput {
            permissions_granted: true,
            panes: &panes,
            tabs: &tabs,
            agents: &agents,
            sessions: &sessions,
            palette: &palette,
            focused: false,
            selected: 0,
            scroll_sessions: 0,
            scroll_agents: 0,
            spinner_tick: 0,
            rows: 8,
            cols: 40,
            armed_issue: None,
            upgrade_progress: &BTreeMap::new(),
        });

        assert!(output.ansi.contains("workspace-a"));
    }

    #[test]
    fn open_sidebar_mode_draws_divider() {
        let panes = PaneManifest::default();
        let tabs = Vec::new();
        let agents = BTreeMap::new();
        let sessions = vec![sess("workspace-a", "/home/u/proj")];
        let palette = Theme::default();

        let output = render(RenderInput {
            permissions_granted: true,
            panes: &panes,
            tabs: &tabs,
            agents: &agents,
            sessions: &sessions,
            palette: &palette,
            focused: false,
            selected: 0,
            scroll_sessions: 0,
            scroll_agents: 0,
            spinner_tick: 0,
            rows: 8,
            cols: 40,
            armed_issue: None,
            upgrade_progress: &BTreeMap::new(),
        });

        assert!(output.ansi.contains("│"));
    }

    /// A session holding one remote pane whose daemon runs an older build.
    fn skewed_session(name: &str) -> SessionInfo {
        let mut session = sess(name, "/home/u/proj");
        session.remote_panes.insert(
            PaneId::Terminal(1),
            zellij_tile::prelude::RemotePaneMetadata {
                pane_uuid: "uuid".into(),
                replay_cursor: 0,
                close_pending: false,
                foreground_argv: Vec::new(),
                health: zellij_tile::prelude::RemotePaneHealth {
                    status: RemoteProtocolStatus::VersionSkew,
                    daemon_version: Some("26.5.0".into()),
                    local_version: Some("26.7.0".into()),
                    retry_count: 0,
                    last_error: None,
                },
            },
        );
        session
    }

    #[test]
    fn a_remote_issue_is_a_navigable_row_under_its_session() {
        let sessions = vec![skewed_session("api-dev")];
        let rows = build_rows(&PaneManifest::default(), &[], &BTreeMap::new(), &sessions);
        // The issue hangs directly off its session, so position names the host.
        assert!(matches!(rows[0], Row::Session { .. }));
        assert!(matches!(rows[1], Row::RemoteIssue(_)));
        // Being a row is what makes it reachable: it lands in the target list
        // that both the keyboard and the click map index into.
        let targets = navigable_targets(&PaneManifest::default(), &[], &BTreeMap::new(), &sessions);
        assert_eq!(targets[1], Target::RemoteIssue("api-dev".into()));
    }

    #[test]
    fn the_rail_shows_the_issue_the_old_banner_hid() {
        let panes = PaneManifest::default();
        let tabs = Vec::new();
        let agents = BTreeMap::new();
        let sessions = vec![skewed_session("api-dev")];
        let palette = Theme::default();
        let progress = BTreeMap::new();
        let input = |cols: usize| RenderInput {
            permissions_granted: true,
            panes: &panes,
            tabs: &tabs,
            agents: &agents,
            sessions: &sessions,
            palette: &palette,
            focused: false,
            selected: 0,
            scroll_sessions: 0,
            scroll_agents: 0,
            spinner_tick: 0,
            rows: 8,
            cols,
            armed_issue: None,
            upgrade_progress: &progress,
        };

        let full = render(input(60));
        assert!(full.ansi.contains("v26.5.0 → 26.7.0"));

        // The rail used to draw no banner at all, so a user living in it could
        // neither see the problem nor act on it. Now the session's own glyph
        // carries the warning.
        let thin = render(input(THIN_WIDTH - 1));
        assert!(thin.ansi.contains('⇪'));
    }

    #[test]
    fn a_healthy_remote_session_adds_no_row() {
        let mut session = sess("api-dev", "/home/u/proj");
        session.remote_panes.insert(
            PaneId::Terminal(1),
            zellij_tile::prelude::RemotePaneMetadata {
                pane_uuid: "uuid".into(),
                replay_cursor: 0,
                close_pending: false,
                foreground_argv: Vec::new(),
                health: Default::default(),
            },
        );
        assert!(session_remote_issue(&session).is_none());
        let rows = build_rows(&PaneManifest::default(), &[], &BTreeMap::new(), &[session]);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn an_initial_failure_is_connecting_not_reconnecting() {
        let mut session = sess("api-dev", "/home/u/proj");
        session.remote_connection_state = RemoteConnectionState::Connecting;
        session.remote_panes.insert(
            PaneId::Terminal(1),
            zellij_tile::prelude::RemotePaneMetadata {
                pane_uuid: "uuid".into(),
                replay_cursor: 0,
                close_pending: false,
                foreground_argv: Vec::new(),
                health: zellij_tile::prelude::RemotePaneHealth {
                    retry_count: 1,
                    last_error: Some("coder workspace is starting".into()),
                    ..Default::default()
                },
            },
        );

        let issue = session_remote_issue(&session).expect("an issue");
        assert_eq!(issue.kind, RemoteIssueKind::Connecting);
        assert_eq!(
            issue.last_error.as_deref(),
            Some("coder workspace is starting")
        );

        session.remote_connection_state = RemoteConnectionState::Reconnecting;
        assert_eq!(
            session_remote_issue(&session).unwrap().kind,
            RemoteIssueKind::Reconnecting
        );
    }

    #[test]
    fn the_worst_pane_decides_the_session_issue() {
        let mut session = skewed_session("api-dev");
        session.remote_panes.insert(
            PaneId::Terminal(2),
            zellij_tile::prelude::RemotePaneMetadata {
                pane_uuid: "other".into(),
                replay_cursor: 0,
                close_pending: false,
                foreground_argv: Vec::new(),
                health: zellij_tile::prelude::RemotePaneHealth {
                    status: RemoteProtocolStatus::ProtocolIncompatible,
                    daemon_version: None,
                    local_version: Some("26.7.0".into()),
                    retry_count: 2,
                    last_error: Some("incompatible protocol version 2".into()),
                },
            },
        );
        let issue = session_remote_issue(&session).expect("an issue");
        // A dead pane outranks a merely-stale one, and the count covers both.
        assert_eq!(issue.kind, RemoteIssueKind::ProtocolIncompatible);
        assert_eq!(issue.pane_count, 2);
        assert_eq!(issue.retry_count, 2);
    }

    #[test]
    fn a_collapsed_dock_width_renders_only_session_indicators() {
        use crate::detect::Agent;

        let panes = PaneManifest {
            panes: std::collections::HashMap::from([(
                0,
                vec![zellij_tile::prelude::PaneInfo {
                    id: 7,
                    is_plugin: false,
                    ..Default::default()
                }],
            )]),
        };
        let tabs = vec![TabInfo {
            position: 0,
            active: true,
            ..Default::default()
        }];
        let mut agents = BTreeMap::new();
        agents.insert(
            PaneId::Terminal(7),
            agent_pane(Agent::Codex, AgentState::Working, true),
        );
        let mut current_session = sess("workspace-a", "/home/u/proj");
        current_session.is_current_session = true;
        let sessions = vec![current_session];
        let palette = Theme::default();

        let output = render(RenderInput {
            permissions_granted: true,
            panes: &panes,
            tabs: &tabs,
            agents: &agents,
            sessions: &sessions,
            palette: &palette,
            focused: false,
            selected: 1,
            scroll_sessions: 0,
            scroll_agents: 0,
            spinner_tick: 0,
            rows: 8,
            cols: THIN_WIDTH - 1,
            armed_issue: None,
            upgrade_progress: &BTreeMap::new(),
        });

        assert_eq!(output.selected, 0);
        assert_eq!(output.click_map.len(), 1);
        assert_eq!(output.click_map[0].index, 0);
    }

    #[test]
    fn navigable_targets_follow_grouped_session_order() {
        let sessions = vec![
            sess("a", "/home/u/proj"),
            sess("c", ""),
            sess("d", "/home/u/other"),
        ];
        let targets = navigable_targets(&PaneManifest::default(), &[], &BTreeMap::new(), &sessions);
        assert_eq!(
            targets,
            vec![
                Target::Session("d".to_string()), // /home/u/other
                Target::Session("a".to_string()), // /home/u/proj
                Target::Session("c".to_string()), // unknown, last
            ]
        );
    }

    fn agent_pane(agent: crate::detect::Agent, state: AgentState, seen: bool) -> PaneAgentState {
        let mut pane = PaneAgentState::new();
        pane.detected_agent = Some(agent);
        pane.state = state;
        pane.seen = seen;
        pane
    }

    #[test]
    fn current_session_activity_rolls_up_by_attention_priority() {
        use crate::detect::Agent;
        let mut agents: BTreeMap<PaneId, PaneAgentState> = BTreeMap::new();

        // No agents → None.
        assert_eq!(current_session_activity(&agents), SessionActivity::None);

        // A single idle, seen agent → Stopped (present, nothing to do).
        agents.insert(
            PaneId::Terminal(1),
            agent_pane(Agent::Codex, AgentState::Idle, true),
        );
        assert_eq!(current_session_activity(&agents), SessionActivity::Stopped);

        // Add a working agent → Running outranks idle.
        agents.insert(
            PaneId::Terminal(2),
            agent_pane(Agent::Claude, AgentState::Working, true),
        );
        assert_eq!(current_session_activity(&agents), SessionActivity::Running);

        // Add an unseen completion → Done-unseen outranks working.
        agents.insert(
            PaneId::Terminal(3),
            agent_pane(Agent::Pi, AgentState::Idle, false),
        );
        assert_eq!(
            current_session_activity(&agents),
            SessionActivity::DoneUnseen
        );

        // Add a blocked agent → Blocked wins over everything.
        agents.insert(
            PaneId::Terminal(4),
            agent_pane(Agent::Codex, AgentState::Blocked, true),
        );
        assert_eq!(current_session_activity(&agents), SessionActivity::Blocked);
    }

    fn status(state: AgentRunState, seen: bool) -> PaneAgentStatus {
        PaneAgentStatus {
            state,
            label: "agent".to_owned(),
            seen,
        }
    }

    #[test]
    fn session_activity_from_states_buckets_cross_session_state() {
        let mut states: BTreeMap<PaneId, PaneAgentStatus> = BTreeMap::new();

        // No published agents → None.
        assert_eq!(session_activity_from_states(&states), SessionActivity::None);

        // An idle, seen agent → Stopped.
        states.insert(PaneId::Terminal(1), status(AgentRunState::Idle, true));
        assert_eq!(
            session_activity_from_states(&states),
            SessionActivity::Stopped
        );

        // A working agent in another session → Running (detectable now that the
        // state crosses the bus).
        states.insert(PaneId::Terminal(2), status(AgentRunState::Working, true));
        assert_eq!(
            session_activity_from_states(&states),
            SessionActivity::Running
        );

        // A blocked agent in another session → Blocked wins, so a workspace
        // waiting on the user shows its red ◉ here. This is the cross-session
        // win the richer rollup unlocks.
        states.insert(PaneId::Terminal(3), status(AgentRunState::Blocked, false));
        assert_eq!(
            session_activity_from_states(&states),
            SessionActivity::Blocked
        );
    }

    #[test]
    fn current_session_activity_falls_back_to_published_state() {
        let agents: BTreeMap<PaneId, PaneAgentState> = BTreeMap::new();
        let mut current_session = sess("workspace-a", "/home/u/proj");
        current_session.is_current_session = true;
        current_session
            .agent_states
            .insert(PaneId::Terminal(7), status(AgentRunState::Working, true));

        assert_eq!(
            session_activity(&current_session, &agents),
            SessionActivity::Running
        );
    }

    #[test]
    fn current_session_entries_fall_back_to_published_state() {
        let panes = PaneManifest {
            panes: std::collections::HashMap::from([(
                0,
                vec![zellij_tile::prelude::PaneInfo {
                    id: 7,
                    is_plugin: false,
                    ..Default::default()
                }],
            )]),
        };
        let tabs = vec![TabInfo {
            position: 0,
            active: true,
            ..Default::default()
        }];
        let agents: BTreeMap<PaneId, PaneAgentState> = BTreeMap::new();
        let mut current_session = sess("workspace-a", "/home/u/proj");
        current_session.is_current_session = true;
        current_session.agent_states.insert(
            PaneId::Terminal(7),
            PaneAgentStatus {
                state: AgentRunState::Working,
                label: "codex".to_owned(),
                seen: true,
            },
        );

        let rows = build_rows(&panes, &tabs, &agents, &[current_session]);
        assert!(rows.iter().any(|row| matches!(
            row,
            Row::Agent(entry)
                if entry.target == Target::Pane(PaneId::Terminal(7))
                    && entry.label == "codex"
                    && entry.state == AgentState::Working
        )));
    }

    #[test]
    fn current_session_entries_keep_published_idle_while_live_state_is_unknown() {
        use crate::detect::Agent;

        let panes = PaneManifest {
            panes: std::collections::HashMap::from([(
                0,
                vec![zellij_tile::prelude::PaneInfo {
                    id: 7,
                    is_plugin: false,
                    ..Default::default()
                }],
            )]),
        };
        let tabs = vec![TabInfo {
            position: 0,
            active: true,
            ..Default::default()
        }];
        let mut agents: BTreeMap<PaneId, PaneAgentState> = BTreeMap::new();
        agents.insert(
            PaneId::Terminal(7),
            agent_pane(Agent::Codex, AgentState::Unknown, true),
        );
        let mut current_session = sess("workspace-a", "/home/u/proj");
        current_session.is_current_session = true;
        current_session.agent_states.insert(
            PaneId::Terminal(7),
            PaneAgentStatus {
                state: AgentRunState::Idle,
                label: "codex".to_owned(),
                seen: true,
            },
        );

        let rows = build_rows(&panes, &tabs, &agents, &[current_session]);
        assert!(rows.iter().any(|row| matches!(
            row,
            Row::Agent(entry)
                if entry.target == Target::Pane(PaneId::Terminal(7))
                    && entry.label == "codex"
                    && entry.state == AgentState::Idle
                    && entry.seen
        )));
    }

    #[test]
    fn blocked_session_gets_a_distinct_red_dot() {
        let p = Theme::default();
        let (blocked_icon, blocked_color) = activity_dot(SessionActivity::Blocked, &p);
        let (idle_icon, idle_color) = activity_dot(SessionActivity::Stopped, &p);
        // Blocked is visually distinct from a merely-stopped session.
        assert_eq!(blocked_icon, "◉");
        assert_eq!(blocked_color, p.red);
        assert_ne!((blocked_icon, blocked_color), (idle_icon, idle_color));
    }
}
