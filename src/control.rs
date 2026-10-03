//! `duetctl`: the local control interface — a Unix-domain socket (the
//! smallest IPC primitive that already fits duet's local-first,
//! single-user-desktop scope) that agents and the `duetctl`/`duet agent`
//! CLIs talk to, both dispatched from `main.rs` before GTK is ever touched.
//! See Milestone 3 section 6: GTK and `duetctl` invoke the exact same
//! `App`/`orchestration` services — this module only parses argv and the
//! wire protocol, never routes or authorizes anything itself.
//!
//! The socket is accepted on a background thread (`spawn_server`), but
//! every request is actually answered on the GTK main thread: each
//! connection packages its parsed `ControlRequest` with a one-shot reply
//! channel into a `ControlEvent` and hands it to `main.rs`'s existing
//! timer-poll loop (the same `std::sync::mpsc` + `glib::timeout_add_local`
//! shape `App::pump_output` already uses), which calls `dispatch` against
//! the live `App`: most requests are answered on the spot by
//! `handle_request`; a browser-portal page operation (Milestone 8) is
//! asynchronous in WebKit, so its reply channel is handed to the
//! `PortalService` callback and answered from there, never blocking the
//! loop. `App`'s session map
//! and GTK widgets are not `Send`, so this is the one safe way to reach
//! them from a request that arrived on another thread.

use crate::agent::Agent;
use crate::app::{App, PortalStep};
use crate::message::{
    AgentInfo, AgentMessage, AgentSummary, ConnectionInfo, LinkSummary, NoteDetail, NoteSummary,
    PortalAction, PortalInfo, PortalPage, PortalScreenshot, PortalSummary, WhoamiInfo,
    WorkspaceInfo,
};
use crate::orchestration::resource::{ResolveOutcome, ResolvedResource, ResourceDetail};
use crate::project::fs::DirEntry;
use crate::project::git::{DiffScope, GitLogEntry, GitStatus};
use crate::project::path::LineRange;
use crate::project::search::{ContentSearchResult, NameMatch};
use crate::project::service::{FileContent, FileInfo};
use crate::project::sync::FileRevision;
use crate::store::default_control_socket_path;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    /// `duet agent list` (preserved for backward compatibility; prefer
    /// `AgentsList`/`duetctl agents list` for new scripts).
    List,
    /// `duet agent send <target> "..."` (preserved; prefer `SendMessage`/
    /// `duetctl send --from --to`).
    Send {
        #[serde(default)]
        source_session_id: Option<Uuid>,
        target: String,
        content: String,
    },
    AgentsList,
    AgentsInspect {
        target: String,
    },
    SendMessage {
        #[serde(default)]
        from: Option<Uuid>,
        to: String,
        content: String,
    },
    ConnectionsList,
    WorkspaceInspect,
    AgentsCreate {
        name: String,
        cwd: PathBuf,
        /// A provider keyword (`claude`/`codex`/`opencode`/`shell`) — see
        /// `parse_provider`. `Agent::Custom` isn't reachable from this CLI
        /// yet (no acceptance-test or roadmap need for it this milestone).
        agent: String,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        requested_by: Option<Uuid>,
    },
    AgentsRemove {
        target: String,
        #[serde(default)]
        requested_by: Option<Uuid>,
    },
    AgentsAssignRole {
        target: String,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        requested_by: Option<Uuid>,
    },
    /// `duetctl whoami`: what the `duet` skill tells an agent to run first
    /// in every session, instead of having its role and connections resent
    /// as prompt text — see `orchestration::skill`. `agent_id` is resolved
    /// from `DUET_AGENT_ID`/`DUET_SESSION_ID` at CLI-parse time, not by the
    /// GTK side, the same pattern every other acting-agent field uses.
    Whoami {
        agent_id: Option<Uuid>,
    },
    /// `duetctl drawing list`: drawings the requester may read (connected
    /// ones, for an agent) — see `app::drawings`.
    DrawingList {
        #[serde(default)]
        requested_by: Option<Uuid>,
    },
    /// `duetctl drawing read <id>`: renders it to a PNG and returns the path
    /// and its strokes. Needs a connection (`ShareContext`) for an agent.
    DrawingRead {
        #[serde(default)]
        requested_by: Option<Uuid>,
        id: Uuid,
    },
    /// `duetctl notes list`: every note `requested_by` can discover — see
    /// `App::list_notes`.
    NotesList {
        #[serde(default)]
        requested_by: Option<Uuid>,
    },
    /// `duetctl notes read <id>`: fails if `requested_by` lacks `ReadNote`
    /// on an edge to this note.
    NotesRead {
        #[serde(default)]
        requested_by: Option<Uuid>,
        id: Uuid,
    },
    /// `duetctl notes replace <id>`: an explicit whole-content overwrite.
    /// Fails if `requested_by` lacks `WriteNote`.
    NotesReplace {
        #[serde(default)]
        requested_by: Option<Uuid>,
        id: Uuid,
        markdown: String,
    },
    /// `duetctl notes append <id>`: adds `addition` after the note's
    /// current content. Fails if `requested_by` lacks `WriteNote`.
    NotesAppend {
        #[serde(default)]
        requested_by: Option<Uuid>,
        id: Uuid,
        addition: String,
    },
    /// `duetctl notes patch <id> --old ... --new ...`: replaces the one
    /// occurrence of `old` with `new`, failing if `old` isn't found or
    /// isn't unique — see `orchestration::notes::apply_patch`. Fails if
    /// `requested_by` lacks `WriteNote`.
    NotesPatch {
        #[serde(default)]
        requested_by: Option<Uuid>,
        id: Uuid,
        old: String,
        new: String,
    },
    /// `duetctl notes connections <id>`: every edge touching this note —
    /// discovery, not itself capability-gated (see `App::note_connections`).
    NotesConnections {
        id: Uuid,
    },
    /// `duetctl resolve @ref` / `duetctl resolve @kind:ref`: Milestone 5's
    /// universal resource addressing — see `orchestration::resource`.
    /// `requested_by`, if the acting agent is live, narrows an otherwise
    /// ambiguous match to its own workspace/floor (section 7); resolution
    /// itself is never permission-gated (section 8).
    Resolve {
        reference: String,
        #[serde(default)]
        requested_by: Option<Uuid>,
    },
    /// `duetctl resource inspect <id>`: basic, non-gated metadata for any
    /// resource kind the resolver knows about — see
    /// `orchestration::resource::ResourceDetail`. `requested_by` only
    /// shapes what a portal's summary reveals (its URL needs
    /// `ControlPortal`).
    ResourceInspect {
        id: Uuid,
        #[serde(default)]
        requested_by: Option<Uuid>,
    },
    /// `duetctl notes attach <id> <path>`: makes a note file-backed (see
    /// `App::attach_note_file`). Fails without `WriteNote`.
    NotesAttach {
        #[serde(default)]
        requested_by: Option<Uuid>,
        id: Uuid,
        path: String,
    },
    /// `duetctl notes detach <id>`: makes a file-backed note internal again.
    NotesDetach {
        #[serde(default)]
        requested_by: Option<Uuid>,
        id: Uuid,
    },
    /// `duetctl file inspect <path|@file:ref>` — Milestone 6's project
    /// filesystem; see `app::files`. `reference` is anything
    /// `App::parse_file_argument` accepts.
    FileInspect {
        reference: String,
    },
    /// `duetctl file read <path|@file:ref> [--lines A-B]`.
    FileRead {
        reference: String,
        #[serde(default)]
        lines: Option<LineRange>,
    },
    /// `duetctl file list [dir] [--hidden]`.
    FileList {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        hidden: bool,
    },
    /// `duetctl file search <query>` (fuzzy filenames) or
    /// `duetctl file search --content <pattern>`.
    FileSearch {
        query: String,
        #[serde(default)]
        content: bool,
        #[serde(default)]
        fixed: bool,
        #[serde(default)]
        hidden: bool,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// `duetctl file write <path> (--revision <rev> | --create)`, content
    /// on stdin — a conflict-checked write (see `App::write_file`).
    FileWrite {
        reference: String,
        content: String,
        #[serde(default)]
        revision: Option<FileRevision>,
        #[serde(default)]
        create: bool,
    },
    GitStatus,
    GitDiff {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        scope: DiffScope,
    },
    GitLog {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    GitStage {
        paths: Vec<String>,
    },
    GitUnstage {
        paths: Vec<String>,
    },
    /// Destructive: refused unless `confirmed` (`--confirm` on the CLI).
    GitDiscard {
        paths: Vec<String>,
        #[serde(default)]
        confirmed: bool,
    },
    GitCommit {
        message: String,
    },
    /// `duetctl git stage --all` / `unstage --all`.
    GitStageAll,
    GitUnstageAll,
    GitBranches,
    /// Switch to an existing local branch; never forced.
    GitSwitch {
        branch: String,
    },
    /// Create a branch from `HEAD` and switch to it.
    GitCreateBranch {
        branch: String,
    },
    /// Remote operations — answered asynchronously (they run on a worker
    /// thread; see `app::scm`).
    GitFetch,
    GitPull,
    GitPush {
        #[serde(default)]
        set_upstream: bool,
    },
    /// `duetctl portal list` — Milestone 8's browser portals; see
    /// `app::portals`. Every portal request names its portal as `portal`:
    /// an id, `@portal:name`, `@name` or a bare name, resolved app-side by
    /// `App::resolve_portal_argument` (so ambiguity is reported, never
    /// guessed). Every one except `list` needs `ControlPortal` for an agent.
    PortalList {
        #[serde(default)]
        requested_by: Option<Uuid>,
    },
    /// `duetctl portal inspect|url|title <portal>`.
    PortalInspect {
        #[serde(default)]
        requested_by: Option<Uuid>,
        portal: String,
        /// `url`/`title` for `duetctl portal url|title`: the client then
        /// prints just that field. Presentation only — the answer is the
        /// same `PortalDetail` either way.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        show: Option<String>,
    },
    /// `duetctl portal navigate <portal> <url>`; answers once loaded.
    PortalNavigate {
        #[serde(default)]
        requested_by: Option<Uuid>,
        portal: String,
        url: String,
    },
    /// `duetctl portal back|forward|reload <portal>`.
    PortalStep {
        #[serde(default)]
        requested_by: Option<Uuid>,
        portal: String,
        step: PortalStep,
    },
    /// `duetctl portal text <portal> [--selector css] [--html] [--limit N]`.
    PortalText {
        #[serde(default)]
        requested_by: Option<Uuid>,
        portal: String,
        #[serde(default)]
        selector: Option<String>,
        #[serde(default)]
        html: bool,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// `duetctl portal screenshot <portal> [--full]`.
    PortalScreenshot {
        #[serde(default)]
        requested_by: Option<Uuid>,
        portal: String,
        #[serde(default)]
        full_page: bool,
    },
    /// `duetctl portal evaluate <portal>` (script on stdin or as an
    /// argument). Privileged: needs the portal's `allow_scripts` too.
    PortalEvaluate {
        #[serde(default)]
        requested_by: Option<Uuid>,
        portal: String,
        script: String,
    },
    /// `duetctl portal click <portal> <selector>`.
    PortalClick {
        #[serde(default)]
        requested_by: Option<Uuid>,
        portal: String,
        selector: String,
    },
    /// `duetctl portal type <portal> <selector> <text> [--append] [--submit]`.
    PortalType {
        #[serde(default)]
        requested_by: Option<Uuid>,
        portal: String,
        selector: String,
        text: String,
        #[serde(default)]
        append: bool,
        #[serde(default)]
        submit: bool,
    },
}

impl ControlRequest {
    /// Whether this request is answered asynchronously, from a WebKit
    /// callback, rather than by `handle_request` — see `dispatch`.
    fn is_async(&self) -> bool {
        matches!(
            self,
            ControlRequest::PortalNavigate { .. }
                | ControlRequest::PortalStep { .. }
                | ControlRequest::PortalText { .. }
                | ControlRequest::PortalScreenshot { .. }
                | ControlRequest::PortalEvaluate { .. }
                | ControlRequest::PortalClick { .. }
                | ControlRequest::PortalType { .. }
                | ControlRequest::GitFetch
                | ControlRequest::GitPull
                | ControlRequest::GitPush { .. }
        )
    }

    /// How long a client waits for this request's answer. Page operations
    /// may legitimately wait for a page to load (`LOAD_TIMEOUT`), so they
    /// get longer than the snappy default.
    fn reply_timeout(&self) -> Duration {
        if matches!(
            self,
            ControlRequest::GitFetch | ControlRequest::GitPull | ControlRequest::GitPush { .. }
        ) {
            // A remote may be slow; git itself gives up on a dead one.
            Duration::from_secs(180)
        } else if self.is_async() {
            crate::portal_runtime::LOAD_TIMEOUT * 2 + Duration::from_secs(5)
        } else {
            REPLY_TIMEOUT
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ControlResponse {
    Agents {
        agents: Vec<AgentSummary>,
        links: Vec<LinkSummary>,
    },
    Sent {
        message: AgentMessage,
    },
    AgentList {
        agents: Vec<AgentInfo>,
    },
    AgentDetail {
        agent: AgentInfo,
    },
    Connections {
        connections: Vec<ConnectionInfo>,
    },
    Workspace {
        workspace: WorkspaceInfo,
    },
    Created {
        id: Uuid,
    },
    Removed,
    RoleAssigned,
    Whoami {
        info: WhoamiInfo,
    },
    Notes {
        notes: Vec<NoteSummary>,
    },
    Drawings {
        drawings: Vec<crate::message::DrawingSummary>,
    },
    Drawing {
        drawing: crate::message::DrawingExport,
    },
    Note {
        note: NoteDetail,
    },
    NoteUpdated {
        id: Uuid,
    },
    Resolved {
        outcome: ResolveOutcome,
    },
    ResourceInspected {
        resource: ResolvedResource,
        detail: ResourceDetail,
    },
    NoteAttached {
        id: Uuid,
        path: String,
    },
    FileInfo {
        info: FileInfo,
    },
    FileContent {
        file: FileContent,
    },
    FileEntries {
        entries: Vec<DirEntry>,
    },
    FileMatches {
        matches: Vec<NameMatch>,
    },
    ContentMatches {
        search: ContentSearchResult,
    },
    FileWritten {
        path: String,
        revision: FileRevision,
    },
    GitStatus {
        status: GitStatus,
    },
    GitDiff {
        diff: String,
    },
    GitLog {
        entries: Vec<GitLogEntry>,
    },
    GitDone {
        message: String,
    },
    GitBranches {
        branches: Vec<crate::project::git::GitBranch>,
    },
    Portals {
        portals: Vec<PortalSummary>,
    },
    PortalDetail {
        portal: PortalInfo,
    },
    PortalAction {
        action: PortalAction,
    },
    PortalPage {
        page: PortalPage,
    },
    PortalScreenshot {
        screenshot: PortalScreenshot,
    },
    PortalEvaluated {
        id: Uuid,
        value: serde_json::Value,
    },
    Error {
        error: String,
    },
}

/// One accepted request, paired with where to send its answer — the unit
/// `main.rs`'s poll loop drains from the receiving end of the channel
/// `spawn_server` was given.
pub struct ControlEvent {
    pub request: ControlRequest,
    pub respond: Sender<ControlResponse>,
}

/// How long a connection waits for the main loop to answer before giving up
/// and reporting an error — keeps a stuck/overloaded main loop from hanging
/// a client forever rather than ever actually happening in practice.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Binds the control socket and spawns the background thread that accepts
/// connections on it, each handled synchronously (control traffic is
/// human/agent-rate, not a hot path, so a connection-per-thread pool would
/// be solving a problem this never has). A stale socket file left by a
/// crashed run is removed before retrying the bind. A reachable socket is
/// preserved so a second instance cannot disconnect the instance already
/// serving requests.
pub fn spawn_server(tx: Sender<ControlEvent>) -> std::io::Result<()> {
    let path =
        default_control_socket_path().map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(&path).is_ok() {
                return Err(error);
            }
            std::fs::remove_file(&path)?;
            UnixListener::bind(&path)?
        }
        Err(error) => return Err(error),
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            handle_connection(stream, &tx);
        }
    });
    Ok(())
}

/// Reads exactly one request line, waits for the main loop's answer, writes
/// exactly one response line, and closes — the whole protocol.
fn handle_connection(stream: UnixStream, tx: &Sender<ControlEvent>) {
    let Ok(mut reader) = stream.try_clone().map(BufReader::new) else {
        return;
    };
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }

    let response = match serde_json::from_str::<ControlRequest>(line.trim_end()) {
        Ok(request) => {
            let (respond, answer) = channel();
            let timeout = request.reply_timeout();
            match tx.send(ControlEvent { request, respond }) {
                Ok(()) => answer
                    .recv_timeout(timeout)
                    .unwrap_or(ControlResponse::Error {
                        error: "duet did not respond in time".to_string(),
                    }),
                Err(_) => ControlResponse::Error {
                    error: "duet is not responding".to_string(),
                },
            }
        }
        Err(error) => ControlResponse::Error {
            error: format!("bad request: {error}"),
        },
    };

    let mut out = serde_json::to_string(&response).unwrap_or_else(|_| {
        r#"{"result":"error","error":"internal: couldn't encode response"}"#.to_string()
    });
    out.push('\n');
    let _ = (&stream).write_all(out.as_bytes());
}

/// A provider keyword as `duetctl agents create --agent <...>` accepts it.
/// Case-insensitive; `Agent::Custom` isn't reachable from the CLI (see
/// `ControlRequest::AgentsCreate`'s doc comment).
fn parse_provider(name: &str) -> Result<Agent, String> {
    match name.to_ascii_lowercase().as_str() {
        "claude" => Ok(Agent::Claude),
        "codex" => Ok(Agent::Codex),
        "opencode" => Ok(Agent::OpenCode),
        "shell" => Ok(Agent::Shell),
        other => Err(format!(
            "unknown provider '{other}' (expected claude, codex, opencode, or shell)"
        )),
    }
}

/// Resolves a role name to its id against the live role roster (built-in
/// plus custom) — the same name-matching style `agent::find_identity`
/// already uses for agents. `"none"` is handled by the caller before this
/// (it means "clear the role", not "a role literally named none").
fn resolve_role_id(app: &App, name: &str) -> Result<Uuid, String> {
    app.roles()
        .into_iter()
        .find(|role| role.name == name)
        .map(|role| role.id)
        .ok_or_else(|| format!("no role named '{name}'"))
}

/// Resolves an optional role flag's value: `None` leaves the role
/// unspecified (used by `AgentsCreate`, where no `--role` just means no
/// role), `Some("none")` explicitly clears an existing role (used by
/// `AgentsAssignRole`), anything else resolves by name.
fn resolve_optional_role(app: &App, role: Option<String>) -> Result<Option<Uuid>, String> {
    match role.as_deref() {
        None | Some("none") => Ok(None),
        Some(name) => resolve_role_id(app, name).map(Some),
    }
}

/// Answers one accepted request — called from `main.rs`'s poll loop, on
/// the GTK main thread. Most requests are answered right away by
/// `handle_request`; a portal operation that needs the page (navigation,
/// text, screenshots, interaction) is asynchronous in WebKit, so its
/// one-shot reply channel travels into the `PortalService` callback and is
/// answered from there — the poll loop never blocks on a page.
pub fn dispatch(app: &Rc<RefCell<App>>, event: ControlEvent) {
    let ControlEvent { request, respond } = event;
    if !request.is_async() {
        let _ = respond.send(handle_request(app, request));
        return;
    }
    let answer = move |response: ControlResponse| {
        let _ = respond.send(response);
    };
    let resolve = |portal: &str, requested_by: Option<Uuid>| {
        app.borrow().resolve_portal_argument(portal, requested_by)
    };
    let on_action = |result: Result<PortalAction, String>| match result {
        Ok(action) => ControlResponse::PortalAction { action },
        Err(error) => ControlResponse::Error { error },
    };
    macro_rules! resolved {
        ($portal:expr, $requested_by:expr) => {
            match resolve(&$portal, $requested_by) {
                Ok(id) => id,
                Err(error) => return answer(ControlResponse::Error { error }),
            }
        };
    }
    match request {
        ControlRequest::GitFetch | ControlRequest::GitPull | ControlRequest::GitPush { .. } => {
            let op = match request {
                ControlRequest::GitFetch => crate::app::scm::RemoteOp::Fetch,
                ControlRequest::GitPull => crate::app::scm::RemoteOp::Pull,
                ControlRequest::GitPush { set_upstream } => {
                    crate::app::scm::RemoteOp::Push { set_upstream }
                }
                _ => unreachable!("matched above"),
            };
            App::git_remote(
                app,
                op,
                Box::new(move |outcome| {
                    answer(match outcome {
                        Ok(outcome) => ControlResponse::GitDone {
                            message: outcome.message,
                        },
                        Err(crate::project::git::GitError::UpstreamRequired(target)) => {
                            ControlResponse::Error {
                                error: format!(
                                    "{} has no upstream yet; run `duetctl git push --set-upstream` to publish it to {}/{}",
                                    target.branch, target.remote, target.branch
                                ),
                            }
                        }
                        Err(error) => ControlResponse::Error {
                            error: error.to_string(),
                        },
                    })
                }),
            );
        }
        ControlRequest::PortalNavigate {
            requested_by,
            portal,
            url,
        } => {
            let id = resolved!(portal, requested_by);
            App::portal_navigate(app, requested_by, id, &url, move |result| {
                answer(on_action(result))
            });
        }
        ControlRequest::PortalStep {
            requested_by,
            portal,
            step,
        } => {
            let id = resolved!(portal, requested_by);
            App::portal_step(app, requested_by, id, step, move |result| {
                answer(on_action(result))
            });
        }
        ControlRequest::PortalText {
            requested_by,
            portal,
            selector,
            html,
            limit,
        } => {
            let id = resolved!(portal, requested_by);
            App::portal_text(
                app,
                requested_by,
                id,
                selector,
                html,
                limit,
                move |result| {
                    answer(match result {
                        Ok(page) => ControlResponse::PortalPage { page },
                        Err(error) => ControlResponse::Error { error },
                    })
                },
            );
        }
        ControlRequest::PortalScreenshot {
            requested_by,
            portal,
            full_page,
        } => {
            let id = resolved!(portal, requested_by);
            App::portal_screenshot(app, requested_by, id, full_page, move |result| {
                answer(match result {
                    Ok(screenshot) => ControlResponse::PortalScreenshot { screenshot },
                    Err(error) => ControlResponse::Error { error },
                })
            });
        }
        ControlRequest::PortalEvaluate {
            requested_by,
            portal,
            script,
        } => {
            let id = resolved!(portal, requested_by);
            App::portal_evaluate(app, requested_by, id, &script, move |result| {
                answer(match result {
                    Ok(value) => ControlResponse::PortalEvaluated { id, value },
                    Err(error) => ControlResponse::Error { error },
                })
            });
        }
        ControlRequest::PortalClick {
            requested_by,
            portal,
            selector,
        } => {
            let id = resolved!(portal, requested_by);
            App::portal_click(app, requested_by, id, &selector, move |result| {
                answer(on_action(result))
            });
        }
        ControlRequest::PortalType {
            requested_by,
            portal,
            selector,
            text,
            append,
            submit,
        } => {
            let id = resolved!(portal, requested_by);
            App::portal_type(
                app,
                requested_by,
                id,
                &selector,
                &text,
                append,
                submit,
                move |result| answer(on_action(result)),
            );
        }
        request => answer(handle_request(app, request)),
    }
}

/// Answers one synchronous request against the live `App` — see
/// `dispatch`, which routes every request here except the asynchronous
/// portal operations and Git remote operations.
pub fn handle_request(app: &Rc<RefCell<App>>, request: ControlRequest) -> ControlResponse {
    match request {
        ControlRequest::List => {
            let app_ref = app.borrow();
            ControlResponse::Agents {
                agents: app_ref.agent_summaries(),
                links: app_ref.link_summaries(),
            }
        }
        ControlRequest::Send {
            source_session_id,
            target,
            content,
        } => match App::send_message(app, source_session_id, &target, content) {
            Ok(message) => ControlResponse::Sent { message },
            Err(error) => ControlResponse::Error { error },
        },
        ControlRequest::SendMessage { from, to, content } => {
            match App::send_message(app, from, &to, content) {
                Ok(message) => ControlResponse::Sent { message },
                Err(error) => ControlResponse::Error { error },
            }
        }
        ControlRequest::AgentsList => ControlResponse::AgentList {
            agents: app.borrow().agent_infos(),
        },
        ControlRequest::AgentsInspect { target } => {
            let app_ref = app.borrow();
            let Some(identity) = app_ref.agent_registry().resolve(&target).cloned() else {
                return ControlResponse::Error {
                    error: format!("no agent named '{target}'"),
                };
            };
            match app_ref
                .agent_infos()
                .into_iter()
                .find(|info| info.id == identity.id)
            {
                Some(agent) => ControlResponse::AgentDetail { agent },
                None => ControlResponse::Error {
                    error: format!("no agent named '{target}'"),
                },
            }
        }
        ControlRequest::ConnectionsList => ControlResponse::Connections {
            connections: app.borrow().connection_infos(),
        },
        ControlRequest::WorkspaceInspect => ControlResponse::Workspace {
            workspace: app.borrow().workspace_info(),
        },
        ControlRequest::AgentsCreate {
            name,
            cwd,
            agent,
            role,
            requested_by,
        } => {
            let provider = match parse_provider(&agent) {
                Ok(provider) => provider,
                Err(error) => return ControlResponse::Error { error },
            };
            let role_id = match resolve_optional_role(&app.borrow(), role) {
                Ok(role_id) => role_id,
                Err(error) => return ControlResponse::Error { error },
            };
            // A CLI creation has no cursor position to spawn at (unlike the
            // GUI's "New session" dialog, which uses the viewport center) —
            // cascade by however many nodes already exist so agents created
            // back-to-back (as the acceptance scenario's setup does) don't
            // all land exactly on top of each other.
            let position = {
                let count = app.borrow().nodes.len() as f64;
                (120.0 + count * 40.0, 120.0 + count * 40.0)
            };
            match App::create_agent(app, requested_by, name, cwd, provider, role_id, position) {
                Ok(id) => ControlResponse::Created { id },
                Err(error) => ControlResponse::Error {
                    error: error.to_string(),
                },
            }
        }
        ControlRequest::AgentsRemove {
            target,
            requested_by,
        } => {
            let Some(id) = app.borrow().agent_registry().resolve(&target).map(|i| i.id) else {
                return ControlResponse::Error {
                    error: format!("no agent named '{target}'"),
                };
            };
            match App::remove_agent(app, requested_by, id) {
                Ok(()) => ControlResponse::Removed,
                Err(error) => ControlResponse::Error {
                    error: error.to_string(),
                },
            }
        }
        ControlRequest::AgentsAssignRole {
            target,
            role,
            requested_by,
        } => {
            let Some(id) = app.borrow().agent_registry().resolve(&target).map(|i| i.id) else {
                return ControlResponse::Error {
                    error: format!("no agent named '{target}'"),
                };
            };
            let role_id = match resolve_optional_role(&app.borrow(), role) {
                Ok(role_id) => role_id,
                Err(error) => return ControlResponse::Error { error },
            };
            match App::assign_role(app, requested_by, id, role_id) {
                Ok(()) => ControlResponse::RoleAssigned,
                Err(error) => ControlResponse::Error {
                    error: error.to_string(),
                },
            }
        }
        ControlRequest::Whoami { agent_id } => {
            let Some(agent_id) = agent_id else {
                return ControlResponse::Error {
                    error: "whoami needs DUET_AGENT_ID in the environment — run this from \
                            inside a duet-launched agent"
                        .to_string(),
                };
            };
            let app_ref = app.borrow();
            let registry = app_ref.agent_registry();
            let Some(identity) = registry.get_agent(agent_id).cloned() else {
                return ControlResponse::Error {
                    error: format!("no agent with id {agent_id}"),
                };
            };
            let infos = app_ref.agent_infos();
            let Some(agent) = infos.iter().find(|info| info.id == agent_id).cloned() else {
                return ControlResponse::Error {
                    error: format!("no agent with id {agent_id}"),
                };
            };
            let connected_agents = registry
                .connected_agents(agent_id)
                .into_iter()
                .filter_map(|other| infos.iter().find(|info| info.id == other.id).cloned())
                .collect();
            ControlResponse::Whoami {
                info: WhoamiInfo {
                    agent,
                    role_instructions: identity.role.map(|role| role.instructions),
                    connected_agents,
                    connected_files: app_ref.connected_files(agent_id),
                    connected_portals: app_ref.list_portals(Some(agent_id)),
                    connected_drawings: app_ref.list_drawings(Some(agent_id)),
                },
            }
        }
        ControlRequest::DrawingList { requested_by } => ControlResponse::Drawings {
            drawings: app.borrow().list_drawings(requested_by),
        },
        ControlRequest::DrawingRead { requested_by, id } => {
            respond(app.borrow().read_drawing(requested_by, id), |drawing| {
                ControlResponse::Drawing { drawing }
            })
        }
        ControlRequest::NotesList { requested_by } => ControlResponse::Notes {
            notes: app.borrow().list_notes(requested_by),
        },
        ControlRequest::NotesRead { requested_by, id } => {
            match app.borrow().read_note(requested_by, id) {
                Ok(note) => ControlResponse::Note { note },
                Err(error) => ControlResponse::Error { error },
            }
        }
        ControlRequest::NotesReplace {
            requested_by,
            id,
            markdown,
        } => match App::replace_note(app, requested_by, id, markdown) {
            Ok(()) => ControlResponse::NoteUpdated { id },
            Err(error) => ControlResponse::Error {
                error: error.to_string(),
            },
        },
        ControlRequest::NotesAppend {
            requested_by,
            id,
            addition,
        } => match App::append_note(app, requested_by, id, addition) {
            Ok(()) => ControlResponse::NoteUpdated { id },
            Err(error) => ControlResponse::Error {
                error: error.to_string(),
            },
        },
        ControlRequest::NotesPatch {
            requested_by,
            id,
            old,
            new,
        } => match App::patch_note(app, requested_by, id, old, new) {
            Ok(()) => ControlResponse::NoteUpdated { id },
            Err(error) => ControlResponse::Error {
                error: error.to_string(),
            },
        },
        ControlRequest::NotesConnections { id } => match app.borrow().note_connections(id) {
            Ok(connections) => ControlResponse::Connections { connections },
            Err(error) => ControlResponse::Error { error },
        },
        ControlRequest::Resolve {
            reference,
            requested_by,
        } => match app.borrow().resolve_resource(&reference, requested_by) {
            Ok(outcome) => ControlResponse::Resolved { outcome },
            Err(error) => ControlResponse::Error { error },
        },
        ControlRequest::ResourceInspect { id, requested_by } => {
            match app.borrow().inspect_resource(id, requested_by) {
                Ok((resource, detail)) => ControlResponse::ResourceInspected { resource, detail },
                Err(error) => ControlResponse::Error { error },
            }
        }
        ControlRequest::NotesAttach {
            requested_by,
            id,
            path,
        } => match App::attach_note_file(app, requested_by, id, &path) {
            Ok(path) => ControlResponse::NoteAttached { id, path },
            Err(error) => ControlResponse::Error { error },
        },
        ControlRequest::NotesDetach { requested_by, id } => {
            match App::detach_note_file(app, requested_by, id) {
                Ok(()) => ControlResponse::NoteUpdated { id },
                Err(error) => ControlResponse::Error { error },
            }
        }
        ControlRequest::FileInspect { reference } => {
            respond(app.borrow().inspect_file(&reference), |info| {
                ControlResponse::FileInfo { info }
            })
        }
        ControlRequest::FileRead { reference, lines } => {
            respond(app.borrow().read_file(&reference, lines), |file| {
                ControlResponse::FileContent { file }
            })
        }
        ControlRequest::FileList { path, hidden } => respond(
            app.borrow().list_directory(path.as_deref(), hidden),
            |entries| ControlResponse::FileEntries { entries },
        ),
        ControlRequest::FileSearch {
            query,
            content,
            fixed,
            hidden,
            limit,
        } => {
            if content {
                respond(
                    app.borrow()
                        .search_files(&query, fixed, limit.unwrap_or(200), hidden),
                    |search| ControlResponse::ContentMatches { search },
                )
            } else {
                respond(
                    app.borrow().find_files(&query, limit.unwrap_or(30), hidden),
                    |matches| ControlResponse::FileMatches { matches },
                )
            }
        }
        ControlRequest::FileWrite {
            reference,
            content,
            revision,
            create,
        } => respond(
            App::write_file(app, &reference, revision, create, &content),
            |revision| ControlResponse::FileWritten {
                path: reference.clone(),
                revision,
            },
        ),
        ControlRequest::GitStatus => respond(app.borrow().git_status(), |status| {
            ControlResponse::GitStatus { status }
        }),
        ControlRequest::GitDiff { path, scope } => {
            respond(app.borrow().git_diff(path.as_deref(), scope), |diff| {
                ControlResponse::GitDiff { diff }
            })
        }
        ControlRequest::GitLog { path, limit } => respond(
            app.borrow().git_log(path.as_deref(), limit.unwrap_or(20)),
            |entries| ControlResponse::GitLog { entries },
        ),
        ControlRequest::GitStage { paths } => {
            respond(App::git_stage(app, &paths), |()| ControlResponse::GitDone {
                message: format!("staged {}", paths.join(", ")),
            })
        }
        ControlRequest::GitUnstage { paths } => respond(App::git_unstage(app, &paths), |()| {
            ControlResponse::GitDone {
                message: format!("unstaged {}", paths.join(", ")),
            }
        }),
        ControlRequest::GitDiscard { paths, confirmed } => {
            respond(App::git_discard(app, &paths, confirmed), |()| {
                ControlResponse::GitDone {
                    message: format!("discarded changes to {}", paths.join(", ")),
                }
            })
        }
        ControlRequest::GitStageAll => {
            respond(App::git_stage_all(app), |()| ControlResponse::GitDone {
                message: "staged all changes".to_string(),
            })
        }
        ControlRequest::GitUnstageAll => {
            respond(App::git_unstage_all(app), |()| ControlResponse::GitDone {
                message: "unstaged everything".to_string(),
            })
        }
        ControlRequest::GitBranches => respond(app.borrow().git_branches(), |branches| {
            ControlResponse::GitBranches { branches }
        }),
        ControlRequest::GitSwitch { branch } => {
            respond(App::git_switch_branch(app, &branch), |()| {
                ControlResponse::GitDone {
                    message: format!("switched to {branch}"),
                }
            })
        }
        ControlRequest::GitCreateBranch { branch } => {
            respond(App::git_create_branch(app, &branch), |()| {
                ControlResponse::GitDone {
                    message: format!("created and switched to {branch}"),
                }
            })
        }
        ControlRequest::GitCommit { message } => respond(App::git_commit(app, &message), |hash| {
            ControlResponse::GitDone {
                message: format!("committed {hash}"),
            }
        }),
        ControlRequest::PortalList { requested_by } => ControlResponse::Portals {
            portals: app.borrow().list_portals(requested_by),
        },
        ControlRequest::PortalInspect {
            requested_by,
            portal,
            ..
        } => {
            let app_ref = app.borrow();
            respond(
                app_ref
                    .resolve_portal_argument(&portal, requested_by)
                    .and_then(|id| app_ref.inspect_portal(requested_by, id)),
                |portal| ControlResponse::PortalDetail { portal },
            )
        }
        // Asynchronous: answered by `dispatch`, never here. Reaching this
        // arm means a caller bypassed `dispatch`.
        ControlRequest::PortalNavigate { .. }
        | ControlRequest::PortalStep { .. }
        | ControlRequest::PortalText { .. }
        | ControlRequest::PortalScreenshot { .. }
        | ControlRequest::PortalEvaluate { .. }
        | ControlRequest::PortalClick { .. }
        | ControlRequest::PortalType { .. }
        | ControlRequest::GitFetch
        | ControlRequest::GitPull
        | ControlRequest::GitPush { .. } => ControlResponse::Error {
            error: "internal: asynchronous operations must go through control::dispatch"
                .to_string(),
        },
    }
}

/// `Ok(value)` → `ok(value)`, `Err(error)` → `ControlResponse::Error`.
fn respond<T>(result: Result<T, String>, ok: impl FnOnce(T) -> ControlResponse) -> ControlResponse {
    match result {
        Ok(value) => ok(value),
        Err(error) => ControlResponse::Error { error },
    }
}

/// `true` for every first argument `duet`'s pre-GTK dispatch (and
/// `duetctl`'s own `main`) recognizes as a control-CLI invocation rather
/// than "launch the GUI".
pub fn is_cli_verb(verb: &str) -> bool {
    matches!(
        verb,
        "agent"
            | "agents"
            | "send"
            | "connections"
            | "workspace"
            | "whoami"
            | "notes"
            | "resolve"
            | "resource"
            | "file"
            | "git"
            | "portal"
            | "portals"
            | "drawing"
            | "drawings"
    )
}

/// `duetctl <verb> ...` / `duet <verb> ...`: connects to the running duet
/// instance's control socket, sends one request, prints its answer, and
/// returns whether it succeeded. Works from inside a headless agent shell —
/// no display needed.
pub fn run_cli(args: &[String]) -> bool {
    // `--json` is a trailing output-format flag, applicable to any command
    // (not just `resolve`/`resource inspect`, though those are why it
    // exists — section 4's "also provide JSON/machine-readable output").
    // Stripped here, before verb-specific parsing, so it's never mistaken
    // for part of a reference or message body.
    let (json, args) = match args.split_last() {
        Some((last, rest)) if last == "--json" => (true, rest),
        _ => (false, args),
    };
    let request = match parse_cli_request(args) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("duetctl: {error}");
            return false;
        }
    };
    match send_and_read(&request) {
        Ok(response) => {
            let ok = !matches!(response, ControlResponse::Error { .. });
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&response).unwrap_or_else(|_| "{}".to_string())
                );
            } else if let (
                ControlRequest::PortalInspect {
                    show: Some(field), ..
                },
                ControlResponse::PortalDetail { portal },
            ) = (&request, &response)
            {
                match field.as_str() {
                    "url" => println!("{}", portal.url),
                    _ => println!("{}", portal.title.as_deref().unwrap_or("")),
                }
            } else {
                print_response(&response);
            }
            ok
        }
        Err(error) => {
            eprintln!("duetctl: {error}");
            false
        }
    }
}

fn send_and_read(request: &ControlRequest) -> anyhow::Result<ControlResponse> {
    let path = default_control_socket_path()?;
    let stream = UnixStream::connect(&path).map_err(|error| {
        anyhow::anyhow!(
            "couldn't reach duet at {} ({error}); is duet running?",
            path.display()
        )
    })?;
    let mut payload = serde_json::to_string(request)?;
    payload.push('\n');
    (&stream).write_all(payload.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    Ok(serde_json::from_str(reply.trim_end())?)
}

/// The acting agent's id for a request issued from inside its own launched
/// shell: `DUET_AGENT_ID` first (section 8's canonical variable), falling
/// back to the older `DUET_SESSION_ID` a pre-Milestone-3 launch still set.
/// `None` from a bare interactive shell — the human operator, trusted the
/// same way the GTK UI already is.
fn acting_agent_id() -> Option<Uuid> {
    std::env::var("DUET_AGENT_ID")
        .or_else(|_| std::env::var("DUET_SESSION_ID"))
        .ok()
        .and_then(|value| Uuid::parse_str(&value).ok())
}

fn parse_cli_request(args: &[String]) -> anyhow::Result<ControlRequest> {
    match args {
        [cmd, rest @ ..] if cmd == "agent" => parse_legacy_agent(rest),
        [cmd, rest @ ..] if cmd == "agents" => parse_agents(rest),
        [cmd, rest @ ..] if cmd == "send" => parse_send(rest),
        [cmd, only] if cmd == "connections" && only == "list" => {
            Ok(ControlRequest::ConnectionsList)
        }
        [cmd, only] if cmd == "workspace" && only == "inspect" => {
            Ok(ControlRequest::WorkspaceInspect)
        }
        [only] if only == "whoami" => Ok(ControlRequest::Whoami {
            agent_id: acting_agent_id(),
        }),
        [cmd, rest @ ..] if cmd == "notes" => parse_notes(rest),
        [cmd, rest @ ..] if cmd == "drawing" || cmd == "drawings" => parse_drawing(rest),
        [cmd, reference] if cmd == "resolve" => Ok(ControlRequest::Resolve {
            reference: reference.clone(),
            requested_by: acting_agent_id(),
        }),
        [cmd, rest @ ..] if cmd == "resource" => parse_resource(rest),
        [cmd, rest @ ..] if cmd == "file" => parse_file(rest),
        [cmd, rest @ ..] if cmd == "git" => parse_git(rest),
        [cmd, rest @ ..] if cmd == "portal" || cmd == "portals" => parse_portal(rest),
        _ => anyhow::bail!(USAGE),
    }
}

const USAGE: &str = r#"usage:
  duetctl agents list
  duetctl agents inspect <id-or-name>
  duetctl agents create --name <name> --cwd <dir> --agent <claude|codex|opencode|shell> [--role <role>]
  duetctl agents remove <id-or-name>
  duetctl agents assign-role <id-or-name> <role-or-none>
  duetctl send --from <id> --to <id-or-name> "<message>"
  duetctl connections list
  duetctl workspace inspect
  duetctl whoami
  duetctl notes list
  duetctl notes read <id>
  duetctl notes replace <id>            (new Markdown content read from stdin)
  duetctl notes append <id>             (text to append read from stdin)
  duetctl notes patch <id> --old <text> --new <text>
  duetctl notes connections <id>
  duetctl notes attach <id> <path>      (sync the note with a project file)
  duetctl notes detach <id>
  duetctl resolve @name                 (or @kind:name, e.g. @agent:backend, @file:src/a.rs#L1-9)
  duetctl resource inspect <id>
  duetctl file inspect <path|@file:path>
  duetctl file read <path|@file:path[#L1-20]> [--lines 1-20]
  duetctl file list [dir] [--hidden]
  duetctl file search <query> [--hidden] [--limit N]            (fuzzy file names)
  duetctl file search --content <pattern> [--fixed] [--hidden] [--limit N]
  duetctl file write <path> (--revision <rev> | --create)       (content read from stdin)
  duetctl git status
  duetctl git diff [path|@diff:path] [--staged | --unstaged]   (default: all uncommitted)
  duetctl git log [path] [-n N]
  duetctl git stage <path>... | --all
  duetctl git unstage <path>... | --all
  duetctl git discard <path>... --confirm
  duetctl git commit -m "<message>"
  duetctl git branches
  duetctl git branch <new-branch>        (create from HEAD and switch to it)
  duetctl git switch <branch>            (existing local branch; never forced)
  duetctl git fetch
  duetctl git pull                       (fast-forward only)
  duetctl git push [--set-upstream]      (never forced)
  duetctl portal list
  duetctl portal inspect|url|title <portal>      (<portal>: id, @portal:name, @name or name)
  duetctl portal navigate <portal> <url>
  duetctl portal back|forward|reload <portal>
  duetctl portal text <portal> [--selector <css>] [--html] [--limit N]
  duetctl portal screenshot <portal> [--full]
  duetctl portal click <portal> <selector>
  duetctl portal type <portal> <selector> <text> [--append] [--submit]
  duetctl portal evaluate <portal> [<script>]    (script read from stdin if omitted)
  append --json to any command for machine-readable output"#;

const RESOURCE_USAGE: &str = r#"usage:
  duetctl resource inspect <id>"#;

fn parse_resource(args: &[String]) -> anyhow::Result<ControlRequest> {
    match args {
        [cmd, id] if cmd == "inspect" => Ok(ControlRequest::ResourceInspect {
            requested_by: acting_agent_id(),
            id: parse_uuid_arg(id)?,
        }),
        _ => anyhow::bail!(RESOURCE_USAGE),
    }
}

/// Reads standard input fully as UTF-8 — how `notes replace`/`notes append`
/// take their (often multi-line) Markdown content, instead of joining argv
/// words with spaces the way legacy `agent send` does: that would silently
/// collapse newlines and indentation, destroying the very Markdown structure
/// this content is supposed to carry.
fn read_stdin_to_string() -> anyhow::Result<String> {
    let mut content = String::new();
    std::io::stdin()
        .read_to_string(&mut content)
        .map_err(|error| anyhow::anyhow!("couldn't read content from stdin: {error}"))?;
    Ok(content)
}

const NOTES_USAGE: &str = r#"usage:
  duetctl notes list
  duetctl notes read <id>
  duetctl notes replace <id>            (new Markdown content read from stdin)
  duetctl notes append <id>             (text to append read from stdin)
  duetctl notes patch <id> --old <text> --new <text>
  duetctl notes connections <id>
  duetctl notes attach <id> <path>
  duetctl notes detach <id>"#;

const FILE_USAGE: &str = r#"usage:
  duetctl file inspect <path|@file:path>
  duetctl file read <path|@file:path[#L1-20]> [--lines 1-20]
  duetctl file list [dir] [--hidden]
  duetctl file search <query> [--hidden] [--limit N]
  duetctl file search --content <pattern> [--fixed] [--hidden] [--limit N]
  duetctl file write <path> (--revision <rev> | --create)   (content read from stdin)"#;

const GIT_USAGE: &str = r#"usage:
  duetctl git status
  duetctl git diff [path|@diff:path] [--staged | --unstaged]
  duetctl git log [path] [-n N]
  duetctl git stage <path>... | --all
  duetctl git unstage <path>... | --all
  duetctl git discard <path>... --confirm
  duetctl git commit -m "<message>"
  duetctl git branches
  duetctl git branch <new-branch>        (create from HEAD and switch to it)
  duetctl git switch <branch>            (existing local branch; never forced)
  duetctl git fetch
  duetctl git pull                       (fast-forward only)
  duetctl git push [--set-upstream]      (never forced)"#;

const PORTAL_USAGE: &str = r#"usage:
  duetctl portal list
  duetctl portal inspect <portal>
  duetctl portal url <portal>
  duetctl portal title <portal>
  duetctl portal navigate <portal> <url>
  duetctl portal back|forward|reload <portal>
  duetctl portal text <portal> [--selector <css>] [--html] [--limit N]
  duetctl portal screenshot <portal> [--full]
  duetctl portal click <portal> <selector>
  duetctl portal type <portal> <selector> <text> [--append] [--submit]
  duetctl portal evaluate <portal> [<script>]    (script read from stdin if omitted)
<portal> is a portal id, @portal:name, @name, or a bare portal name"#;

/// `duetctl portal ...`. `url`/`title` are `inspect` with a narrower
/// printout (`PortalInspect::show`), so they ride the same request.
fn parse_portal(args: &[String]) -> anyhow::Result<ControlRequest> {
    let requested_by = acting_agent_id();
    let Some((verb, rest)) = args.split_first() else {
        anyhow::bail!(PORTAL_USAGE);
    };
    let (positional, flags) = split_flags(rest, &["--selector", "--limit"], PORTAL_USAGE)?;
    let has = |flag: &str| flags.iter().any(|(name, _)| *name == flag);
    let value = |flag: &str| {
        flags
            .iter()
            .find(|(name, _)| *name == flag)
            .and_then(|(_, value)| *value)
    };
    let known_flags: &[&str] = match verb.as_str() {
        "text" => &["--selector", "--html", "--limit"],
        "screenshot" => &["--full"],
        "type" => &["--append", "--submit"],
        _ => &[],
    };
    if let Some((unknown, _)) = flags.iter().find(|(name, _)| !known_flags.contains(name)) {
        anyhow::bail!("unknown flag {unknown} for `portal {verb}`\n\n{PORTAL_USAGE}");
    }
    let portal = |index: usize| -> anyhow::Result<String> {
        positional
            .get(index)
            .map(|portal| portal.to_string())
            .ok_or_else(|| anyhow::anyhow!("`portal {verb}` needs a portal\n\n{PORTAL_USAGE}"))
    };
    let only = |count: usize| -> anyhow::Result<()> {
        if positional.len() > count {
            anyhow::bail!(
                "too many arguments for `portal {verb}` (quote selectors and text)\n\n{PORTAL_USAGE}"
            );
        }
        Ok(())
    };
    Ok(match verb.as_str() {
        "list" => {
            only(0)?;
            ControlRequest::PortalList { requested_by }
        }
        "inspect" | "url" | "title" => {
            only(1)?;
            ControlRequest::PortalInspect {
                requested_by,
                portal: portal(0)?,
                show: (verb != "inspect").then(|| verb.clone()),
            }
        }
        "navigate" | "open" => {
            only(2)?;
            ControlRequest::PortalNavigate {
                requested_by,
                portal: portal(0)?,
                url: positional
                    .get(1)
                    .map(|url| url.to_string())
                    .ok_or_else(|| anyhow::anyhow!("`portal navigate` needs a URL"))?,
            }
        }
        "back" | "forward" | "reload" => {
            only(1)?;
            ControlRequest::PortalStep {
                requested_by,
                portal: portal(0)?,
                step: match verb.as_str() {
                    "back" => PortalStep::Back,
                    "forward" => PortalStep::Forward,
                    _ => PortalStep::Reload,
                },
            }
        }
        "text" => {
            only(1)?;
            ControlRequest::PortalText {
                requested_by,
                portal: portal(0)?,
                selector: value("--selector").map(str::to_string),
                html: has("--html"),
                limit: parse_limit(value("--limit"))?,
            }
        }
        "screenshot" => {
            only(1)?;
            ControlRequest::PortalScreenshot {
                requested_by,
                portal: portal(0)?,
                full_page: has("--full"),
            }
        }
        "click" => {
            only(2)?;
            ControlRequest::PortalClick {
                requested_by,
                portal: portal(0)?,
                selector: positional
                    .get(1)
                    .map(|selector| selector.to_string())
                    .ok_or_else(|| anyhow::anyhow!("`portal click` needs a CSS selector"))?,
            }
        }
        "type" => {
            only(3)?;
            ControlRequest::PortalType {
                requested_by,
                portal: portal(0)?,
                selector: positional
                    .get(1)
                    .map(|selector| selector.to_string())
                    .ok_or_else(|| anyhow::anyhow!("`portal type` needs a CSS selector"))?,
                text: positional
                    .get(2)
                    .map(|text| text.to_string())
                    .ok_or_else(|| anyhow::anyhow!("`portal type` needs the text to type"))?,
                append: has("--append"),
                submit: has("--submit"),
            }
        }
        "evaluate" | "eval" => {
            only(2)?;
            let script = match positional.get(1) {
                Some(script) => script.to_string(),
                None => read_stdin_to_string()?,
            };
            if script.trim().is_empty() {
                anyhow::bail!("`portal evaluate` needs a script (as an argument or on stdin)");
            }
            ControlRequest::PortalEvaluate {
                requested_by,
                portal: portal(0)?,
                script,
            }
        }
        _ => anyhow::bail!(PORTAL_USAGE),
    })
}

/// `split_flags`' result: positional arguments, then `(flag, value)` pairs.
type SplitArgs<'a> = (Vec<&'a str>, Vec<(&'a str, Option<&'a str>)>);

/// Splits `args` into positional arguments and `--flag [value]` options;
/// `with_value` names the flags that take a value.
fn split_flags<'a>(
    args: &'a [String],
    with_value: &[&str],
    usage: &str,
) -> anyhow::Result<SplitArgs<'a>> {
    let mut positional = Vec::new();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg.starts_with('-') && arg.len() > 1 {
            if with_value.contains(&arg) {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("{arg} needs a value\n\n{usage}"))?;
                flags.push((arg, Some(value.as_str())));
                i += 2;
            } else {
                flags.push((arg, None));
                i += 1;
            }
        } else {
            positional.push(arg);
            i += 1;
        }
    }
    Ok((positional, flags))
}

fn parse_limit(value: Option<&str>) -> anyhow::Result<Option<usize>> {
    value
        .map(|raw| {
            raw.parse::<usize>()
                .map_err(|_| anyhow::anyhow!("'{raw}' is not a number"))
        })
        .transpose()
}

fn parse_file(args: &[String]) -> anyhow::Result<ControlRequest> {
    let Some((cmd, rest)) = args.split_first() else {
        anyhow::bail!(FILE_USAGE);
    };
    let (positional, flags) = split_flags(rest, &["--lines", "--limit", "--revision"], FILE_USAGE)?;
    let flag = |name: &str| flags.iter().any(|(flag, _)| *flag == name);
    let value = |name: &str| {
        flags
            .iter()
            .find(|(flag, _)| *flag == name)
            .and_then(|(_, value)| *value)
    };
    for (name, _) in &flags {
        let known = match cmd.as_str() {
            "read" => ["--lines"].contains(name),
            "list" => ["--hidden"].contains(name),
            "search" => ["--content", "--fixed", "--hidden", "--limit"].contains(name),
            "write" => ["--revision", "--create"].contains(name),
            _ => false,
        };
        if !known {
            anyhow::bail!("unknown flag '{name}'\n\n{FILE_USAGE}");
        }
    }
    match (cmd.as_str(), positional.as_slice()) {
        ("inspect", [reference]) => Ok(ControlRequest::FileInspect {
            reference: reference.to_string(),
        }),
        ("read", [reference]) => Ok(ControlRequest::FileRead {
            reference: reference.to_string(),
            lines: value("--lines")
                .map(LineRange::parse)
                .transpose()
                .map_err(|error| anyhow::anyhow!(error))?,
        }),
        ("list", []) | ("list", [_]) => Ok(ControlRequest::FileList {
            path: positional.first().map(|path| path.to_string()),
            hidden: flag("--hidden"),
        }),
        ("search", query) if !query.is_empty() => Ok(ControlRequest::FileSearch {
            query: query.join(" "),
            content: flag("--content"),
            fixed: flag("--fixed"),
            hidden: flag("--hidden"),
            limit: parse_limit(value("--limit"))?,
        }),
        ("write", [reference]) => {
            let revision = value("--revision").map(|raw| FileRevision(raw.to_string()));
            let create = flag("--create");
            if revision.is_none() && !create {
                anyhow::bail!(
                    "file write needs --revision <rev> (from `file read`/`file inspect`) or --create\n\n{FILE_USAGE}"
                );
            }
            Ok(ControlRequest::FileWrite {
                reference: reference.to_string(),
                content: read_stdin_to_string()?,
                revision,
                create,
            })
        }
        _ => anyhow::bail!(FILE_USAGE),
    }
}

fn parse_git(args: &[String]) -> anyhow::Result<ControlRequest> {
    let Some((cmd, rest)) = args.split_first() else {
        anyhow::bail!(GIT_USAGE);
    };
    let (positional, flags) = split_flags(rest, &["-n", "-m", "--message"], GIT_USAGE)?;
    let flag = |name: &str| flags.iter().any(|(flag, _)| *flag == name);
    let value = |name: &str| {
        flags
            .iter()
            .find(|(flag, _)| *flag == name)
            .and_then(|(_, value)| *value)
    };
    for (name, _) in &flags {
        let known = match cmd.as_str() {
            "diff" => ["--staged", "--cached", "--unstaged"].contains(name),
            "log" => ["-n"].contains(name),
            "discard" | "restore" => ["--confirm"].contains(name),
            "commit" => ["-m", "--message"].contains(name),
            "stage" | "add" | "unstage" => ["--all"].contains(name),
            "push" => ["--set-upstream", "-u"].contains(name),
            _ => false,
        };
        if !known {
            anyhow::bail!("unknown flag '{name}'\n\n{GIT_USAGE}");
        }
    }
    let paths = || {
        positional
            .iter()
            .map(|path| path.to_string())
            .collect::<Vec<_>>()
    };
    match (cmd.as_str(), positional.as_slice()) {
        ("status", []) => Ok(ControlRequest::GitStatus),
        ("diff", [] | [_]) => Ok(ControlRequest::GitDiff {
            path: positional.first().map(|path| path.to_string()),
            scope: if flag("--staged") || flag("--cached") {
                DiffScope::Staged
            } else if flag("--unstaged") {
                DiffScope::Unstaged
            } else {
                DiffScope::Head
            },
        }),
        ("log", [] | [_]) => Ok(ControlRequest::GitLog {
            path: positional.first().map(|path| path.to_string()),
            limit: parse_limit(value("-n"))?,
        }),
        ("stage" | "add", []) if flag("--all") => Ok(ControlRequest::GitStageAll),
        ("unstage", []) if flag("--all") => Ok(ControlRequest::GitUnstageAll),
        ("branches" | "branch", []) => Ok(ControlRequest::GitBranches),
        ("branch", [name]) => Ok(ControlRequest::GitCreateBranch {
            branch: name.to_string(),
        }),
        ("switch" | "checkout", [name]) => Ok(ControlRequest::GitSwitch {
            branch: name.to_string(),
        }),
        ("fetch", []) => Ok(ControlRequest::GitFetch),
        ("pull", []) => Ok(ControlRequest::GitPull),
        ("push", []) => Ok(ControlRequest::GitPush {
            set_upstream: flag("--set-upstream") || flag("-u"),
        }),
        ("stage" | "add", paths_given) if !paths_given.is_empty() => {
            Ok(ControlRequest::GitStage { paths: paths() })
        }
        ("unstage", paths_given) if !paths_given.is_empty() => {
            Ok(ControlRequest::GitUnstage { paths: paths() })
        }
        ("discard" | "restore", paths_given) if !paths_given.is_empty() => {
            Ok(ControlRequest::GitDiscard {
                paths: paths(),
                confirmed: flag("--confirm"),
            })
        }
        ("commit", []) => Ok(ControlRequest::GitCommit {
            message: value("-m")
                .or_else(|| value("--message"))
                .ok_or_else(|| anyhow::anyhow!("git commit needs -m \"<message>\"\n\n{GIT_USAGE}"))?
                .to_string(),
        }),
        _ => anyhow::bail!(GIT_USAGE),
    }
}

fn parse_uuid_arg(raw: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(raw).map_err(|_| anyhow::anyhow!("'{raw}' is not a valid id"))
}

fn parse_drawing(args: &[String]) -> anyhow::Result<ControlRequest> {
    match args {
        [] => Ok(ControlRequest::DrawingList {
            requested_by: acting_agent_id(),
        }),
        [only] if only == "list" => Ok(ControlRequest::DrawingList {
            requested_by: acting_agent_id(),
        }),
        [cmd, id] if cmd == "read" => Ok(ControlRequest::DrawingRead {
            requested_by: acting_agent_id(),
            id: parse_uuid_arg(id)?,
        }),
        _ => anyhow::bail!(DRAWING_USAGE),
    }
}

const DRAWING_USAGE: &str = r#"usage:
  duetctl drawing list         (drawings connected to you)
  duetctl drawing read <id>    (renders it to a PNG; prints the path and its strokes)"#;

fn parse_notes(args: &[String]) -> anyhow::Result<ControlRequest> {
    match args {
        [only] if only == "list" => Ok(ControlRequest::NotesList {
            requested_by: acting_agent_id(),
        }),
        [cmd, id] if cmd == "read" => Ok(ControlRequest::NotesRead {
            requested_by: acting_agent_id(),
            id: parse_uuid_arg(id)?,
        }),
        [cmd, id] if cmd == "replace" => Ok(ControlRequest::NotesReplace {
            requested_by: acting_agent_id(),
            id: parse_uuid_arg(id)?,
            markdown: read_stdin_to_string()?,
        }),
        [cmd, id] if cmd == "append" => Ok(ControlRequest::NotesAppend {
            requested_by: acting_agent_id(),
            id: parse_uuid_arg(id)?,
            addition: read_stdin_to_string()?,
        }),
        [cmd, id, rest @ ..] if cmd == "patch" => parse_notes_patch(parse_uuid_arg(id)?, rest),
        [cmd, id] if cmd == "connections" => Ok(ControlRequest::NotesConnections {
            id: parse_uuid_arg(id)?,
        }),
        [cmd, id, path] if cmd == "attach" => Ok(ControlRequest::NotesAttach {
            requested_by: acting_agent_id(),
            id: parse_uuid_arg(id)?,
            path: path.clone(),
        }),
        [cmd, id] if cmd == "detach" => Ok(ControlRequest::NotesDetach {
            requested_by: acting_agent_id(),
            id: parse_uuid_arg(id)?,
        }),
        _ => anyhow::bail!(NOTES_USAGE),
    }
}

fn parse_notes_patch(id: Uuid, args: &[String]) -> anyhow::Result<ControlRequest> {
    let (mut old, mut new) = (None, None);
    let mut i = 0;
    while i + 1 < args.len() {
        let (flag, value) = (args[i].as_str(), args[i + 1].clone());
        match flag {
            "--old" => old = Some(value),
            "--new" => new = Some(value),
            other => anyhow::bail!("unknown flag '{other}'\n\n{NOTES_USAGE}"),
        }
        i += 2;
    }
    Ok(ControlRequest::NotesPatch {
        requested_by: acting_agent_id(),
        id,
        old: old.ok_or_else(|| anyhow::anyhow!("notes patch needs --old\n\n{NOTES_USAGE}"))?,
        new: new.ok_or_else(|| anyhow::anyhow!("notes patch needs --new\n\n{NOTES_USAGE}"))?,
    })
}

fn parse_legacy_agent(args: &[String]) -> anyhow::Result<ControlRequest> {
    match args {
        [only] if only == "list" => Ok(ControlRequest::List),
        [cmd, target, rest @ ..] if cmd == "send" && !rest.is_empty() => Ok(ControlRequest::Send {
            source_session_id: acting_agent_id(),
            target: target.clone(),
            content: rest.join(" "),
        }),
        _ => anyhow::bail!(r#"usage: duet agent list | duet agent send <agent> "<message>""#),
    }
}

fn parse_agents(args: &[String]) -> anyhow::Result<ControlRequest> {
    match args {
        [only] if only == "list" => Ok(ControlRequest::AgentsList),
        [cmd, target] if cmd == "inspect" => Ok(ControlRequest::AgentsInspect {
            target: target.clone(),
        }),
        [cmd, rest @ ..] if cmd == "create" => parse_agents_create(rest),
        [cmd, target] if cmd == "remove" => Ok(ControlRequest::AgentsRemove {
            target: target.clone(),
            requested_by: acting_agent_id(),
        }),
        [cmd, target, role] if cmd == "assign-role" => Ok(ControlRequest::AgentsAssignRole {
            target: target.clone(),
            role: Some(role.clone()),
            requested_by: acting_agent_id(),
        }),
        _ => anyhow::bail!(USAGE),
    }
}

fn parse_agents_create(args: &[String]) -> anyhow::Result<ControlRequest> {
    let (mut name, mut cwd, mut agent, mut role) = (None, None, None, None);
    let mut i = 0;
    while i + 1 < args.len() {
        let (flag, value) = (args[i].as_str(), args[i + 1].clone());
        match flag {
            "--name" => name = Some(value),
            "--cwd" => cwd = Some(value),
            "--agent" => agent = Some(value),
            "--role" => role = Some(value),
            other => anyhow::bail!("unknown flag '{other}'\n\n{USAGE}"),
        }
        i += 2;
    }
    Ok(ControlRequest::AgentsCreate {
        name: name.ok_or_else(|| anyhow::anyhow!("agents create needs --name\n\n{USAGE}"))?,
        cwd: PathBuf::from(
            cwd.ok_or_else(|| anyhow::anyhow!("agents create needs --cwd\n\n{USAGE}"))?,
        ),
        agent: agent.ok_or_else(|| anyhow::anyhow!("agents create needs --agent\n\n{USAGE}"))?,
        role,
        requested_by: acting_agent_id(),
    })
}

fn parse_send(args: &[String]) -> anyhow::Result<ControlRequest> {
    let mut from = acting_agent_id();
    let mut to = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" if i + 1 < args.len() => {
                from = Some(
                    Uuid::parse_str(&args[i + 1])
                        .map_err(|_| anyhow::anyhow!("--from must be an agent id"))?,
                );
                i += 2;
            }
            "--to" if i + 1 < args.len() => {
                to = Some(args[i + 1].clone());
                i += 2;
            }
            _ => break,
        }
    }
    let to = to.ok_or_else(|| anyhow::anyhow!("send needs --to <agent>\n\n{USAGE}"))?;
    let content = args[i..].join(" ");
    if content.is_empty() {
        anyhow::bail!("send needs a message\n\n{USAGE}");
    }
    Ok(ControlRequest::SendMessage { from, to, content })
}

fn print_response(response: &ControlResponse) {
    match response {
        ControlResponse::Agents { agents, links } => {
            if agents.is_empty() {
                println!("no agents running");
            }
            for agent in agents {
                println!("{}\t{}\t{}", agent.id, agent.name, agent.agent);
            }
            for link in links {
                println!("connected: {} <-> {}", link.source, link.target);
            }
        }
        ControlResponse::Sent { message } => {
            println!("{:?} (message {})", message.status, message.id);
        }
        ControlResponse::AgentList { agents } => {
            if agents.is_empty() {
                println!("no agents running");
            }
            for agent in agents {
                let role = agent.role.as_deref().unwrap_or("-");
                let manager = if agent.manager { " (manager)" } else { "" };
                println!(
                    "{}\t{}\t{}\t{role}{manager}\t{}",
                    agent.id, agent.name, agent.provider, agent.activity
                );
            }
        }
        ControlResponse::AgentDetail { agent } => {
            println!("id:       {}", agent.id);
            println!("name:     {}", agent.name);
            println!("provider: {}", agent.provider);
            println!("role:     {}", agent.role.as_deref().unwrap_or("-"));
            println!("manager:  {}", agent.manager);
            println!("activity: {}", agent.activity);
        }
        ControlResponse::Connections { connections } => {
            if connections.is_empty() {
                println!("no connections");
            }
            for connection in connections {
                let capabilities = if connection.capabilities.is_empty() {
                    "visual only".to_string()
                } else {
                    connection.capabilities.join(",")
                };
                println!(
                    "{} -> {} [{capabilities}]",
                    connection.source_name, connection.target_name
                );
            }
        }
        ControlResponse::Workspace { workspace } => {
            println!("id:          {}", workspace.id);
            println!("name:        {}", workspace.name);
            println!("root:        {}", workspace.root_dir);
            println!("environment: {}", workspace.environment);
            println!("nodes:       {}", workspace.node_count);
            println!("edges:       {}", workspace.edge_count);
        }
        ControlResponse::Created { id } => println!("created agent {id}"),
        ControlResponse::Removed => println!("removed"),
        ControlResponse::RoleAssigned => println!("role assigned"),
        ControlResponse::Whoami { info } => {
            println!("id:       {}", info.agent.id);
            println!("name:     {}", info.agent.name);
            println!("provider: {}", info.agent.provider);
            println!("role:     {}", info.agent.role.as_deref().unwrap_or("-"));
            println!("manager:  {}", info.agent.manager);
            println!("activity: {}", info.agent.activity);
            if let Some(instructions) = &info.role_instructions {
                println!("\nrole instructions:\n{instructions}");
            }
            if info.connected_agents.is_empty() {
                println!("\nconnected to: none");
            } else {
                println!("\nconnected to:");
                for agent in &info.connected_agents {
                    println!("  {}\t{}\t{}", agent.id, agent.name, agent.provider);
                }
            }
            if !info.connected_files.is_empty() {
                println!(
                    "\nconnected files and folders (`duetctl file inspect <ref>`, then `file read` or `file list`):"
                );
                for reference in &info.connected_files {
                    println!("  {reference}");
                }
            }
            if !info.connected_drawings.is_empty() {
                println!(
                    "\nconnected drawings (`duetctl drawing read <id>` renders one to a PNG):"
                );
                for drawing in &info.connected_drawings {
                    println!("  {}\t{} strokes", drawing.id, drawing.strokes);
                }
            }
            if !info.connected_portals.is_empty() {
                println!("\nconnected browser portals (`duetctl portal inspect <id>`):");
                for portal in &info.connected_portals {
                    println!(
                        "  {}\t{}\t{}",
                        portal.id,
                        portal.name,
                        if portal.controllable {
                            "control"
                        } else {
                            "no access"
                        }
                    );
                }
            }
        }
        ControlResponse::Drawings { drawings } => {
            if drawings.is_empty() {
                println!("no drawings (connect one to your terminal on the canvas)");
            }
            for drawing in drawings {
                println!("{}\t{} strokes", drawing.id, drawing.strokes);
            }
        }
        ControlResponse::Drawing { drawing } => {
            println!("image:   {}", drawing.path.display());
            println!("size:    {}x{} px", drawing.width, drawing.height);
            println!("strokes: {}", drawing.strokes.len());
            for (index, stroke) in drawing.strokes.iter().enumerate() {
                let points: Vec<String> = stroke
                    .points
                    .iter()
                    .map(|(x, y)| format!("{x},{y}"))
                    .collect();
                println!(
                    "  {} {} w{:.0}: {}",
                    index + 1,
                    stroke.color,
                    stroke.width,
                    points.join(" ")
                );
            }
        }
        ControlResponse::Notes { notes } => {
            if notes.is_empty() {
                println!("no notes");
            }
            for note in notes {
                println!("{}\t{}", note.id, note.title);
            }
        }
        ControlResponse::Note { note } => {
            println!("id:    {}", note.id);
            println!("title: {}", note.title);
            println!("color: {}", note.color);
            if let Some(file) = &note.file {
                println!("file:  {file}");
            }
            println!("\n{}", note.markdown);
        }
        ControlResponse::NoteAttached { id, path } => {
            println!("note {id} is now synced with {path}")
        }
        ControlResponse::FileInfo { info } => {
            println!("path:      {}", info.path);
            println!("reference: {}", info.reference);
            println!("kind:      {:?}", info.kind);
            println!("size:      {} bytes", info.size);
            if let Some(class) = info.class {
                println!("class:     {class:?}");
            }
            if let Some(lines) = info.line_count {
                println!("lines:     {lines}");
            }
            if let Some(revision) = &info.revision {
                println!("revision:  {revision}");
            }
            if let Some(status) = info.git_status {
                println!("git:       {status}");
            }
        }
        ControlResponse::FileContent { file } => {
            // Raw content only, so `duetctl file read x > copy` works; the
            // revision and line count are on `--json` / `file inspect`.
            print!("{}", file.content);
            if !file.content.is_empty() && !file.content.ends_with('\n') {
                println!();
            }
        }
        ControlResponse::FileEntries { entries } => {
            for entry in entries {
                let suffix = if entry.kind == crate::project::fs::EntryKind::Directory {
                    "/"
                } else {
                    ""
                };
                println!("{}{suffix}", entry.path);
            }
        }
        ControlResponse::FileMatches { matches } => {
            if matches.is_empty() {
                println!("no matching files");
            }
            for hit in matches {
                println!("{}", hit.path);
            }
        }
        ControlResponse::ContentMatches { search } => {
            if search.matches.is_empty() {
                println!("no matches");
            }
            for hit in &search.matches {
                println!("{}:{}:{}: {}", hit.path, hit.line, hit.column, hit.text);
            }
            if search.truncated {
                println!("(more matches not shown; narrow the pattern or raise --limit)");
            }
        }
        ControlResponse::FileWritten { path, revision } => {
            println!("wrote {path} (revision {revision})")
        }
        ControlResponse::GitStatus { status } => {
            println!(
                "branch: {}",
                status.branch.as_deref().unwrap_or("(detached)")
            );
            match &status.upstream {
                Some(upstream) if status.upstream_gone => {
                    println!("upstream: {upstream} (gone)")
                }
                Some(upstream) => println!(
                    "upstream: {upstream} (ahead {}, behind {})",
                    status.ahead, status.behind
                ),
                None => println!("upstream: none"),
            }
            if status.entries.is_empty() {
                println!("clean");
            }
            for entry in &status.entries {
                match &entry.original_path {
                    Some(original) => println!(
                        "{}{} {} -> {}",
                        entry.index, entry.worktree, original, entry.path
                    ),
                    None => println!("{}{} {}", entry.index, entry.worktree, entry.path),
                }
            }
        }
        ControlResponse::GitDiff { diff } => print!("{diff}"),
        ControlResponse::GitLog { entries } => {
            for entry in entries {
                println!("{} {} ({})", entry.short_hash, entry.subject, entry.author);
            }
        }
        ControlResponse::GitDone { message } => println!("{message}"),
        ControlResponse::GitBranches { branches } => {
            for branch in branches {
                let marker = if branch.current { "*" } else { " " };
                match &branch.upstream {
                    Some(upstream) => println!("{marker} {} -> {upstream}", branch.name),
                    None => println!("{marker} {}", branch.name),
                }
            }
        }
        ControlResponse::NoteUpdated { id } => println!("updated note {id}"),
        ControlResponse::Resolved { outcome } => print_resolve_outcome(outcome),
        ControlResponse::ResourceInspected { resource, detail } => {
            print_resolved_resource(resource);
            match detail {
                ResourceDetail::Agent(info) => {
                    println!("provider:  {}", info.provider);
                    println!("role:      {}", info.role.as_deref().unwrap_or("-"));
                    println!("manager:   {}", info.manager);
                    println!("activity:  {}", info.activity);
                }
                ResourceDetail::Note(note) => {
                    println!("title:     {}", note.title);
                }
                ResourceDetail::Portal(portal) => {
                    println!("url:       {}", portal.url.as_deref().unwrap_or("-"));
                    println!("scripts:   {}", portal.allow_scripts);
                }
            }
        }
        ControlResponse::Portals { portals } => {
            if portals.is_empty() {
                println!("no portals");
            }
            for portal in portals {
                let access = if portal.controllable {
                    "control"
                } else {
                    "no access"
                };
                println!(
                    "{}\t{}\t{access}\t{}",
                    portal.id,
                    portal.name,
                    portal
                        .url
                        .as_deref()
                        .filter(|url| !url.is_empty())
                        .unwrap_or("-")
                );
            }
        }
        ControlResponse::PortalDetail { portal } => {
            println!("id:          {}", portal.id);
            println!("name:        {}", portal.name);
            println!("url:         {}", portal.url);
            println!("title:       {}", portal.title.as_deref().unwrap_or("-"));
            println!("loading:     {}", portal.loading);
            println!(
                "history:     back={} forward={}",
                portal.can_go_back, portal.can_go_forward
            );
            println!("profile:     {} ({})", portal.profile_id, portal.storage);
            println!("scripts:     {}", portal.allow_scripts);
            println!(
                "controllers: {}",
                if portal.controllers.is_empty() {
                    "-".to_string()
                } else {
                    portal.controllers.join(", ")
                }
            );
        }
        ControlResponse::PortalAction { action } => {
            println!("{}", action.detail);
            println!("url:   {}", action.url);
            println!("title: {}", action.title.as_deref().unwrap_or("-"));
        }
        ControlResponse::PortalPage { page } => {
            println!("url:   {}", page.url);
            println!("title: {}", page.title);
            println!();
            println!("{}", page.text);
            if page.truncated {
                println!("\n[truncated — use --selector or --limit to read a specific part]");
            }
        }
        ControlResponse::PortalScreenshot { screenshot } => {
            println!("{}", screenshot.path);
            println!(
                "{}x{} of {}",
                screenshot.width, screenshot.height, screenshot.url
            );
        }
        ControlResponse::PortalEvaluated { value, .. } => {
            println!(
                "{}",
                serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string())
            );
        }
        ControlResponse::Error { error } => {
            eprintln!("duetctl: {error}");
        }
    }
}

fn print_resolved_resource(resource: &ResolvedResource) {
    println!("kind:      {}", resource.kind.label());
    println!("id:        {}", resource.id);
    println!("name:      {}", resource.name);
    if resource.path.is_some() {
        println!("reference: {}", resource.reference());
    }
    if let Some(lines) = resource.lines {
        println!("lines:     {}-{}", lines.start, lines.end);
    }
    println!(
        "workspace: {} ({})",
        resource.workspace_name, resource.workspace_id
    );
    println!("floor:     {:?}", resource.floor);
}

fn print_resolve_outcome(outcome: &ResolveOutcome) {
    match outcome {
        ResolveOutcome::Found { resource } => print_resolved_resource(resource),
        ResolveOutcome::Ambiguous { candidates } => {
            println!("ambiguous: {} resources match", candidates.len());
            for candidate in candidates {
                println!(
                    "  {}\t{}\t{}\tworkspace={} ({})\tfloor={:?}",
                    candidate.kind.label(),
                    candidate.id,
                    candidate.name,
                    candidate.workspace_name,
                    candidate.workspace_id,
                    candidate.floor
                );
            }
        }
        ResolveOutcome::NotFound => println!("no resource found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_list_still_parses_with_no_arguments() {
        assert!(matches!(
            parse_cli_request(&["agent".to_string(), "list".to_string()]).unwrap(),
            ControlRequest::List
        ));
    }

    #[test]
    fn legacy_send_joins_trailing_words_into_one_content_string() {
        let args = [
            "agent".to_string(),
            "send".to_string(),
            "dev".to_string(),
            "hello".to_string(),
            "there".to_string(),
        ];
        let ControlRequest::Send {
            target, content, ..
        } = parse_cli_request(&args).unwrap()
        else {
            panic!("expected a Send request");
        };
        assert_eq!(target, "dev");
        assert_eq!(content, "hello there");
    }

    #[test]
    fn legacy_send_without_content_is_rejected() {
        assert!(
            parse_cli_request(&["agent".to_string(), "send".to_string(), "dev".to_string()])
                .is_err()
        );
    }

    #[test]
    fn unknown_command_is_rejected() {
        assert!(parse_cli_request(&["dance".to_string()]).is_err());
    }

    #[test]
    fn requests_round_trip_through_json() {
        let request = ControlRequest::Send {
            source_session_id: Some(Uuid::nil()),
            target: "dev".to_string(),
            content: "hi".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        let parsed: ControlRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            ControlRequest::Send {
                source_session_id,
                target,
                content,
            } => {
                assert_eq!(source_session_id, Some(Uuid::nil()));
                assert_eq!(target, "dev");
                assert_eq!(content, "hi");
            }
            _ => panic!("expected Send"),
        }
    }

    #[test]
    fn agents_list_parses() {
        assert!(matches!(
            parse_cli_request(&["agents".to_string(), "list".to_string()]).unwrap(),
            ControlRequest::AgentsList
        ));
    }

    #[test]
    fn agents_inspect_parses_its_target() {
        let ControlRequest::AgentsInspect { target } = parse_cli_request(&[
            "agents".to_string(),
            "inspect".to_string(),
            "backend".to_string(),
        ])
        .unwrap() else {
            panic!("expected AgentsInspect");
        };
        assert_eq!(target, "backend");
    }

    #[test]
    fn send_parses_from_and_to_flags_and_joins_the_rest_as_content() {
        let from = Uuid::new_v4();
        let args = [
            "send".to_string(),
            "--from".to_string(),
            from.to_string(),
            "--to".to_string(),
            "backend".to_string(),
            "please".to_string(),
            "inspect".to_string(),
        ];
        let ControlRequest::SendMessage {
            from: parsed_from,
            to,
            content,
        } = parse_cli_request(&args).unwrap()
        else {
            panic!("expected SendMessage");
        };
        assert_eq!(parsed_from, Some(from));
        assert_eq!(to, "backend");
        assert_eq!(content, "please inspect");
    }

    #[test]
    fn send_without_to_is_rejected() {
        assert!(parse_cli_request(&["send".to_string(), "hello".to_string()]).is_err());
    }

    #[test]
    fn agents_create_parses_every_flag() {
        let args = [
            "agents".to_string(),
            "create".to_string(),
            "--name".to_string(),
            "backend".to_string(),
            "--cwd".to_string(),
            "/tmp".to_string(),
            "--agent".to_string(),
            "claude".to_string(),
            "--role".to_string(),
            "Developer".to_string(),
        ];
        let ControlRequest::AgentsCreate {
            name,
            cwd,
            agent,
            role,
            ..
        } = parse_cli_request(&args).unwrap()
        else {
            panic!("expected AgentsCreate");
        };
        assert_eq!(name, "backend");
        assert_eq!(cwd, PathBuf::from("/tmp"));
        assert_eq!(agent, "claude");
        assert_eq!(role, Some("Developer".to_string()));
    }

    #[test]
    fn agents_assign_role_parses_none_as_a_literal_value() {
        let args = [
            "agents".to_string(),
            "assign-role".to_string(),
            "backend".to_string(),
            "none".to_string(),
        ];
        let ControlRequest::AgentsAssignRole { target, role, .. } =
            parse_cli_request(&args).unwrap()
        else {
            panic!("expected AgentsAssignRole");
        };
        assert_eq!(target, "backend");
        assert_eq!(role, Some("none".to_string()));
    }

    #[test]
    fn connections_and_workspace_inspect_parse() {
        assert!(matches!(
            parse_cli_request(&["connections".to_string(), "list".to_string()]).unwrap(),
            ControlRequest::ConnectionsList
        ));
        assert!(matches!(
            parse_cli_request(&["workspace".to_string(), "inspect".to_string()]).unwrap(),
            ControlRequest::WorkspaceInspect
        ));
    }

    #[test]
    fn whoami_parses_with_no_arguments_and_is_a_recognized_verb() {
        assert!(is_cli_verb("whoami"));
        assert!(matches!(
            parse_cli_request(&["whoami".to_string()]).unwrap(),
            ControlRequest::Whoami { .. }
        ));
    }

    #[test]
    fn parse_provider_accepts_every_builtin_case_insensitively() {
        assert_eq!(parse_provider("Claude").unwrap(), Agent::Claude);
        assert_eq!(parse_provider("CODEX").unwrap(), Agent::Codex);
        assert_eq!(parse_provider("opencode").unwrap(), Agent::OpenCode);
        assert_eq!(parse_provider("shell").unwrap(), Agent::Shell);
        assert!(parse_provider("custom").is_err());
    }

    #[test]
    fn is_cli_verb_recognizes_every_dispatched_verb_and_nothing_else() {
        for verb in [
            "agent",
            "agents",
            "send",
            "connections",
            "workspace",
            "notes",
        ] {
            assert!(is_cli_verb(verb));
        }
        assert!(!is_cli_verb("--version"));
    }

    #[test]
    fn drawing_commands_parse() {
        let id = Uuid::new_v4();
        assert!(is_cli_verb("drawing"));
        assert!(matches!(
            parse_cli_request(&["drawing".to_string(), "list".to_string()]).unwrap(),
            ControlRequest::DrawingList { .. }
        ));
        assert!(matches!(
            parse_cli_request(&["drawing".to_string(), "read".to_string(), id.to_string()])
                .unwrap(),
            ControlRequest::DrawingRead { id: parsed, .. } if parsed == id
        ));
        assert!(
            parse_cli_request(&["drawing".to_string(), "read".to_string(), "x".to_string()])
                .is_err()
        );
        assert!(parse_cli_request(&["drawing".to_string(), "draw".to_string()]).is_err());
    }

    #[test]
    fn notes_list_uses_the_acting_agent_id() {
        assert!(matches!(
            parse_cli_request(&["notes".to_string(), "list".to_string()]).unwrap(),
            ControlRequest::NotesList { .. }
        ));
    }

    #[test]
    fn notes_read_parses_its_id() {
        let id = Uuid::new_v4();
        let ControlRequest::NotesRead { id: parsed_id, .. } =
            parse_cli_request(&["notes".to_string(), "read".to_string(), id.to_string()]).unwrap()
        else {
            panic!("expected NotesRead");
        };
        assert_eq!(parsed_id, id);
    }

    #[test]
    fn notes_read_rejects_a_non_uuid_id() {
        assert!(
            parse_cli_request(&[
                "notes".to_string(),
                "read".to_string(),
                "not-a-uuid".to_string()
            ])
            .is_err()
        );
    }

    #[test]
    fn notes_patch_parses_old_and_new_flags() {
        let id = Uuid::new_v4();
        let args = [
            "notes".to_string(),
            "patch".to_string(),
            id.to_string(),
            "--old".to_string(),
            "not started".to_string(),
            "--new".to_string(),
            "done".to_string(),
        ];
        let ControlRequest::NotesPatch {
            id: parsed_id,
            old,
            new,
            ..
        } = parse_cli_request(&args).unwrap()
        else {
            panic!("expected NotesPatch");
        };
        assert_eq!(parsed_id, id);
        assert_eq!(old, "not started");
        assert_eq!(new, "done");
    }

    #[test]
    fn notes_patch_without_new_flag_is_rejected() {
        let id = Uuid::new_v4();
        let args = [
            "notes".to_string(),
            "patch".to_string(),
            id.to_string(),
            "--old".to_string(),
            "x".to_string(),
        ];
        assert!(parse_cli_request(&args).is_err());
    }

    #[test]
    fn notes_connections_parses_its_id() {
        let id = Uuid::new_v4();
        let ControlRequest::NotesConnections { id: parsed_id } = parse_cli_request(&[
            "notes".to_string(),
            "connections".to_string(),
            id.to_string(),
        ])
        .unwrap() else {
            panic!("expected NotesConnections");
        };
        assert_eq!(parsed_id, id);
    }

    #[test]
    fn notes_requests_round_trip_through_json() {
        let id = Uuid::new_v4();
        let request = ControlRequest::NotesReplace {
            requested_by: Some(Uuid::nil()),
            id,
            markdown: "# hi\n\nbody".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        let parsed: ControlRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            ControlRequest::NotesReplace {
                requested_by,
                id: parsed_id,
                markdown,
            } => {
                assert_eq!(requested_by, Some(Uuid::nil()));
                assert_eq!(parsed_id, id);
                assert_eq!(markdown, "# hi\n\nbody");
            }
            _ => panic!("expected NotesReplace"),
        }
    }

    #[test]
    fn resolve_parses_its_reference() {
        let ControlRequest::Resolve { reference, .. } =
            parse_cli_request(&["resolve".to_string(), "@backend".to_string()]).unwrap()
        else {
            panic!("expected Resolve");
        };
        assert_eq!(reference, "@backend");
    }

    #[test]
    fn resource_inspect_parses_its_id() {
        let id = Uuid::new_v4();
        let ControlRequest::ResourceInspect { id: parsed_id, .. } = parse_cli_request(&[
            "resource".to_string(),
            "inspect".to_string(),
            id.to_string(),
        ])
        .unwrap() else {
            panic!("expected ResourceInspect");
        };
        assert_eq!(parsed_id, id);
    }

    #[test]
    fn resolve_and_resource_are_recognized_verbs() {
        assert!(is_cli_verb("resolve"));
        assert!(is_cli_verb("resource"));
    }

    #[test]
    fn a_trailing_json_flag_is_stripped_before_parsing_and_recognized_elsewhere() {
        let args = [
            "resolve".to_string(),
            "@backend".to_string(),
            "--json".to_string(),
        ];
        let (json, stripped) = match args.split_last() {
            Some((last, rest)) if last == "--json" => (true, rest),
            _ => (false, &args[..]),
        };
        assert!(json);
        let ControlRequest::Resolve { reference, .. } = parse_cli_request(stripped).unwrap() else {
            panic!("expected Resolve");
        };
        assert_eq!(reference, "@backend");
    }

    #[test]
    fn resolve_requests_round_trip_through_json() {
        let request = ControlRequest::Resolve {
            reference: "@backend".to_string(),
            requested_by: Some(Uuid::nil()),
        };
        let json = serde_json::to_string(&request).unwrap();
        let parsed: ControlRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            ControlRequest::Resolve {
                reference,
                requested_by,
            } => {
                assert_eq!(reference, "@backend");
                assert_eq!(requested_by, Some(Uuid::nil()));
            }
            _ => panic!("expected Resolve"),
        }
    }

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn portal_commands_parse() {
        assert!(is_cli_verb("portal") && is_cli_verb("portals"));
        assert!(matches!(
            parse_cli_request(&args(&["portal", "list"])).unwrap(),
            ControlRequest::PortalList { .. }
        ));
        for (verb, expected) in [
            ("inspect", None),
            ("url", Some("url")),
            ("title", Some("title")),
        ] {
            let ControlRequest::PortalInspect { portal, show, .. } =
                parse_cli_request(&args(&["portal", verb, "@portal:frontend"])).unwrap()
            else {
                panic!("expected PortalInspect");
            };
            assert_eq!(portal, "@portal:frontend");
            assert_eq!(show.as_deref(), expected);
        }
        let ControlRequest::PortalNavigate { portal, url, .. } =
            parse_cli_request(&args(&["portal", "navigate", "frontend", "localhost:3000"]))
                .unwrap()
        else {
            panic!("expected PortalNavigate");
        };
        assert_eq!(
            (portal.as_str(), url.as_str()),
            ("frontend", "localhost:3000")
        );
        for (verb, step) in [
            ("back", PortalStep::Back),
            ("forward", PortalStep::Forward),
            ("reload", PortalStep::Reload),
        ] {
            let ControlRequest::PortalStep { step: parsed, .. } =
                parse_cli_request(&args(&["portal", verb, "@frontend"])).unwrap()
            else {
                panic!("expected PortalStep");
            };
            assert_eq!(parsed, step);
        }
        let ControlRequest::PortalText {
            selector,
            html,
            limit,
            ..
        } = parse_cli_request(&args(&[
            "portal",
            "text",
            "@frontend",
            "--selector",
            "#status",
            "--html",
            "--limit",
            "500",
        ]))
        .unwrap()
        else {
            panic!("expected PortalText");
        };
        assert_eq!(selector.as_deref(), Some("#status"));
        assert!(html);
        assert_eq!(limit, Some(500));
        assert!(matches!(
            parse_cli_request(&args(&["portal", "screenshot", "@frontend", "--full"])).unwrap(),
            ControlRequest::PortalScreenshot {
                full_page: true,
                ..
            }
        ));
        let ControlRequest::PortalClick { selector, .. } =
            parse_cli_request(&args(&["portal", "click", "@frontend", "button.primary"])).unwrap()
        else {
            panic!("expected PortalClick");
        };
        assert_eq!(selector, "button.primary");
        let ControlRequest::PortalType {
            selector,
            text,
            append,
            submit,
            ..
        } = parse_cli_request(&args(&[
            "portal",
            "type",
            "@frontend",
            "#user",
            "ada lovelace",
            "--submit",
        ]))
        .unwrap()
        else {
            panic!("expected PortalType");
        };
        assert_eq!(
            (selector.as_str(), text.as_str()),
            ("#user", "ada lovelace")
        );
        assert!(submit && !append);
        let ControlRequest::PortalEvaluate { script, .. } = parse_cli_request(&args(&[
            "portal",
            "evaluate",
            "@frontend",
            "document.title",
        ]))
        .unwrap() else {
            panic!("expected PortalEvaluate");
        };
        assert_eq!(script, "document.title");

        // Missing arguments, unknown flags, unquoted extra words and
        // unknown verbs are refused rather than guessed at.
        for bad in [
            &["portal"][..],
            &["portal", "text"],
            &["portal", "navigate", "frontend"],
            &["portal", "click", "frontend"],
            &["portal", "type", "frontend", "#user"],
            &["portal", "text", "frontend", "--full"],
            &["portal", "click", "frontend", "a", "b"],
            &["portal", "explode", "frontend"],
        ] {
            assert!(parse_cli_request(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn portal_requests_are_async_and_round_trip_through_json() {
        let request = ControlRequest::PortalType {
            requested_by: Some(Uuid::new_v4()),
            portal: "@portal:frontend".to_string(),
            selector: "#q".to_string(),
            text: "x\ny".to_string(),
            append: true,
            submit: false,
        };
        assert!(request.is_async());
        assert!(request.reply_timeout() > REPLY_TIMEOUT);
        let json = serde_json::to_string(&request).unwrap();
        let ControlRequest::PortalType { text, append, .. } = serde_json::from_str(&json).unwrap()
        else {
            panic!("expected PortalType");
        };
        assert_eq!((text.as_str(), append), ("x\ny", true));
        let list = ControlRequest::PortalList { requested_by: None };
        assert!(!list.is_async());
        assert_eq!(list.reply_timeout(), REPLY_TIMEOUT);
        // `show` is presentation-only and omitted when unset.
        let inspect = serde_json::to_string(&ControlRequest::PortalInspect {
            requested_by: None,
            portal: "x".to_string(),
            show: None,
        })
        .unwrap();
        assert!(!inspect.contains("show"));
        let response = ControlResponse::PortalEvaluated {
            id: Uuid::nil(),
            value: serde_json::json!({"a": 1}),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["result"], "portal_evaluated");
        assert_eq!(json["value"]["a"], 1);
    }

    #[test]
    fn file_and_git_are_recognized_verbs() {
        assert!(is_cli_verb("file"));
        assert!(is_cli_verb("git"));
    }

    #[test]
    fn file_commands_parse() {
        assert!(matches!(
            parse_cli_request(&args(&["file", "inspect", "@file:src/a.rs"])).unwrap(),
            ControlRequest::FileInspect { reference } if reference == "@file:src/a.rs"
        ));
        let ControlRequest::FileRead { reference, lines } =
            parse_cli_request(&args(&["file", "read", "src/a.rs", "--lines", "3-9"])).unwrap()
        else {
            panic!("expected FileRead");
        };
        assert_eq!(reference, "src/a.rs");
        assert_eq!(lines, Some(LineRange { start: 3, end: 9 }));
        assert!(matches!(
            parse_cli_request(&args(&["file", "list", "src", "--hidden"])).unwrap(),
            ControlRequest::FileList { path: Some(path), hidden: true } if path == "src"
        ));
        assert!(matches!(
            parse_cli_request(&args(&["file", "list"])).unwrap(),
            ControlRequest::FileList {
                path: None,
                hidden: false
            }
        ));
        let ControlRequest::FileSearch {
            query,
            content,
            limit,
            ..
        } = parse_cli_request(&args(&[
            "file",
            "search",
            "--content",
            "fn login",
            "--limit",
            "5",
        ]))
        .unwrap()
        else {
            panic!("expected FileSearch");
        };
        assert_eq!(query, "fn login");
        assert!(content);
        assert_eq!(limit, Some(5));
        assert!(parse_cli_request(&args(&["file", "read", "a", "--bogus"])).is_err());
        assert!(parse_cli_request(&args(&["file", "read", "a", "--lines", "x"])).is_err());
        // A write must say what it's based on (or that it's creating).
        assert!(parse_cli_request(&args(&["file", "write", "a.rs"])).is_err());
        assert!(parse_cli_request(&args(&["file", "search"])).is_err());
    }

    #[test]
    fn git_branch_and_remote_commands_parse() {
        assert!(matches!(
            parse_cli_request(&args(&["git", "stage", "--all"])).unwrap(),
            ControlRequest::GitStageAll
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "unstage", "--all"])).unwrap(),
            ControlRequest::GitUnstageAll
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "branches"])).unwrap(),
            ControlRequest::GitBranches
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "branch", "feature/x"])).unwrap(),
            ControlRequest::GitCreateBranch { branch } if branch == "feature/x"
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "switch", "main"])).unwrap(),
            ControlRequest::GitSwitch { branch } if branch == "main"
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "fetch"])).unwrap(),
            ControlRequest::GitFetch
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "pull"])).unwrap(),
            ControlRequest::GitPull
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "push"])).unwrap(),
            ControlRequest::GitPush {
                set_upstream: false
            }
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "push", "--set-upstream"])).unwrap(),
            ControlRequest::GitPush { set_upstream: true }
        ));
        // No force push, no stray flags.
        assert!(parse_cli_request(&args(&["git", "push", "--force"])).is_err());
        assert!(parse_cli_request(&args(&["git", "pull", "--rebase"])).is_err());
        let fetch = parse_cli_request(&args(&["git", "fetch"])).unwrap();
        assert!(fetch.is_async());
        assert!(fetch.reply_timeout() > REPLY_TIMEOUT);
    }

    #[test]
    fn git_commands_parse() {
        assert!(matches!(
            parse_cli_request(&args(&["git", "status"])).unwrap(),
            ControlRequest::GitStatus
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "diff"])).unwrap(),
            ControlRequest::GitDiff {
                path: None,
                scope: DiffScope::Head
            }
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "diff", "@diff:src/a.rs", "--staged"])).unwrap(),
            ControlRequest::GitDiff {
                path: Some(_),
                scope: DiffScope::Staged
            }
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "log", "-n", "3"])).unwrap(),
            ControlRequest::GitLog {
                path: None,
                limit: Some(3)
            }
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "stage", "a", "b"])).unwrap(),
            ControlRequest::GitStage { paths } if paths == vec!["a", "b"]
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "discard", "a"])).unwrap(),
            ControlRequest::GitDiscard {
                confirmed: false,
                ..
            }
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "discard", "a", "--confirm"])).unwrap(),
            ControlRequest::GitDiscard {
                confirmed: true,
                ..
            }
        ));
        assert!(matches!(
            parse_cli_request(&args(&["git", "commit", "-m", "feat: x"])).unwrap(),
            ControlRequest::GitCommit { message } if message == "feat: x"
        ));
        assert!(parse_cli_request(&args(&["git", "commit"])).is_err());
        assert!(parse_cli_request(&args(&["git", "stage"])).is_err());
        assert!(parse_cli_request(&args(&["git", "rebase"])).is_err());
        assert!(parse_cli_request(&args(&["git", "branch", "a", "b"])).is_err());
    }

    #[test]
    fn notes_attach_and_detach_parse() {
        let id = Uuid::new_v4();
        assert!(matches!(
            parse_cli_request(&args(&["notes", "attach", &id.to_string(), "docs/plan.md"])).unwrap(),
            ControlRequest::NotesAttach { path, .. } if path == "docs/plan.md"
        ));
        assert!(matches!(
            parse_cli_request(&args(&["notes", "detach", &id.to_string()])).unwrap(),
            ControlRequest::NotesDetach { .. }
        ));
    }

    #[test]
    fn file_requests_round_trip_through_json() {
        let request = ControlRequest::FileWrite {
            reference: "src/a.rs".to_string(),
            content: "fn a() {}\n".to_string(),
            revision: Some(FileRevision("abc".to_string())),
            create: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        let ControlRequest::FileWrite {
            revision, content, ..
        } = serde_json::from_str(&json).unwrap()
        else {
            panic!("expected FileWrite");
        };
        assert_eq!(revision, Some(FileRevision("abc".to_string())));
        assert_eq!(content, "fn a() {}\n");
    }
}
