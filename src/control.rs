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
//! shape `App::pump_output` already uses), which calls `handle_request`
//! against the live `App` and sends the answer back. `App`'s session map
//! and GTK widgets are not `Send`, so this is the one safe way to reach
//! them from a request that arrived on another thread.

use crate::agent::Agent;
use crate::app::App;
use crate::message::{
    AgentInfo, AgentMessage, AgentSummary, ConnectionInfo, LinkSummary, NoteDetail, NoteSummary,
    WhoamiInfo, WorkspaceInfo,
};
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
    Note {
        note: NoteDetail,
    },
    NoteUpdated {
        id: Uuid,
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
            match tx.send(ControlEvent { request, respond }) {
                Ok(()) => answer
                    .recv_timeout(REPLY_TIMEOUT)
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

/// Answers one request against the live `App` — called from `main.rs`'s
/// poll loop, on the GTK main thread.
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
                },
            }
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
    }
}

/// `true` for every first argument `duet`'s pre-GTK dispatch (and
/// `duetctl`'s own `main`) recognizes as a control-CLI invocation rather
/// than "launch the GUI".
pub fn is_cli_verb(verb: &str) -> bool {
    matches!(
        verb,
        "agent" | "agents" | "send" | "connections" | "workspace" | "whoami" | "notes"
    )
}

/// `duetctl <verb> ...` / `duet <verb> ...`: connects to the running duet
/// instance's control socket, sends one request, prints its answer, and
/// returns whether it succeeded. Works from inside a headless agent shell —
/// no display needed.
pub fn run_cli(args: &[String]) -> bool {
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
            print_response(&response);
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
  duetctl notes connections <id>"#;

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
  duetctl notes connections <id>"#;

fn parse_note_id(raw: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(raw).map_err(|_| anyhow::anyhow!("'{raw}' is not a valid note id"))
}

fn parse_notes(args: &[String]) -> anyhow::Result<ControlRequest> {
    match args {
        [only] if only == "list" => Ok(ControlRequest::NotesList {
            requested_by: acting_agent_id(),
        }),
        [cmd, id] if cmd == "read" => Ok(ControlRequest::NotesRead {
            requested_by: acting_agent_id(),
            id: parse_note_id(id)?,
        }),
        [cmd, id] if cmd == "replace" => Ok(ControlRequest::NotesReplace {
            requested_by: acting_agent_id(),
            id: parse_note_id(id)?,
            markdown: read_stdin_to_string()?,
        }),
        [cmd, id] if cmd == "append" => Ok(ControlRequest::NotesAppend {
            requested_by: acting_agent_id(),
            id: parse_note_id(id)?,
            addition: read_stdin_to_string()?,
        }),
        [cmd, id, rest @ ..] if cmd == "patch" => parse_notes_patch(parse_note_id(id)?, rest),
        [cmd, id] if cmd == "connections" => Ok(ControlRequest::NotesConnections {
            id: parse_note_id(id)?,
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
            println!("\n{}", note.markdown);
        }
        ControlResponse::NoteUpdated { id } => println!("updated note {id}"),
        ControlResponse::Error { error } => {
            eprintln!("duetctl: {error}");
        }
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
}
