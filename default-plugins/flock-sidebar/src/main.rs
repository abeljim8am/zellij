//! flock-sidebar — an agent-aware sidebar plugin for Zellij.
//!
//! Phase 2 added agent detection for the plugin's own session: it identifies
//! which panes run AI coding agents (from their `CommandChanged` argv) and
//! classifies each one's live state (Idle / Working / Blocked) by matching the
//! pane's on-screen chrome via the ported herdr detectors. The herdr async
//! polling loop becomes event-driven — `PaneRenderReportWithAnsi` pushes screen
//! content, `CommandChanged` pushes the running command, and a recurring `Timer`
//! drives the Claude working-hold / stale-hook grace windows.
//!
//! Phase 3 renders that detected state as herdr's sidebar, re-targeted from
//! `ratatui` onto the plugin's raw-ANSI output (see [`ui`]): a scrollable list
//! of per-pane agent + state rows with herdr's exact state icons/colors, plus
//! mouse scroll and click-to-focus. The same `Timer` now also advances a spinner
//! for working agents.
//!
//! Phase 4 adds unseen / notification tracking: when an agent pane finishes in
//! the background (a Working/Blocked → Idle transition while it is *not* the
//! focused pane), it shows herdr's Done-unseen icon (teal `●`) until the user
//! focuses it, then reverts to the seen icon (green `✓`). Focus is tracked from
//! `PaneUpdate`/`TabUpdate` (`is_focused` + the active tab) and fed into each
//! pane's seen arbitration — see [`State::sync_focus`] and
//! [`state::PaneAgentState::set_focused`].
//!
//! Phase 5 adds the hook channel: agents report their own state directly through
//! a `zellij pipe --name flock-state` message (requires the `ReadCliPipes`
//! permission), which [`State::pipe`] parses (see [`hook`]) and applies to the
//! target pane as a hook authority. The Phase 2 arbitration already favors a
//! hook report over screen detection — with strong visible signals still able to
//! veto a stale, non-blocked hook — so a self-report overrides screen detection
//! per that precedence. The bundled `assets/*/flock-agent-state.sh` hooks are
//! ported from herdr's, retargeted from its socket onto `zellij pipe`.
//!
//! Phase 6 gives each session a stable workspace identity. The forked server
//! records the folder it was launched in as `SessionInfo.workspace_root`, and
//! the sidebar groups sessions under that folder (see [`ui::group_sessions`])
//! instead of guessing from pane cwds.

mod codespace;
mod detect;
mod devcontainer;
mod hook;
mod palette;
mod sessionizer;
mod state;
mod ui;

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use detect::{detect_agent, identify_agent_from_command, identify_agent_from_screen, AgentState};
use hook::{parse_hook_report, HookReport, Presence, HOOK_PIPE_NAME};
use palette::{fg, Theme, BOLD, DIM, RESET};
use sessionizer::SessionizerConfig;
use state::PaneAgentState;
use ui::{ClickTarget, SidebarMode, Target};
use zellij_tile::prelude::*;

/// How often we re-evaluate time-based holds/grace windows when nothing is
/// animating. herdr polled every 300ms; we only need a tick frequent enough to
/// expire the 1.2s Claude hold and the 2s stale-hook window without a new render
/// report.
const STATE_TICK_SECS: f64 = 0.5;
/// Faster cadence used while at least one agent is working, so the spinner
/// animates smoothly (~8 frames/sec).
const SPINNER_TICK_SECS: f64 = 0.12;
/// How long without a pushed render report before we treat our session as
/// backgrounded (no client attached) and start pulling agent pane contents on
/// the timer instead. Comfortably above the slow `STATE_TICK_SECS` cadence so a
/// merely-idle foreground session keeps using the cheaper pushed reports.
const RENDER_REPORT_STALE_SECS: f64 = 1.5;
/// How often the sidebar asks the host to rescan live sessions. `SessionUpdate`
/// events only reflect the server's cached view; this command refreshes that
/// cache from the live socket/session-metadata files so the workspace section
/// contains every running session.
const SESSION_REFRESH_SECS: f64 = 1.0;
/// How often to reconcile pane command identity outside PaneUpdate. Session
/// switches can leave the plugin rendering before a fresh command event arrives.
const AGENT_COMMAND_SYNC_SECS: f64 = 1.0;
/// A retry notification stays long enough to read but never requires input.
const REMOTE_NOTIFICATION_SECS: f64 = 6.0;
const NOTIFICATION_MODE_KEY: &str = "flock_remote_notification";
const NOTIFICATION_TITLE_KEY: &str = "flock_remote_notification_title";
const NOTIFICATION_DETAIL_KEY: &str = "flock_remote_notification_detail";
const NOTIFICATION_PERSISTENT_KEY: &str = "flock_remote_notification_persistent";

/// Pipe message name (sent by a name-only `MessagePlugin` keybind, e.g. Alt b)
/// that flips the dock between its rail and its expanded width. We only relay it
/// to the server: the dock's widths come from the layout's `size`/`closed_size`,
/// and the server clamps and applies them.
const DOCK_TOGGLE_PIPE: &str = "flock-toggle-dock";

/// Pipe message name used by a keybinding to close the current session and
/// move its client to the next session in sidebar display order. The handoff
/// must happen before the kill so the user's terminal remains attached.
const CLOSE_CURRENT_SESSION_PIPE: &str = "flock-close-current-session";

/// Session name used by the flock-selector cold-shell entry point (set via its
/// `session_name` layout arg). It's the picker's throwaway host session, not a
/// workspace, so the sidebar always hides it from the workspace list. Must match
/// the `session_name` value in the bundled `flock-selector` layout.
const HIDDEN_SESSION_NAME: &str = "flock-selector";

#[derive(Default)]
struct State {
    /// Original plugin configuration, forwarded when we launch the same wasm as
    /// a short-lived floating notification instance.
    plugin_configuration: BTreeMap<String, String>,
    /// This instance is a top-right notification rather than the dock.
    is_notification: bool,
    notification_title: String,
    notification_detail: String,
    notification_persistent: bool,
    /// Deduplicates repeated health polls while still allowing an existing toast
    /// to be updated when an incident changes severity.
    remote_notification_kind: Option<ui::RemoteIssueKind>,
    /// Whether our permission request has been granted yet. Until it is, we
    /// can't read pane contents / application state, so we render a hint.
    permissions_granted: bool,
    /// Latest pane manifest for our own session.
    panes: PaneManifest,
    /// Latest tab list for our own session.
    tabs: Vec<TabInfo>,
    /// Latest cross-session list, grouped by `workspace_root` in the sidebar.
    sessions: Vec<SessionInfo>,
    /// Optional sessionizer-style filter. When configured with the same
    /// `individual_dirs` / `root_dirs` args as flock-selector, the workspace
    /// list only shows sessions that belong to those projects.
    sessionizer: SessionizerConfig,
    /// Last time we explicitly refreshed the cross-session list via
    /// `get_session_list`.
    last_session_refresh: Option<Instant>,
    /// Last time we reconciled pane ids with their foreground commands.
    last_agent_command_sync: Option<Instant>,
    /// Panes whose foreground command we've already resolved — either via a
    /// live `CommandChanged` event or one `get_pane_running_command` host
    /// query. Each host query forks a full `ps` process-table scan on the pty
    /// thread, so the manifest sync only queries panes it hasn't answered yet
    /// instead of every terminal pane on every tick; `CommandChanged` is the
    /// live path for later foreground changes.
    command_synced: HashSet<PaneId>,
    /// Per-pane agent detection + arbitrated state, keyed by pane id.
    agents: BTreeMap<PaneId, PaneAgentState>,
    /// The session whose remote-issue row has been armed for a second click.
    /// Activating anything else disarms it.
    armed_issue: Option<String>,
    /// In-flight and just-finished upgrades, keyed by session name. Entries are
    /// dropped once the health record they were fixing comes back clean.
    upgrade_progress: BTreeMap<String, ui::UpgradeProgress>,
    /// The flock binary to invoke for `remote-upgrade`, from the session
    /// environment. Absent means the upgrade action is unavailable.
    flock_executable: Option<String>,
    /// The last per-pane agent status we published to the cross-session bus
    /// (Phase 7). Diffed against the freshly-built status on each update so we
    /// only `publish_agent_state` — and thus only re-serialize the session
    /// metadata to disk — when the published picture actually changes.
    last_published: BTreeMap<PaneId, PaneAgentStatus>,
    /// Whether the recurring state tick timer has been armed.
    timer_running: bool,
    /// Sidebar colors, resolved from the user's active zellij theme (updated on
    /// each `ModeUpdate`).
    palette: Theme,
    /// User-requested sidebar presentation. Width follows this state, and the
    /// renderer uses it for both the workspaces and agents sections.
    sidebar_mode: SidebarMode,
    /// Timestamp attached to the last local/adopted sidebar mode. Cross-session
    /// sync uses this so every live sidebar converges on the newest toggle.
    sidebar_state_updated_at_millis: u64,
    /// Unified keyboard selection cursor over the sessions then the agents.
    selected: usize,
    /// Scroll offset into the workspaces (sessions) section.
    scroll_sessions: usize,
    /// Scroll offset into the agents section.
    scroll_agents: usize,
    /// Spinner animation frame counter, advanced by the timer while working.
    spinner_tick: u32,
    /// Row → selection-index map from the last render, for mouse hit-testing.
    click_map: Vec<ClickTarget>,
    /// Plugin pane dimensions from the last render, for mouse hit-testing.
    rows: usize,
    cols: usize,
    /// Our own plugin id (from `get_plugin_ids`), used to find our pane in the
    /// manifest so the selection cursor only shows while the sidebar is focused.
    own_plugin_id: u32,
    /// Whether our own plugin pane is the focused pane in the active tab. The
    /// selection cursor is hidden when this is false, so an unfocused ambient
    /// rail shows only status — no cursor.
    focused: bool,
    /// When we last received a pushed `PaneRenderReportWithAnsi`. The host only
    /// emits those while a client is attached to our session, so once they go
    /// stale (we've been switched away from) we fall back to *pulling* each
    /// agent pane's contents on the timer — see [`State::pull_agent_screens`] —
    /// keeping a backgrounded session's agent state live cross-session.
    last_render_report_at: Option<Instant>,
    /// When we last pulled agent pane contents ourselves. Pulls serialize each
    /// pane's grid across the wasm boundary, so they're clamped to the slow
    /// state cadence even when the timer runs at the spinner cadence.
    last_screen_pull: Option<Instant>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.plugin_configuration = configuration.clone();
        self.is_notification = configuration
            .get(NOTIFICATION_MODE_KEY)
            .is_some_and(|value| value == "true");
        if self.is_notification {
            self.notification_title = configuration
                .get(NOTIFICATION_TITLE_KEY)
                .cloned()
                .unwrap_or_else(|| "Remote connection problem".to_owned());
            self.notification_detail = configuration
                .get(NOTIFICATION_DETAIL_KEY)
                .cloned()
                .unwrap_or_else(|| "Connection failed".to_owned());
            self.notification_persistent = configuration
                .get(NOTIFICATION_PERSISTENT_KEY)
                .is_some_and(|value| value == "true");
            set_selectable(false);
            subscribe(&[EventType::ModeUpdate, EventType::Mouse, EventType::Timer]);
            return;
        }
        self.sessionizer = SessionizerConfig::from_args(&configuration);
        // Exclude the sidebar from focus navigation, like zellij's own tab-bar /
        // status-bar: Ctrl-h/l skip over it instead of landing on it, and it's a
        // glance-and-click ambient rail (mouse clicks still work) rather than a
        // keyboard-focusable pane.
        set_selectable(false);

        // Permissions needed across all phases:
        // - ReadApplicationState: pane/tab/session manifests
        // - ReadPaneContents: PaneRenderReportWithAnsi screen scraping (Phase 2)
        // - ChangeApplicationState: switch session / focus pane on activation,
        //   resize our pane, and publish cross-session sidebar state
        // - ReadCliPipes: agent hook reports via `zellij pipe` (Phase 5)
        // - RunCommands: `flock remote-agent remote-upgrade` from a remote-issue row
        // - MessageAndLaunchOtherPlugins: launch our non-focus-stealing toast instance
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ReadPaneContents,
            PermissionType::ChangeApplicationState,
            PermissionType::ReadCliPipes,
            PermissionType::RunCommands,
            PermissionType::MessageAndLaunchOtherPlugins,
        ]);

        subscribe(&[
            EventType::ModeUpdate,
            EventType::PaneUpdate,
            EventType::TabUpdate,
            EventType::SessionUpdate,
            EventType::CommandChanged,
            EventType::PaneRenderReportWithAnsi,
            EventType::Mouse,
            EventType::Key,
            EventType::PermissionRequestResult,
            EventType::Visible,
            EventType::Timer,
            EventType::RunCommandResult,
        ]);

        self.flock_executable = get_session_environment_variables()
            .remove("FLOCK_EXECUTABLE")
            .filter(|executable| !executable.trim().is_empty());

        // Our own pane id, to detect when the sidebar itself is focused.
        self.own_plugin_id = get_plugin_ids().plugin_id;

        // Drive the time-based stabilization windows. Re-armed on each Timer.
        set_timeout(STATE_TICK_SECS);
        self.timer_running = true;
    }

    fn update(&mut self, event: Event) -> bool {
        if self.is_notification {
            return self.update_notification(event);
        }
        let mut should_render = false;
        match event {
            Event::PermissionRequestResult(result) => {
                self.permissions_granted = matches!(result, PermissionStatus::Granted);
                if self.permissions_granted {
                    self.refresh_session_list(Instant::now());
                }
                should_render = true;
            },
            Event::ModeUpdate(mode_info) => {
                // Track the active theme so the sidebar's colors follow it.
                self.palette = Theme::from_style(&mode_info.style);
                should_render = true;
            },
            Event::PaneUpdate(manifest) => {
                let now = Instant::now();
                self.panes = manifest;
                // Drop tracked state for panes that no longer exist.
                self.prune_closed_panes();
                // Re-seed agent identity from the live pane manifest / process
                // table. When a client switches away and back, the previous
                // CommandChanged event may not replay for an already-running
                // agent, so this keeps the detail list from falling back to none.
                self.sync_agents_from_manifest(now);
                // A focus change here may clear a Done-unseen notification.
                self.sync_focus();
                should_render = true;
            },
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                // The active tab feeds which pane counts as "viewed".
                self.sync_focus();
                should_render = true;
            },
            Event::SessionUpdate(sessions, _resurrectable) => {
                self.sessions = sessions;
                self.sync_dock_mode_from_sessions();
                self.sync_remote_notification();
                should_render = true;
            },
            Event::CommandChanged(pane_id, command, is_foreground, _focused_clients) => {
                if self.apply_command_changed(pane_id, &command, is_foreground, Instant::now()) {
                    should_render = true;
                }
            },
            Event::PaneRenderReportWithAnsi(pane_contents) => {
                let now = Instant::now();
                self.last_render_report_at = Some(now);
                for (pane_id, contents) in pane_contents {
                    let screen = screen_text(&contents);
                    if self.observe_pane_screen(pane_id, &screen, now) {
                        should_render = true;
                    }
                }
            },
            Event::Timer(_) => {
                let now = Instant::now();
                for entry in self.agents.values_mut() {
                    if entry.tick(now) {
                        should_render = true;
                    }
                }
                if self.should_sync_agent_commands(now) {
                    should_render |= self.sync_agents_from_manifest(now);
                }
                // When pushed render reports have gone stale — i.e. no client is
                // attached because we've been switched away from — pull each
                // agent pane's screen ourselves so detection keeps running and we
                // keep publishing live state for the cross-session view. Pulls
                // are bounded to the slow state cadence so a working spinner
                // never turns into an 8Hz scrollback poll of a session nobody
                // is looking at.
                let reports_are_stale = self.render_reports_are_stale(now);
                if reports_are_stale && self.should_pull_agent_screens(now) {
                    self.last_screen_pull = Some(now);
                    should_render |= self.pull_agent_screens(now);
                }
                // While anything is working, animate the spinner and tick faster;
                // otherwise fall back to the slow hold/grace cadence. With stale
                // reports no client is attached, so there is no spinner to
                // animate — stay on the slow cadence.
                let working = self.any_working();
                if working {
                    self.spinner_tick = self.spinner_tick.wrapping_add(1);
                    should_render = true;
                }
                if self.should_refresh_session_list(now) {
                    should_render |= self.refresh_session_list(now);
                }
                set_timeout(if working && !reports_are_stale {
                    SPINNER_TICK_SECS
                } else {
                    STATE_TICK_SECS
                });
            },
            Event::Mouse(mouse) => match mouse {
                // The wheel moves the keyboard cursor, so scrolling and selection
                // stay in lockstep (the agent list follows the cursor in render).
                Mouse::ScrollUp(n) => {
                    self.select_prev(n.max(1));
                    should_render = true;
                },
                Mouse::ScrollDown(n) => {
                    self.select_next(n.max(1));
                    should_render = true;
                },
                Mouse::LeftClick(line, _) => {
                    if line >= 0 {
                        if let Some(index) = ui::index_at_row(&self.click_map, line as usize) {
                            self.selected = index;
                            // A second click on an armed issue row confirms it;
                            // clicking anything else backs out.
                            self.activate_selected();
                            should_render = true;
                        }
                    }
                },
                _ => {},
            },
            Event::RunCommandResult(exit_code, _stdout, stderr, context) => {
                if let Some(session) = context.get(UPGRADE_CONTEXT_KEY) {
                    self.finish_upgrade(session.clone(), exit_code, &stderr);
                    should_render = true;
                }
            },
            // Reports the *plugin's* own pane visibility, not an agent pane's,
            // so it doesn't bear on seen-tracking (that follows the focused
            // terminal pane via `PaneUpdate`). Kept subscribed for later phases.
            Event::Visible(_) => {},
            Event::Key(key) => {
                if key.has_no_modifiers() {
                    match key.bare_key {
                        // Keyboard-first navigation over sessions + agents.
                        BareKey::Up | BareKey::Char('k') => {
                            self.select_prev(1);
                            should_render = true;
                        },
                        BareKey::Down | BareKey::Char('j') => {
                            self.select_next(1);
                            should_render = true;
                        },
                        BareKey::Enter => {
                            self.activate_selected();
                            should_render = true;
                        },
                        // Do not close on Esc: one plugin instance backs the
                        // sidebar panes across tabs, so close_self() here tears
                        // down every sidebar and looks like a plugin crash.
                        _ => {},
                    }
                }
            },
            _ => {},
        }
        // Any handled event may have changed an agent's state; mirror the latest
        // picture onto the cross-session bus (no-op when unchanged).
        self.publish_state_if_changed();
        should_render
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if self.is_notification && pipe_message.name == "flock-remote-notification" {
            if let Some(title) = pipe_message.args.get(NOTIFICATION_TITLE_KEY) {
                self.notification_title = title.clone();
            }
            if let Some(detail) = pipe_message.args.get(NOTIFICATION_DETAIL_KEY) {
                self.notification_detail = detail.clone();
            }
            self.notification_persistent = pipe_message
                .args
                .get(NOTIFICATION_PERSISTENT_KEY)
                .is_some_and(|value| value == "true");
            if !self.notification_persistent {
                set_timeout(REMOTE_NOTIFICATION_SECS);
            }
            return true;
        }
        // The dock-toggle channel: a name-only `MessagePlugin` keybind, broadcast to
        // every plugin, so it can never load a plugin and therefore can never
        // conjure a second sidebar pane the way a plugin-URL pipe could.
        if pipe_message.name == DOCK_TOGGLE_PIPE {
            if !self.is_notification {
                self.toggle_dock();
            }
            return false; // the server's resize triggers our re-render
        }
        if pipe_message.name == CLOSE_CURRENT_SESSION_PIPE {
            self.close_current_session_and_switch();
            return false;
        }
        // Only the agent self-report channel concerns us otherwise; ignore the
        // rest so we don't claim pipes meant for other plugins.
        if pipe_message.name != HOOK_PIPE_NAME {
            return false;
        }
        let should_render = match parse_hook_report(&pipe_message.args) {
            Ok(report) => self.apply_hook_report(report),
            Err(reason) => {
                // A malformed report is dropped, not applied — log for the
                // operator and leave every pane's state untouched.
                eprintln!("flock-sidebar: ignoring {HOOK_PIPE_NAME} report: {reason}");
                false
            },
        };
        // A hook report can change agent state; publish it cross-session.
        self.publish_state_if_changed();
        should_render
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if self.is_notification {
            self.render_notification(cols);
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let sessions = self.render_sessions();

        let output = ui::render(ui::RenderInput {
            permissions_granted: self.permissions_granted,
            panes: &self.panes,
            tabs: &self.tabs,
            agents: &self.agents,
            sessions: &sessions,
            palette: &self.palette,
            focused: self.focused,
            selected: self.selected,
            scroll_sessions: self.scroll_sessions,
            scroll_agents: self.scroll_agents,
            spinner_tick: self.spinner_tick,
            rows,
            cols,
            armed_issue: self.armed_issue.as_deref(),
            upgrade_progress: &self.upgrade_progress,
        });
        self.selected = output.selected;
        self.scroll_sessions = output.scroll_sessions;
        self.scroll_agents = output.scroll_agents;
        self.click_map = output.click_map;
        print!("{}", output.ansi);
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

impl State {
    fn update_notification(&mut self, event: Event) -> bool {
        match event {
            Event::ModeUpdate(mode_info) => {
                self.palette = Theme::from_style(&mode_info.style);
                true
            },
            Event::Timer(_) if !self.notification_persistent => {
                close_self();
                false
            },
            Event::Mouse(Mouse::LeftClick(_, _)) => {
                close_self();
                false
            },
            _ => false,
        }
    }

    fn render_notification(&self, cols: usize) {
        let content_width = cols.saturating_sub(3);
        let title = truncate_notification(&self.notification_title, content_width);
        let detail = truncate_notification(&self.notification_detail, content_width);
        let hint = if self.notification_persistent {
            "click to dismiss · details in dock"
        } else {
            "details in dock"
        };
        print!(
            "\x1b[2J\x1b[H{}{}⚠  {}{}\r\n{}{}{}\r\n{}{}{}",
            fg(self.palette.red),
            BOLD,
            title,
            RESET,
            fg(self.palette.text),
            detail,
            RESET,
            fg(self.palette.muted),
            DIM,
            truncate_notification(hint, content_width),
        );
    }

    /// Launch one notification for a continuous incident. Session health is
    /// refreshed once a second, so keying this to retry_count or error wording
    /// would create a new floating pane on every poll/retry.
    fn sync_remote_notification(&mut self) {
        let issue = self
            .sessions
            .iter()
            .find(|session| session.is_current_session)
            .and_then(ui::session_remote_issue);
        let Some(issue) = issue else {
            self.remote_notification_kind = None;
            return;
        };
        if self.remote_notification_kind == Some(issue.kind) {
            return;
        }
        self.remote_notification_kind = Some(issue.kind);

        let notification = remote_notification_content(&issue);
        let mut configuration = self.plugin_configuration.clone();
        configuration.insert(NOTIFICATION_MODE_KEY.to_owned(), "true".to_owned());
        let notification_args = BTreeMap::from([
            (NOTIFICATION_TITLE_KEY.to_owned(), notification.title),
            (NOTIFICATION_DETAIL_KEY.to_owned(), notification.detail),
            (
                NOTIFICATION_PERSISTENT_KEY.to_owned(),
                notification.persistent.to_string(),
            ),
        ]);
        let coordinates = FloatingPaneCoordinates::new(
            Some("66%".to_owned()),
            Some("1".to_owned()),
            Some("33%".to_owned()),
            Some("6".to_owned()),
            Some(true),
            Some(false),
        )
        .unwrap_or_default();
        let message = MessageToPlugin::new("flock-remote-notification")
            .with_plugin_url("zellij:OWN_URL")
            .with_plugin_config(configuration)
            .with_args(notification_args)
            .with_floating_pane_coordinates(coordinates)
            .new_plugin_instance_should_have_pane_title("Flock remote");
        #[cfg(target_family = "wasm")]
        pipe_message_to_plugin(message);
        #[cfg(not(target_family = "wasm"))]
        let _ = message;
    }

    /// Whether any agent — in this session or any other — is currently Working.
    /// Drives the faster spinner-animation cadence; the cross-session check keeps
    /// the spinner animating for working agents shown from the published bus, not
    /// just our own panes.
    fn any_working(&self) -> bool {
        self.agents
            .values()
            .any(|st| st.is_agent() && st.state == AgentState::Working)
            || self.sessions.iter().any(|session| {
                session
                    .agent_states
                    .values()
                    .any(|status| matches!(status.state, AgentRunState::Working))
            })
    }

    /// Whether pushed render reports have gone stale (no client attached). When
    /// true we drive detection by pulling instead — see [`pull_agent_screens`].
    fn render_reports_are_stale(&self, now: Instant) -> bool {
        self.last_render_report_at
            .is_none_or(|last| now.duration_since(last).as_secs_f64() >= RENDER_REPORT_STALE_SECS)
    }

    /// Pull each tracked agent pane's on-screen contents and re-run detection.
    /// The host serves `get_pane_scrollback` straight from the pane's grid, which
    /// it maintains regardless of whether a client is attached — so this keeps a
    /// backgrounded session's agent state live when the pushed
    /// `PaneRenderReportWithAnsi` events have dried up. Returns whether any
    /// agent's state changed. Panes that have since closed return an error and
    /// are skipped (pruning removes them on the next `PaneUpdate`).
    fn pull_agent_screens(&mut self, now: Instant) -> bool {
        // Remote panes are pulled even before an agent is identified — the
        // screen is their only identification source, so a backgrounded bound
        // session must keep probing or a remote agent never appears.
        let pane_ids: Vec<PaneId> = self
            .agents
            .iter()
            .filter(|(_, st)| st.is_agent() || st.remote)
            .map(|(pane_id, _)| *pane_id)
            .collect();
        let mut changed = false;
        for pane_id in pane_ids {
            let Ok(contents) = get_pane_scrollback(pane_id, false) else {
                continue;
            };
            let screen = screen_text(&contents);
            if self.observe_pane_screen(pane_id, &screen, now) {
                changed = true;
            }
        }
        changed
    }

    /// Feed one pane's screen text through identification (remote panes) and
    /// state detection. Returns whether the arbitrated state changed.
    ///
    /// Screen identification only ever *adds* information. When a remote pane's
    /// agent reports through the remote daemon, the hook label is the identity
    /// and the daemon — which watches the agent's process — owns presence, so
    /// the screen is used purely to refine state. The screen may establish and
    /// revoke an agent only when it is the sole source, i.e. a remote host with
    /// no integration hooks installed.
    ///
    /// That asymmetry is the fix for agents blinking in and out.
    /// `identify_agent_from_screen` recognizes structural chrome for two agents
    /// only, and its Codex marker — a line starting `› ` — is generic enough to
    /// show up in other TUIs and in ordinary scrollback. Treating it as identity
    /// for a remote pane running a third agent retired the correct hook, and the
    /// pane was released a grace window later; the agent's next state transition
    /// brought it back, and the cycle repeated.
    fn observe_pane_screen(&mut self, pane_id: PaneId, screen: &str, now: Instant) -> bool {
        let entry = self.agents.entry(pane_id).or_default();
        if entry.remote && entry.hook_authority.is_none() {
            match identify_agent_from_screen(screen) {
                Some(identified) => entry.set_detected_agent(Some(identified), now),
                None => {
                    // An overlay screen (transcript viewer, model picker) hides
                    // the chrome without saying the agent is gone.
                    let agent = entry.detected_agent;
                    if agent.is_some() && !detect_agent(agent, screen).skip_state_update {
                        entry.mark_agent_missing(now);
                    }
                    false
                },
            };
        }
        let detection = detect_agent(entry.detection_agent(), screen);
        entry.observe_screen(detection, now)
    }

    /// Whether enough time has elapsed to refresh the cross-session list. Kept
    /// separate from the animation cadence so a working spinner doesn't turn
    /// into an aggressive disk/socket poll.
    fn should_refresh_session_list(&self, now: Instant) -> bool {
        self.permissions_granted
            && self
                .last_session_refresh
                .is_none_or(|last| now.duration_since(last).as_secs_f64() >= SESSION_REFRESH_SECS)
    }

    /// Whether enough time has elapsed since the last self-initiated screen
    /// pull. Keeps the pull cadence at `STATE_TICK_SECS` even when the timer
    /// itself runs at the (much faster) spinner cadence.
    fn should_pull_agent_screens(&self, now: Instant) -> bool {
        self.last_screen_pull
            .is_none_or(|last| now.duration_since(last).as_secs_f64() >= STATE_TICK_SECS)
    }

    fn should_sync_agent_commands(&self, now: Instant) -> bool {
        self.permissions_granted
            && self.last_agent_command_sync.is_none_or(|last| {
                now.duration_since(last).as_secs_f64() >= AGENT_COMMAND_SYNC_SECS
            })
    }

    /// Ask the host for a fresh live-session snapshot. The host also feeds the
    /// result back into the server's `SessionUpdate` cache, but updating our
    /// local copy here avoids waiting for the round trip.
    fn refresh_session_list(&mut self, now: Instant) -> bool {
        self.last_session_refresh = Some(now);
        match get_session_list() {
            Ok(snapshot) => {
                if self.sessions == snapshot.live_sessions {
                    false
                } else {
                    self.sessions = snapshot.live_sessions;
                    self.sync_dock_mode_from_sessions();
                    self.retire_settled_upgrades();
                    true
                }
            },
            Err(reason) => {
                eprintln!("flock-sidebar: failed to refresh session list: {reason}");
                false
            },
        }
    }

    /// Drop finished upgrade notices once the health they were fixing reads
    /// clean, so a `✓` row retires itself instead of needing a timer.
    fn retire_settled_upgrades(&mut self) {
        if self.upgrade_progress.is_empty() {
            return;
        }
        let sessions = self.visible_sessions();
        self.upgrade_progress.retain(|session, progress| {
            // A run still in flight always stays; only a finished one is
            // allowed to disappear when its session stops reporting a problem.
            if matches!(progress, ui::UpgradeProgress::Working) {
                return true;
            }
            sessions
                .iter()
                .find(|candidate| candidate.name == *session)
                .and_then(ui::session_remote_issue)
                .is_some()
        });
    }

    /// The ordered navigable targets (sessions then agents). Rebuilt on demand;
    /// the same ordering drives the render, so indices line up.
    fn targets(&self) -> Vec<Target> {
        let sessions = self.visible_sessions();
        ui::navigable_targets(&self.panes, &self.tabs, &self.agents, &sessions)
    }

    /// Sessions visible in the workspace section. The flock-selector's cold-shell
    /// entry session (named [`HIDDEN_SESSION_NAME`]) is always hidden — it's the
    /// picker's throwaway host, not a workspace. With no sessionizer config, every
    /// other live session remains visible for backwards-compatible default
    /// behavior; otherwise only sessions whose workspace is in the configured set —
    /// plus remote-bound sessions: a codespace's `workspace_root` is never a
    /// configured project folder (the workspace lives inside the codespace), and
    /// a devcontainer session's usually is, but keeping the binding check makes
    /// it visible even when the sidebar's dir args diverge from the selector's.
    fn visible_sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .filter(|session| session.name != HIDDEN_SESSION_NAME)
            .filter(|session| {
                !self.sessionizer.is_configured()
                    || self.sessionizer.contains_workspace(&session.workspace_root)
                    || session_remote_binding(session)
                        .is_some_and(|binding| self.remote_binding_enabled(binding))
            })
            .cloned()
            .map(|mut session| {
                if session.default_command.as_deref().is_some_and(|command| {
                    parse_remote_binding(command).is_some()
                        && self.parse_enabled_remote_binding(command).is_none()
                }) {
                    // UI row construction deliberately knows only binding
                    // shapes. Scrubbing disabled bindings here prevents badges
                    // and remote behavior from leaking through that layer.
                    session.default_command = None;
                }
                session
            })
            .collect()
    }

    /// The session list used for rendering, with this plugin's last published
    /// state overlaid onto the current session. The cross-session snapshot can
    /// lag a refresh behind after switching sessions; using the local publish
    /// cache avoids a one-frame Unknown icon while screen detection catches up.
    fn render_sessions(&self) -> Vec<SessionInfo> {
        let mut sessions = self.visible_sessions();
        self.overlay_last_published_agent_state(&mut sessions);
        sessions
    }

    fn overlay_last_published_agent_state(&self, sessions: &mut [SessionInfo]) {
        if self.last_published.is_empty() {
            return;
        }
        let Some(current) = sessions
            .iter_mut()
            .find(|session| session.is_current_session)
        else {
            return;
        };
        for (pane_id, status) in &self.last_published {
            if status.state == AgentRunState::Unknown {
                continue;
            }
            let should_overlay = current.agent_states.get(pane_id).is_none_or(|current| {
                current.state == AgentRunState::Unknown
                    && labels_compatible(&current.label, &status.label)
            });
            if should_overlay {
                current.agent_states.insert(*pane_id, status.clone());
            }
        }
    }

    /// Ask the server to flip the dock. The server owns the width: it resolves the
    /// mode to a column count from the layout's `size`/`closed_size`, clamps it
    /// against the tab, and applies it to every tab. We never touch geometry, so we
    /// can no longer fight the layout engine.
    fn toggle_dock(&mut self) {
        // Pass the mode we believe is current so the server can compare-and-swap.
        // A dock exists per tab, so this broadcast reaches every instance; only the
        // one whose view is still accurate should flip it. Update the local cache
        // immediately so a stale 1s disk scan cannot make the next press try to
        // open an already-open dock.
        let current = self.sidebar_mode;
        self.sidebar_mode = current.toggled();
        self.sidebar_state_updated_at_millis = now_millis();
        set_dock_mode(self.sidebar_mode.into(), Some(current.into()));
    }

    /// Track the server's dock mode, and adopt a newer one from another session.
    ///
    /// Our own session's `dock_state` is authoritative for us — the server writes it
    /// — so we mirror it rather than keeping our own idea of the mode. If some other
    /// session has a strictly newer one, we ask the server to adopt it; the server
    /// then stamps its own timestamp, which is what makes the fleet converge.
    fn sync_dock_mode_from_sessions(&mut self) -> bool {
        let mut should_render = false;
        if let Some(own) = self
            .sessions
            .iter()
            .find(|session| session.is_current_session)
            .and_then(|session| session.dock_state)
        {
            // Disk-backed scans can lag the live mode. Ignore an older own
            // dock_state so it cannot rewind a toggle we already applied.
            if own.updated_at_millis >= self.sidebar_state_updated_at_millis {
                self.sidebar_state_updated_at_millis = own.updated_at_millis;
                let own_mode = SidebarMode::from(own.mode);
                if own_mode != self.sidebar_mode {
                    self.sidebar_mode = own_mode;
                    should_render = true;
                }
            }
        }
        let Some(newest) = self
            .sessions
            .iter()
            .filter(|session| !session.is_current_session)
            .filter_map(|session| session.dock_state)
            .max_by_key(|state| state.updated_at_millis)
        else {
            return should_render;
        };
        if newest.updated_at_millis <= self.sidebar_state_updated_at_millis {
            return should_render;
        }
        let mode = SidebarMode::from(newest.mode);
        if mode == self.sidebar_mode {
            return should_render;
        }
        // Unconditional: this is "converge to the mode another session published",
        // not a flip, so there is nothing to compare against.
        set_dock_mode(mode.into(), None);
        should_render
    }

    /// Move the selection cursor up by `n`, clamped at the top.
    fn select_prev(&mut self, n: usize) {
        self.selected = self.skip_unreachable(self.selected.saturating_sub(n), false);
    }

    /// Move the selection cursor down by `n`, clamped at the last target.
    fn select_next(&mut self, n: usize) {
        let last = self.targets().len().saturating_sub(1);
        self.selected = self.skip_unreachable(self.selected.saturating_add(n).min(last), true);
    }

    /// Step past targets the current render cannot show. The rail draws one
    /// glyph per session and folds each session's remote issue into it, so
    /// stopping the cursor on an issue row there would look like a dead key.
    fn skip_unreachable(&self, mut index: usize, forward: bool) -> usize {
        if self.cols >= ui::THIN_WIDTH {
            return index;
        }
        let targets = self.targets();
        while matches!(targets.get(index), Some(Target::RemoteIssue(_))) {
            match if forward {
                index.checked_add(1).filter(|next| *next < targets.len())
            } else {
                index.checked_sub(1)
            } {
                Some(next) => index = next,
                // Nothing reachable that way; leave the cursor where it was.
                None => break,
            }
        }
        index
    }

    /// Act on the selected row: switch to a session, or focus an agent pane.
    fn activate_selected(&mut self) {
        let target = self.targets().into_iter().nth(self.selected);
        // Activating anything at all clears a pending confirm; only the armed
        // row's own activation re-arms below.
        let previously_armed = self.armed_issue.take();
        match target {
            Some(Target::Session(name)) => switch_session(Some(&name)),
            Some(Target::RemoteIssue(session)) => {
                // The dock is not keyboard-focusable. A second click is the
                // confirmation path for its actionable issue row.
                if previously_armed.as_deref() == Some(session.as_str()) {
                    self.start_upgrade(&session);
                } else if self.issue_is_actionable(&session) {
                    self.armed_issue = Some(session);
                }
            },
            Some(Target::Pane(PaneId::Terminal(id))) => focus_terminal_pane(id, false, false),
            Some(Target::Pane(PaneId::Plugin(id))) => focus_plugin_pane(id, false, false),
            None => {},
        }
    }

    /// Hand this client to the next visible session before killing the session
    /// it came from. `switch_session` waits for the server-side handoff, which
    /// keeps the following kill from severing the client mid-transition.
    fn close_current_session_and_switch(&self) {
        let sessions = self.visible_sessions();
        let Some((current, next)) = current_and_next_session(&sessions) else {
            return;
        };

        switch_session(Some(&next));
        let _ = kill_sessions(&[current.as_str()]);
    }

    /// Whether this session's issue offers an action. A reconnecting transport
    /// is already retrying, so arming a confirm over it would promise a fix the
    /// button cannot deliver.
    fn issue_is_actionable(&self, session: &str) -> bool {
        self.visible_sessions()
            .iter()
            .find(|candidate| candidate.name == session)
            .and_then(ui::session_remote_issue)
            .is_some_and(|issue| issue.kind.is_actionable())
    }

    /// Run `flock remote-agent remote-upgrade` for a session's remote backend.
    /// The binary is replaced immediately. A daemon which owns live PTYs stays
    /// on its current build until those panes close; the action must report
    /// that pending state rather than claiming those panes reconnected.
    fn start_upgrade(&mut self, session: &str) {
        let Some(backend) = self
            .visible_sessions()
            .into_iter()
            .find(|candidate| candidate.name == session)
            .and_then(|candidate| candidate.remote_backend)
        else {
            return;
        };
        let Some(argv) = remote_upgrade_argv(&backend, self.flock_executable.as_deref()) else {
            return;
        };
        self.upgrade_progress
            .insert(session.to_owned(), ui::UpgradeProgress::Working);
        run_command(
            &argv.iter().map(String::as_str).collect::<Vec<_>>(),
            BTreeMap::from_iter([(UPGRADE_CONTEXT_KEY.to_owned(), session.to_owned())]),
        );
    }

    /// Apply a parsed agent self-report (Phase 5 hook channel) to its target
    /// pane. The pane's [`PaneAgentState`] entry is created on demand — a hook
    /// can arrive before we've seen the pane's command or any render report — so
    /// a self-reporting agent shows up immediately. Returns whether the sidebar
    /// needs a repaint. The Phase 2 arbitration takes it from here: the hook is
    /// the authority unless a strong visible screen signal vetoes it.
    fn apply_hook_report(&mut self, report: HookReport) -> bool {
        let now = Instant::now();
        match report {
            HookReport::State {
                pane_id,
                agent_label,
                state,
                presence,
            } => {
                let entry = self.agents.entry(pane_id).or_default();
                match presence {
                    // A transition the agent reported itself.
                    Presence::Report => entry.set_hook_authority(agent_label, state, now),
                    // The remote daemon re-asserting a picture it still holds:
                    // proof of presence, not a new report.
                    Presence::Heartbeat => entry.refresh_hook_authority(agent_label, state, now),
                }
            },
            HookReport::Release { pane_id } => match self.agents.get_mut(&pane_id) {
                // Releasing a pane we never tracked is a no-op.
                Some(entry) => entry.clear_hook_authority(now),
                None => false,
            },
        }
    }

    /// Record how a `remote-upgrade` run ended. Installation and activation are
    /// separate outcomes: an active daemon deliberately remains old while it
    /// owns PTYs, and the row must continue to say so.
    fn finish_upgrade(&mut self, session: String, exit_code: Option<i32>, stderr: &[u8]) {
        let progress = if exit_code == Some(0) {
            match pending_upgrade_panes(stderr) {
                PendingUpgrade::No => ui::UpgradeProgress::Activated {
                    version: VERSION.to_owned(),
                },
                PendingUpgrade::Panes(panes) => ui::UpgradeProgress::InstalledPending {
                    version: VERSION.to_owned(),
                    panes: Some(panes),
                },
                PendingUpgrade::Restart => ui::UpgradeProgress::InstalledPending {
                    version: VERSION.to_owned(),
                    panes: None,
                },
            }
        } else {
            ui::UpgradeProgress::Failed {
                reason: last_error_line(stderr),
            }
        };
        self.upgrade_progress.insert(session, progress);
    }

    /// Build this session's per-pane agent status from the live tracked state
    /// and, if it differs from what we last published, push it to the server's
    /// cross-session bus (Phase 7). The server stores it on this session's
    /// `SessionInfo`, where every other session's sidebar reads it (via the
    /// session-list poll) to render this workspace's agents in full fidelity.
    /// Only agent panes are published; the diff guard means a republish with no
    /// change does not re-serialize the session metadata to disk.
    fn publish_state_if_changed(&mut self) {
        let states = self.build_publish_states();
        if states != self.last_published {
            self.last_published = states.clone();
            publish_agent_state(states);
        }
    }

    /// The per-pane agent statuses this session should publish: every live
    /// tracked agent pane, Coder remote panes included (their state arrives
    /// through the forwarded hook channel like any other hook report).
    fn build_publish_states(&self) -> BTreeMap<PaneId, PaneAgentStatus> {
        let mut states = BTreeMap::new();
        for (pane_id, st) in &self.agents {
            if !st.is_agent() {
                continue;
            }
            states.insert(*pane_id, self.status_to_publish(pane_id, st));
        }
        states
    }

    fn status_to_publish(&self, pane_id: &PaneId, st: &PaneAgentState) -> PaneAgentStatus {
        let next = PaneAgentStatus {
            state: to_run_state(st.state),
            label: st.effective_agent_label().unwrap_or_default(),
            seen: st.seen,
        };
        if next.state == AgentRunState::Unknown {
            if let Some(previous) = self.last_published.get(pane_id) {
                if previous.state != AgentRunState::Unknown
                    && labels_compatible(&next.label, &previous.label)
                {
                    return previous.clone();
                }
            }
        }
        next
    }

    /// Push the current focus picture into each tracked pane's state: a pane is
    /// "viewed" when it is the focused pane in the active tab. Focusing a pane
    /// clears its Done-unseen notification (see [`PaneAgentState::set_focused`]),
    /// and the flag also tells the next completion whether it happened under the
    /// user's eye. Only our own session's panes are in the manifest, which is
    /// exactly the set whose screens we can observe.
    fn sync_focus(&mut self) {
        let active_tab = self
            .tabs
            .iter()
            .find(|tab| tab.active)
            .map(|tab| tab.position);
        let own = self.own_plugin_id;
        let mut self_focused = false;
        for (tab_idx, panes_in_tab) in &self.panes.panes {
            let tab_is_active = active_tab == Some(*tab_idx);
            for pane in panes_in_tab {
                let pane_id = if pane.is_plugin {
                    PaneId::Plugin(pane.id)
                } else {
                    PaneId::Terminal(pane.id)
                };
                // Our own pane being focused in the active tab enables the cursor.
                if pane.is_plugin && pane.id == own && tab_is_active && pane.is_focused {
                    self_focused = true;
                }
                if let Some(entry) = self.agents.get_mut(&pane_id) {
                    entry.set_focused(tab_is_active && pane.is_focused);
                }
            }
        }
        self.focused = self_focused;
    }

    /// Reconcile tracked agent identities with the panes currently present in
    /// the manifest. `CommandChanged` remains the main live path, but it is not
    /// replayed just because a client switches back to an existing session. This
    /// pass lets an already-running agent reappear after reconnect/re-attach by
    /// asking the host for the current foreground command, falling back to the
    /// layout command stored in `PaneInfo`.
    fn sync_agents_from_manifest(&mut self, now: Instant) -> bool {
        if !self.permissions_granted {
            return false;
        }
        self.last_agent_command_sync = Some(now);
        let panes: Vec<(PaneId, Option<String>)> = self
            .panes
            .panes
            .values()
            .flatten()
            .filter(|pane| !pane.is_plugin)
            .map(|pane| (PaneId::Terminal(pane.id), pane.terminal_command.clone()))
            .filter(|(pane_id, _)| !self.command_synced.contains(pane_id))
            .collect();

        let mut changed = false;
        for (pane_id, terminal_command) in panes {
            let command = get_pane_running_command(pane_id)
                .ok()
                .filter(|command| !command.is_empty())
                .or_else(|| {
                    terminal_command
                        .as_deref()
                        .map(argv_from_terminal_command)
                        .filter(|command| !command.is_empty())
                });
            // One answer (even "no command") is enough: later foreground
            // changes arrive as CommandChanged events.
            self.command_synced.insert(pane_id);
            let Some(command) = command else {
                continue;
            };
            if self.seed_agent_command(pane_id, &command, now) {
                changed = true;
            }
        }
        changed
    }

    /// Apply a live `CommandChanged` event to the pane's tracked agent state.
    /// Returns whether a repaint is needed.
    fn apply_command_changed(
        &mut self,
        pane_id: PaneId,
        command: &[String],
        is_foreground: bool,
        now: Instant,
    ) -> bool {
        // A live event is authoritative — no need for the manifest sync to
        // ever host-query this pane.
        self.command_synced.insert(pane_id);
        if is_foreground {
            // The foreground command is the program actually running in the
            // pane; only it determines the agent.
            let agent = identify_agent_from_command(command);
            let remote_transport = agent.is_none() && self.command_is_remote_transport(command);
            let entry = self.agents.entry(pane_id).or_default();
            if agent.is_none() && (remote_transport || entry.remote) {
                // A remote transport's local argv (`gh codespace ssh …`, or an
                // ssh child of it) carries no agent identity — identification
                // and release are screen-driven (see `observe_pane_screen`),
                // so argv must neither set nor clear the agent here.
                entry.remote = true;
                return false;
            }
            if agent.is_some() {
                // A local agent took the pane's foreground over (e.g. run
                // directly inside a bound session) — argv identity wins again.
                entry.remote = false;
            }
            if agent.is_none() && entry.detected_agent.is_some() {
                // A non-agent foreground report while an agent is tracked can
                // be a transient scan miss: under a resident env wrapper
                // (devenv/nix develop) the host falls back to reporting the
                // wrapper process when the agent's line is missed. Confirm
                // through the same grace window as process exit instead of
                // clearing outright — a fresh agent report cancels it, and a
                // real exit (the wrapper's inner shell becomes the foreground
                // leader) expires the window and releases the agent.
                entry.mark_agent_missing(now);
                return false;
            }
            entry.set_detected_agent(agent, now)
        } else {
            // `is_foreground == false` means the pane's shell has no foreground
            // child at all (the host falls back to reporting the shell
            // command) — the agent process exited while the pane stayed open.
            // This is the only live signal for that transition, so open the
            // agent-missing grace window; the timer releases the agent unless
            // a fresh detection lands first (the host scan can transiently
            // miss a live process).
            if let Some(entry) = self.agents.get_mut(&pane_id) {
                entry.mark_agent_missing(now);
            }
            false
        }
    }

    fn seed_agent_command(&mut self, pane_id: PaneId, command: &[String], now: Instant) -> bool {
        let Some(agent) = identify_agent_from_command(command) else {
            // A remote transport snapshot marks the pane remote so screen
            // identification takes over; any other non-agent snapshot is
            // ignored (it must not clear an already-tracked agent either).
            if self.command_is_remote_transport(command) {
                self.agents.entry(pane_id).or_default().remote = true;
            }
            return false;
        };
        self.agents
            .entry(pane_id)
            .or_default()
            .set_detected_agent(Some(agent), now)
    }

    /// Whether a pane's argv is a remote-transport command rather than a local
    /// program: a remote binding itself (codespace SSH or the devcontainer
    /// wrapper), or — inside a bound session, where every default pane is the
    /// transport (possibly reported with rewritten argv: an ssh child, or the
    /// devcontainer CLI's node process after the wrapper's `exec`) — any
    /// command that isn't a recognized agent.
    fn command_is_remote_transport(&self, command: &[String]) -> bool {
        self.parse_enabled_remote_binding(command).is_some()
            || (self.current_session_is_bound() && identify_agent_from_command(command).is_none())
    }

    fn parse_enabled_remote_binding(&self, argv: &[String]) -> Option<RemoteBinding> {
        parse_remote_binding(argv).filter(|binding| self.remote_binding_enabled(*binding))
    }

    fn remote_binding_enabled(&self, binding: RemoteBinding) -> bool {
        match binding {
            RemoteBinding::Codespace => self.sessionizer.codespaces_enabled(),
            RemoteBinding::Devcontainer => self.sessionizer.devcontainers_enabled(),
            RemoteBinding::Coder => self.sessionizer.coder_enabled(),
            RemoteBinding::Ssh => self.sessionizer.ssh_enabled(),
        }
    }

    /// Whether the session this sidebar runs in is remote-bound (its
    /// `default_command` carries the codespace SSH or devcontainer binding).
    fn current_session_is_bound(&self) -> bool {
        self.sessions.iter().any(|session| {
            session.is_current_session
                && session_remote_binding(session)
                    .is_some_and(|binding| self.remote_binding_enabled(binding))
        })
    }

    /// Remove tracked agent state for panes that are no longer in the manifest.
    fn prune_closed_panes(&mut self) {
        let live: HashSet<PaneId> = self
            .panes
            .panes
            .values()
            .flatten()
            .map(|pane| {
                if pane.is_plugin {
                    PaneId::Plugin(pane.id)
                } else {
                    PaneId::Terminal(pane.id)
                }
            })
            .collect();
        self.agents.retain(|pane_id, _| live.contains(pane_id));
        // Pane ids are reused; forgetting closed panes lets the manifest sync
        // re-query a fresh pane that takes over an old id.
        self.command_synced.retain(|pane_id| live.contains(pane_id));
    }
}

fn truncate_notification(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut result: String = text.chars().take(width).collect();
    if text.chars().count() > width {
        result.pop();
        result.push('…');
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteNotificationContent {
    title: String,
    detail: String,
    persistent: bool,
}

fn remote_notification_content(issue: &ui::RemoteIssue) -> RemoteNotificationContent {
    let title = match issue.kind {
        ui::RemoteIssueKind::Connecting => format!("Connecting {}", issue.session),
        ui::RemoteIssueKind::Reconnecting => format!("Connection lost · {}", issue.session),
        ui::RemoteIssueKind::Diagnostic => format!("Remote warning · {}", issue.session),
        ui::RemoteIssueKind::VersionSkew => format!("Remote update · {}", issue.session),
        ui::RemoteIssueKind::ProtocolIncompatible => {
            format!("Incompatible remote · {}", issue.session)
        },
        ui::RemoteIssueKind::InstallFailed => {
            format!("Remote install failed · {}", issue.session)
        },
    };
    let detail = issue
        .last_error
        .clone()
        .unwrap_or_else(|| match issue.kind {
            ui::RemoteIssueKind::Connecting => "Initial connection failed; retrying".to_owned(),
            ui::RemoteIssueKind::Reconnecting => "Connection dropped; retrying".to_owned(),
            ui::RemoteIssueKind::Diagnostic => "The remote bridge reported a warning".to_owned(),
            ui::RemoteIssueKind::VersionSkew => "The remote daemon uses another build".to_owned(),
            ui::RemoteIssueKind::ProtocolIncompatible => {
                "The remote agent must be reinstalled".to_owned()
            },
            ui::RemoteIssueKind::InstallFailed => "Remote agent installation failed".to_owned(),
        });
    RemoteNotificationContent {
        title,
        detail,
        persistent: issue.kind.is_actionable(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingUpgrade {
    No,
    Panes(usize),
    Restart,
}

fn pending_upgrade_panes(stderr: &[u8]) -> PendingUpgrade {
    let detail = String::from_utf8_lossy(stderr);
    let Some(line) = detail
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
    else {
        return PendingUpgrade::No;
    };
    if line.contains("running daemon did not accept the retire request") {
        return PendingUpgrade::Restart;
    }
    if line.contains("live pane(s) remain on the previous daemon") {
        return line
            .split(" installed; ")
            .nth(1)
            .and_then(|suffix| suffix.split_whitespace().next())
            .and_then(|panes| panes.parse().ok())
            .map_or(PendingUpgrade::Restart, PendingUpgrade::Panes);
    }
    PendingUpgrade::No
}

/// Context key tagging a `remote-upgrade` run with the session it belongs to,
/// so its result lands on the right row.
const UPGRADE_CONTEXT_KEY: &str = "flock_remote_upgrade";

/// The `flock remote-agent remote-upgrade` argv for a session's backend.
/// Devcontainers are rebuilt rather than upgraded in place, so they are
/// deliberately excluded.
fn remote_upgrade_argv(backend: &RemoteBackend, executable: Option<&str>) -> Option<Vec<String>> {
    let mut argv = vec![
        executable?.to_owned(),
        "remote-agent".to_owned(),
        "remote-upgrade".to_owned(),
        "--provider".to_owned(),
    ];
    match backend {
        RemoteBackend::Coder { workspace, .. } => {
            argv.push("coder".to_owned());
            argv.push("--workspace".to_owned());
            argv.push(workspace.clone());
        },
        RemoteBackend::Ssh {
            destination,
            extra_args,
            ..
        } => {
            argv.push("ssh".to_owned());
            argv.push("--destination".to_owned());
            argv.push(destination.clone());
            for arg in extra_args {
                argv.push("--ssh-arg".to_owned());
                argv.push(arg.clone());
            }
        },
        RemoteBackend::Devcontainer { .. } => return None,
    }
    Some(argv)
}

/// The last non-empty line of a failed command's stderr, trimmed to something a
/// narrow row can hold. Callers have already decided this is a failure.
fn last_error_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.to_owned())
        .unwrap_or_else(|| "upgrade failed".to_owned())
}

/// Which remote transport a session/pane binding uses. Both kinds behave the
/// same everywhere in the sidebar (remote flag, screen-driven identity, wider
/// gone-grace); the variant only picks the workspace-row badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteBinding {
    Codespace,
    Devcontainer,
    Coder,
    Ssh,
}

/// Recognize any remote binding in an argv (a session's `default_command` or
/// a pane's command).
pub(crate) fn parse_remote_binding(argv: &[String]) -> Option<RemoteBinding> {
    if let Some(binding) = parse_remote_pty_binding(argv) {
        return Some(binding);
    }
    if codespace::parse_codespace_ssh(argv).is_some() {
        return Some(RemoteBinding::Codespace);
    }
    if devcontainer::parse_devcontainer_command(argv).is_some() {
        return Some(RemoteBinding::Devcontainer);
    }
    None
}

/// Recognize a `flock remote-agent remote-pty --provider <p> …` argv — the
/// bridge every provider on the remote-agent engine runs.
///
/// A pane that *is* the bridge says so in its own argv, so recognizing it here
/// makes the remote flag independent of when the session list happens to arrive.
/// Without this, coder and ssh panes fell back to "this session is bound", which
/// needs `self.sessions` to already carry the typed backend: a `CommandChanged`
/// that landed before the first session refresh left the pane marked local
/// forever — the bridge's local argv never changes, so no later event corrected
/// it — and a local absence signal could then release a live remote agent.
fn parse_remote_pty_binding(argv: &[String]) -> Option<RemoteBinding> {
    let bridge = argv
        .windows(2)
        .any(|pair| pair[0] == "remote-agent" && pair[1] == "remote-pty");
    if !bridge {
        return None;
    }
    let provider = argv
        .iter()
        .position(|arg| arg == "--provider")
        .and_then(|index| argv.get(index + 1))?;
    match provider.as_str() {
        "coder" => Some(RemoteBinding::Coder),
        "ssh" => Some(RemoteBinding::Ssh),
        "devcontainer" => Some(RemoteBinding::Devcontainer),
        _ => None,
    }
}

pub(crate) fn session_remote_binding(session: &SessionInfo) -> Option<RemoteBinding> {
    match &session.remote_backend {
        Some(RemoteBackend::Coder { .. }) => Some(RemoteBinding::Coder),
        Some(RemoteBackend::Ssh { .. }) => Some(RemoteBinding::Ssh),
        Some(RemoteBackend::Devcontainer { .. }) => Some(RemoteBinding::Devcontainer),
        None => session
            .default_command
            .as_deref()
            .and_then(parse_remote_binding),
    }
}

/// Resolve the current session and its successor using the same stable ordering
/// as the sidebar. The last session wraps to the first; with no distinct target
/// the operation is intentionally a no-op.
fn current_and_next_session(sessions: &[SessionInfo]) -> Option<(String, String)> {
    let ordered = ui::ordered_sessions(sessions);
    if ordered.len() < 2 {
        return None;
    }
    let current_index = ordered
        .iter()
        .position(|session| session.is_current_session)?;
    let current = ordered[current_index].name.clone();
    let next = ordered
        .iter()
        .cycle()
        .skip(current_index + 1)
        .take(ordered.len() - 1)
        .find(|session| !session.is_current_session)?
        .name
        .clone();
    Some((current, next))
}

fn argv_from_terminal_command(command: &str) -> Vec<String> {
    command.split_whitespace().map(String::from).collect()
}

fn labels_compatible(left: &str, right: &str) -> bool {
    left.is_empty() || right.is_empty() || left == right
}

/// Map the plugin's internal agent state to the serializable, cross-session
/// [`AgentRunState`] carried on `SessionInfo` (Phase 7).
fn to_run_state(state: AgentState) -> AgentRunState {
    match state {
        AgentState::Idle => AgentRunState::Idle,
        AgentState::Working => AgentRunState::Working,
        AgentState::Blocked => AgentRunState::Blocked,
        AgentState::Unknown => AgentRunState::Unknown,
    }
}

/// Flatten a pane's viewport into a single screen-text snapshot for detection.
///
/// `PaneRenderReportWithAnsi` lines carry SGR/CSI escape sequences; herdr's
/// detectors are written for the rendered plain text (they inspect the first
/// glyph of a line, match literal chrome strings, etc.), so strip the escapes
/// first while preserving the visible glyphs and spacing.
fn screen_text(contents: &PaneContents) -> String {
    let mut out = String::new();
    for line in &contents.viewport {
        strip_ansi_into(line, &mut out);
        out.push('\n');
    }
    out
}

/// Append `line` to `out` with ANSI escape sequences removed.
fn strip_ansi_into(line: &str, out: &mut String) {
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            // CSI: ESC [ ... <final byte 0x40–0x7E>
            Some('[') => {
                chars.next();
                for p in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&p) {
                        break;
                    }
                }
            },
            // OSC: ESC ] ... terminated by BEL or ST (ESC \)
            Some(']') => {
                chars.next();
                while let Some(p) = chars.next() {
                    if p == '\u{07}' {
                        break;
                    }
                    if p == '\u{1b}' {
                        if matches!(chars.peek(), Some('\\')) {
                            chars.next();
                        }
                        break;
                    }
                }
            },
            // Other escape: ESC <single byte>
            Some(_) => {
                chars.next();
            },
            None => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_notifications_are_transient_and_keep_the_provider_diagnostic() {
        let content = remote_notification_content(&ui::RemoteIssue {
            session: "api-dev".into(),
            kind: ui::RemoteIssueKind::Connecting,
            daemon_version: None,
            local_version: None,
            pane_count: 1,
            retry_count: 1,
            last_error: Some("coder workspace is starting".into()),
        });

        assert_eq!(content.title, "Connecting api-dev");
        assert_eq!(content.detail, "coder workspace is starting");
        assert!(!content.persistent);
    }

    #[test]
    fn actionable_remote_notifications_wait_for_dismissal() {
        let content = remote_notification_content(&ui::RemoteIssue {
            session: "api-dev".into(),
            kind: ui::RemoteIssueKind::ProtocolIncompatible,
            daemon_version: None,
            local_version: Some("26.10.1".into()),
            pane_count: 1,
            retry_count: 2,
            last_error: Some("incompatible protocol version 2".into()),
        });

        assert!(content.persistent);
        assert!(content.title.contains("Incompatible remote"));
    }
    use crate::detect::Agent;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn state_with_provider(key: &str) -> State {
        let configuration = BTreeMap::from_iter([(key.to_owned(), "true".to_owned())]);
        State {
            sessionizer: SessionizerConfig::from_args(&configuration),
            ..State::default()
        }
    }

    #[test]
    fn remote_upgrade_argv_uses_the_nested_remote_agent_command() {
        let backend = RemoteBackend::Coder {
            workspace: "alice/api".into(),
            local_session_id: "api".into(),
        };

        assert_eq!(
            remote_upgrade_argv(&backend, Some("/opt/flock/bin/flock")),
            Some(vec![
                "/opt/flock/bin/flock".into(),
                "remote-agent".into(),
                "remote-upgrade".into(),
                "--provider".into(),
                "coder".into(),
                "--workspace".into(),
                "alice/api".into(),
            ])
        );
    }

    #[test]
    fn successful_install_distinguishes_activation_from_a_live_old_daemon() {
        assert_eq!(pending_upgrade_panes(b""), PendingUpgrade::No);
        assert_eq!(
            pending_upgrade_panes(
                b"flock: Coder workspace api installed; 4 live pane(s) remain on the previous daemon until they close\n"
            ),
            PendingUpgrade::Panes(4)
        );
        assert_eq!(
            pending_upgrade_panes(
                b"flock: SSH host dev installed, but the running daemon did not accept the retire request (EOF); its live panes remain on the previous build until that daemon restarts\n"
            ),
            PendingUpgrade::Restart
        );
    }

    #[test]
    fn disabled_remote_bindings_are_not_recognized_or_badged() {
        let mut state = State::default();
        let command = argv(&["gh", "codespace", "ssh", "-c", "my-cs"]);
        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(pane_id, &command, true, Instant::now());
        assert!(!state.agents.get(&pane_id).unwrap().remote);

        let mut session = SessionInfo::new("api".into());
        session.default_command = Some(command);
        state.sessions = vec![session];
        let visible = state.visible_sessions();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].default_command, None);
    }

    #[test]
    fn typed_coder_binding_keeps_session_visible_and_marks_its_panes_remote() {
        let mut state = state_with_provider("coder_enabled");
        let mut session = SessionInfo::new("wooli-test".into());
        session.is_current_session = true;
        session.remote_backend = Some(RemoteBackend::Coder {
            workspace: "abeljim/wooli-test".into(),
            local_session_id: "wooli-test".into(),
        });
        state.sessions = vec![session];

        assert_eq!(state.visible_sessions().len(), 1);
        assert!(state.current_session_is_bound());

        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(
            pane_id,
            &argv(&[
                "/tmp/flock",
                "remote-agent",
                "remote-pty",
                "--provider",
                "coder",
                "--workspace",
                "abeljim/wooli-test",
            ]),
            true,
            Instant::now(),
        );
        assert!(state.agents.get(&pane_id).unwrap().remote);
    }

    #[test]
    fn typed_ssh_binding_keeps_session_visible_and_marks_its_panes_remote() {
        let mut state = state_with_provider("ssh_enabled");
        let mut session = SessionInfo::new("dev-box".into());
        session.is_current_session = true;
        session.remote_backend = Some(RemoteBackend::Ssh {
            name: "Dev Box".into(),
            destination: "abel@dev.example.com".into(),
            extra_args: vec!["-p".into(), "2222".into()],
            local_session_id: "dev-box".into(),
        });
        state.sessions = vec![session];

        assert_eq!(state.visible_sessions().len(), 1);
        assert!(state.current_session_is_bound());

        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(
            pane_id,
            &argv(&[
                "/tmp/flock",
                "remote-agent",
                "remote-pty",
                "--provider",
                "ssh",
                "--destination",
                "abel@dev.example.com",
                "--ssh-arg",
                "-p",
                "--ssh-arg",
                "2222",
            ]),
            true,
            Instant::now(),
        );
        assert!(state.agents.get(&pane_id).unwrap().remote);
    }

    #[test]
    fn native_coder_session_publishes_hook_reported_agent_state() {
        let mut state = state_with_provider("coder_enabled");
        let mut session = SessionInfo::new("wooli-test".into());
        session.is_current_session = true;
        session.remote_backend = Some(RemoteBackend::Coder {
            workspace: "abeljim/wooli-test".into(),
            local_session_id: "wooli-test".into(),
        });
        state.sessions = vec![session];

        let pane_id = PaneId::Terminal(3);
        state.agents.entry(pane_id).or_default().set_hook_authority(
            "claude".into(),
            AgentState::Working,
            Instant::now(),
        );

        let published = state.build_publish_states();
        let status = published.get(&pane_id).expect("coder pane published");
        assert_eq!(status.state, AgentRunState::Working);
        assert_eq!(status.label, "claude");
    }

    #[test]
    fn disabled_devcontainer_binding_is_not_recognized() {
        let mut state = State::default();
        let mut session = SessionInfo::new("api".into());
        session.is_current_session = true;
        session.default_command = Some(devcontainer_binding("/work/api"));
        state.sessions = vec![session];
        // The provider is off, so the bound session is not treated as remote.
        assert!(!state.current_session_is_bound());
    }

    #[test]
    fn transport_argv_marks_pane_remote_without_agent() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);

        let changed = state.apply_command_changed(
            pane_id,
            &argv(&["gh", "codespace", "ssh", "-c", "my-cs"]),
            true,
            now,
        );
        assert!(!changed);
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(entry.remote);
        assert!(!entry.is_agent());
    }

    #[test]
    fn remote_pane_identifies_agent_from_screen_and_releases_on_absence() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(
            pane_id,
            &argv(&["gh", "codespace", "ssh", "-c", "my-cs"]),
            true,
            now,
        );

        // Claude's chrome renders over the SSH transport → identified + tracked.
        let claude_screen = "✳ Simplifying…\n─────────\n❯ \n─────────";
        assert!(state.observe_pane_screen(pane_id, claude_screen, now));
        let entry = state.agents.get(&pane_id).unwrap();
        assert_eq!(entry.detected_agent, Some(Agent::Claude));
        assert_eq!(entry.state, AgentState::Working);

        // The chrome disappears (agent exited to the remote shell): the
        // absence window opens, and the remote grace releases the agent.
        state.observe_pane_screen(pane_id, "user@codespace:~/repo$ ", now);
        let entry = state.agents.get_mut(&pane_id).unwrap();
        assert!(entry.is_agent(), "still tracked inside the grace window");
        assert!(entry.tick(now + state::AGENT_GONE_GRACE));
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(!entry.is_agent());
        assert!(
            entry.remote,
            "the transport stays remote for re-identification"
        );
    }

    #[test]
    fn remote_pane_constant_transport_argv_never_clears_identified_agent() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);
        let transport = argv(&["gh", "codespace", "ssh", "-c", "my-cs"]);
        state.apply_command_changed(pane_id, &transport, true, now);
        let claude_screen = "some output\n─────────\n❯ \n─────────";
        state.observe_pane_screen(pane_id, claude_screen, now);
        assert_eq!(
            state.agents.get(&pane_id).and_then(|e| e.detected_agent),
            Some(Agent::Claude)
        );

        // The host keeps reporting the transport argv — that says nothing
        // about the remote agent and must not open the missing window.
        assert!(!state.apply_command_changed(pane_id, &transport, true, now));
        assert!(!state
            .agents
            .get_mut(&pane_id)
            .unwrap()
            .tick(now + state::AGENT_GONE_GRACE));
        assert_eq!(
            state.agents.get(&pane_id).and_then(|e| e.detected_agent),
            Some(Agent::Claude)
        );
    }

    #[test]
    fn local_agent_argv_wins_back_a_remote_pane() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(
            pane_id,
            &argv(&["gh", "codespace", "ssh", "-c", "my-cs"]),
            true,
            now,
        );
        assert!(state.agents.get(&pane_id).unwrap().remote);

        // A local agent takes the foreground (e.g. run from a local shell
        // pane) — argv identity applies again and the remote flag drops.
        assert!(state.apply_command_changed(pane_id, &argv(&["claude"]), true, now));
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(!entry.remote);
        assert_eq!(entry.detected_agent, Some(Agent::Claude));
    }

    #[test]
    fn seed_marks_transport_snapshot_remote() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(9);
        assert!(!state.seed_agent_command(
            pane_id,
            &argv(&["gh", "codespace", "ssh", "-c", "my-cs"]),
            now
        ));
        assert!(state.agents.get(&pane_id).unwrap().remote);
    }

    fn devcontainer_binding(path: &str) -> Vec<String> {
        argv(&[
            "flock",
            "remote-agent",
            "remote-pty",
            "--provider",
            "devcontainer",
            "--workspace-folder",
            path,
        ])
    }

    #[test]
    fn devcontainer_wrapper_argv_marks_pane_remote_without_agent() {
        let mut state = state_with_provider("devcontainers_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);

        let changed =
            state.apply_command_changed(pane_id, &devcontainer_binding("/work/app"), true, now);
        assert!(!changed);
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(entry.remote);
        assert!(!entry.is_agent());
    }

    /// After the wrapper's `exec`, the host process walk reports the
    /// devcontainer CLI's node argv — not the binding shape — so the pane must
    /// be marked remote through the bound-*session* branch (the session's
    /// `default_command` still carries the wrapper).
    #[test]
    fn session_bound_devcontainer_marks_rewritten_node_argv_remote() {
        let mut state = state_with_provider("devcontainers_enabled");
        let now = Instant::now();
        let mut session = SessionInfo::new("app".to_string());
        session.is_current_session = true;
        session.default_command = Some(devcontainer_binding("/work/app"));
        state.sessions = vec![session];

        let pane_id = PaneId::Terminal(4);
        let node_argv = argv(&[
            "node",
            "/usr/local/lib/node_modules/@devcontainers/cli/devcontainer.js",
            "exec",
            "--workspace-folder",
            "/work/app",
        ]);
        assert!(!state.apply_command_changed(pane_id, &node_argv, true, now));
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(entry.remote);
        assert!(!entry.is_agent());
    }

    #[test]
    fn seed_marks_devcontainer_transport_snapshot_remote() {
        let mut state = state_with_provider("devcontainers_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(9);
        assert!(!state.seed_agent_command(pane_id, &devcontainer_binding("/work/app"), now));
        assert!(state.agents.get(&pane_id).unwrap().remote);
    }

    #[test]
    fn seed_agent_command_recreates_missing_agent_entry() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        let command = vec!["/opt/homebrew/bin/codex".to_string()];

        assert!(state.seed_agent_command(pane_id, &command, now));
        assert_eq!(
            state
                .agents
                .get(&pane_id)
                .and_then(|pane| pane.detected_agent),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn seed_agent_command_does_not_clear_existing_agent_on_plain_shell_snapshot() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        state.seed_agent_command(pane_id, &["codex".to_string()], now);

        assert!(!state.seed_agent_command(pane_id, &["zsh".to_string()], now));
        assert_eq!(
            state
                .agents
                .get(&pane_id)
                .and_then(|pane| pane.detected_agent),
            Some(Agent::Codex)
        );
        assert!(state
            .agents
            .get(&pane_id)
            .is_some_and(|pane| pane.is_agent()));
    }

    #[test]
    fn foreground_exit_releases_agent_after_grace() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);

        assert!(state.apply_command_changed(
            pane_id,
            &["/opt/homebrew/bin/claude".to_string()],
            true,
            now
        ));
        assert!(state
            .agents
            .get(&pane_id)
            .is_some_and(|pane| pane.is_agent()));

        // Claude exits: the host reports the shell itself, no foreground child.
        assert!(!state.apply_command_changed(
            pane_id,
            &["/opt/homebrew/bin/fish".to_string()],
            false,
            now
        ));
        // Still shown inside the grace window (the scan may have missed).
        assert!(state
            .agents
            .get(&pane_id)
            .is_some_and(|pane| pane.is_agent()));

        // The timer tick past the grace window releases the agent.
        let entry = state.agents.get_mut(&pane_id).expect("tracked pane");
        assert!(entry.tick(now + crate::state::AGENT_GONE_GRACE));
        assert!(!entry.is_agent());
    }

    #[test]
    fn non_agent_foreground_report_opens_grace_for_detected_agent() {
        use std::time::Duration;
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        assert!(state.apply_command_changed(pane_id, &["claude".to_string()], true, now));

        // The host transiently reports the resident devenv wrapper instead of
        // the agent (a scan miss) — the agent must survive the grace window.
        assert!(!state.apply_command_changed(
            pane_id,
            &["devenv".to_string(), "shell".to_string()],
            true,
            now
        ));
        assert!(state
            .agents
            .get(&pane_id)
            .is_some_and(|pane| pane.is_agent()));

        // A fresh agent report cancels the pending release.
        state.apply_command_changed(
            pane_id,
            &["claude".to_string()],
            true,
            now + Duration::from_secs(1),
        );
        let entry = state.agents.get_mut(&pane_id).expect("tracked pane");
        assert!(!entry.tick(now + crate::state::AGENT_GONE_GRACE + Duration::from_secs(1)));
        assert!(entry.is_agent());
    }

    #[test]
    fn non_agent_foreground_report_releases_agent_after_grace() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        state.apply_command_changed(pane_id, &["claude".to_string()], true, now);

        // Claude exits inside the devenv shell: the wrapper's inner shell is
        // now the foreground leader, so the host keeps reporting a foreground
        // command that is not an agent.
        assert!(!state.apply_command_changed(pane_id, &["bash".to_string()], true, now));

        let entry = state.agents.get_mut(&pane_id).expect("tracked pane");
        assert!(entry.tick(now + crate::state::AGENT_GONE_GRACE));
        assert!(!entry.is_agent());
    }

    #[test]
    fn non_agent_foreground_report_keeps_hook_only_agent() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        state.agents.entry(pane_id).or_default().set_hook_authority(
            "custom-agent".into(),
            AgentState::Working,
            now,
        );

        // A hook-only agent has no detected process identity; an unrelated
        // foreground command must not open the release window for it.
        assert!(!state.apply_command_changed(pane_id, &["vim".to_string()], true, now));

        let entry = state.agents.get_mut(&pane_id).expect("tracked pane");
        assert!(!entry.tick(now + crate::state::AGENT_GONE_GRACE));
        assert!(entry.is_agent());
    }

    #[test]
    fn foreground_exit_for_untracked_pane_is_ignored() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(9);

        // A shell-only pane reporting "no foreground child" must not create
        // an agent entry (or crash) — it was never tracked.
        assert!(!state.apply_command_changed(pane_id, &["/bin/zsh".to_string()], false, now));
        assert!(!state.agents.contains_key(&pane_id));
        // But the answer still counts as synced: no host re-query needed.
        assert!(state.command_synced.contains(&pane_id));
    }

    #[test]
    fn terminal_command_string_can_seed_agent_identity() {
        let argv =
            argv_from_terminal_command("/opt/homebrew/bin/claude --dangerously-skip-permissions");

        assert_eq!(identify_agent_from_command(&argv), Some(Agent::Claude));
    }

    #[test]
    fn remote_pty_panes_are_marked_remote_without_a_session_list() {
        // The ordering hazard: a CommandChanged can arrive before the first
        // session refresh, and the bridge's local argv never changes afterwards,
        // so a pane misjudged here would stay misjudged for its whole life.
        let mut state = state_with_provider("coder_enabled");
        assert!(state.sessions.is_empty());

        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(
            pane_id,
            &argv(&[
                "/tmp/flock",
                "remote-agent",
                "remote-pty",
                "--provider",
                "coder",
                "--workspace",
                "abeljim/wooli-test",
            ]),
            true,
            Instant::now(),
        );

        assert!(state.agents.get(&pane_id).unwrap().remote);
    }

    #[test]
    fn a_remote_agent_survives_a_local_no_foreground_child_report() {
        // End-to-end over the local paths that used to release remote agents:
        // the hook establishes the agent, then the host reports the pane has no
        // foreground child. On a remote pane that says nothing about the agent.
        let mut state = state_with_provider("coder_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);
        let bridge = argv(&[
            "/tmp/flock",
            "remote-agent",
            "remote-pty",
            "--provider",
            "coder",
            "--workspace",
            "abeljim/wooli-test",
        ]);
        state.apply_command_changed(pane_id, &bridge, true, now);
        state.apply_hook_report(HookReport::State {
            pane_id,
            agent_label: "opencode".into(),
            state: AgentState::Working,
            presence: Presence::Report,
        });
        assert!(state.agents.get(&pane_id).unwrap().is_agent());

        state.apply_command_changed(pane_id, &bridge, false, now);
        // A screen with no recognizable agent chrome, as during a full-screen
        // tool or a torn frame.
        state.observe_pane_screen(pane_id, "$ ", now);

        let entry = state.agents.get_mut(&pane_id).unwrap();
        assert!(!entry.tick(now + state::AGENT_GONE_GRACE * 100));
        assert!(entry.is_agent());
        assert_eq!(entry.state, AgentState::Working);
    }

    #[test]
    fn a_remote_agent_is_not_misidentified_by_generic_codex_chrome() {
        // The primary flicker path: a `› ` line is Codex's screen marker but
        // appears in other TUIs and in scrollback. It must not retire the
        // opencode hook that actually owns this pane.
        let mut state = state_with_provider("coder_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(
            pane_id,
            &argv(&[
                "/tmp/flock",
                "remote-agent",
                "remote-pty",
                "--provider",
                "coder",
            ]),
            true,
            now,
        );
        state.apply_hook_report(HookReport::State {
            pane_id,
            agent_label: "opencode".into(),
            state: AgentState::Working,
            presence: Presence::Report,
        });

        state.observe_pane_screen(pane_id, "some output\n› \n", now);

        let entry = state.agents.get(&pane_id).unwrap();
        assert_eq!(entry.detected_agent, None, "screen must not claim identity");
        assert_eq!(entry.effective_agent_label().as_deref(), Some("opencode"));
        assert!(entry.hook_authority.is_some(), "hook must survive");
    }

    #[test]
    fn publish_preserves_previous_state_during_unknown_warmup() {
        let mut state = State::default();
        let pane_id = PaneId::Terminal(7);
        state.last_published.insert(
            pane_id,
            PaneAgentStatus {
                state: AgentRunState::Idle,
                label: "codex".to_owned(),
                seen: true,
            },
        );
        let mut pane = PaneAgentState::new();
        pane.detected_agent = Some(Agent::Codex);
        pane.state = AgentState::Unknown;

        let status = state.status_to_publish(&pane_id, &pane);

        assert_eq!(status.state, AgentRunState::Idle);
        assert_eq!(status.label, "codex");
        assert!(status.seen);
    }

    #[test]
    fn render_sessions_overlay_last_published_current_state() {
        let mut state = State::default();
        let pane_id = PaneId::Terminal(7);
        let mut current = SessionInfo::new("workspace-a".to_string());
        current.is_current_session = true;
        state.sessions = vec![current];
        state.last_published.insert(
            pane_id,
            PaneAgentStatus {
                state: AgentRunState::Idle,
                label: "codex".to_owned(),
                seen: true,
            },
        );

        let sessions = state.render_sessions();

        assert_eq!(
            sessions[0]
                .agent_states
                .get(&pane_id)
                .map(|status| status.state),
            Some(AgentRunState::Idle)
        );
    }

    #[test]
    fn close_session_target_follows_sidebar_order() {
        let mut current = SessionInfo::new("current".to_string());
        current.workspace_root = "/work/b".into();
        current.is_current_session = true;
        let mut next = SessionInfo::new("next".to_string());
        next.workspace_root = "/work/c".into();
        let mut previous = SessionInfo::new("previous".to_string());
        previous.workspace_root = "/work/a".into();

        assert_eq!(
            current_and_next_session(&[next, current, previous]),
            Some(("current".to_string(), "next".to_string()))
        );
    }

    #[test]
    fn close_session_target_wraps_to_first_sidebar_session() {
        let mut first = SessionInfo::new("first".to_string());
        first.workspace_root = "/work/a".into();
        let mut current = SessionInfo::new("current".to_string());
        current.workspace_root = "/work/z".into();
        current.is_current_session = true;

        assert_eq!(
            current_and_next_session(&[current, first]),
            Some(("current".to_string(), "first".to_string()))
        );
    }

    #[test]
    fn close_session_target_requires_another_session() {
        let mut current = SessionInfo::new("only".to_string());
        current.is_current_session = true;

        assert_eq!(current_and_next_session(&[current]), None);
        assert_eq!(
            current_and_next_session(&[
                SessionInfo::new("one".to_string()),
                SessionInfo::new("two".to_string()),
            ]),
            None
        );
    }

    #[test]
    fn stale_own_dock_state_does_not_rewind_a_local_toggle() {
        let mut current = SessionInfo::new("native".to_string());
        current.is_current_session = true;
        current.dock_state = Some(DockState {
            mode: DockMode::Closed,
            updated_at_millis: 1_000,
        });
        let mut state = State {
            sessions: vec![current],
            sidebar_mode: SidebarMode::Open,
            sidebar_state_updated_at_millis: 2_000,
            ..State::default()
        };

        assert!(!state.sync_dock_mode_from_sessions());
        assert_eq!(state.sidebar_mode, SidebarMode::Open);
        assert_eq!(state.sidebar_state_updated_at_millis, 2_000);
    }

    #[test]
    fn newer_own_dock_state_updates_local_mode() {
        let mut current = SessionInfo::new("native".to_string());
        current.is_current_session = true;
        current.dock_state = Some(DockState {
            mode: DockMode::Closed,
            updated_at_millis: 2_000,
        });
        let mut state = State {
            sessions: vec![current],
            sidebar_mode: SidebarMode::Open,
            sidebar_state_updated_at_millis: 1_000,
            ..State::default()
        };

        assert!(state.sync_dock_mode_from_sessions());
        assert_eq!(state.sidebar_mode, SidebarMode::Closed);
        assert_eq!(state.sidebar_state_updated_at_millis, 2_000);
    }
}
