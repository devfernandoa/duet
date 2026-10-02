//! A minimal local control interface: a Unix-domain socket (the smallest
//! IPC primitive that already fits duet's local-first, single-user-desktop
//! scope — no new dependency, no network exposure) that an agent's own
//! shell talks to via `duet agent list`/`duet agent send`, both dispatched
//! from `main.rs` before GTK is ever touched.
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

use crate::app::App;
use crate::message::{AgentMessage, AgentSummary, LinkSummary};
use crate::store::default_control_socket_path;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::rc::Rc;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    List,
    Send {
        #[serde(default)]
        source_session_id: Option<Uuid>,
        target: String,
        content: String,
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
    }
}

/// `duet agent list` / `duet agent send <target> "<message>"` — connects to
/// the running duet instance's control socket, sends one request, prints
/// its answer, and exits. Dispatched from `main.rs` before GTK is touched,
/// so it works from inside a plain agent shell with no display needed.
/// Returns `true` on success; every failure path prints its own message
/// (to stderr) before returning `false`, so the caller only needs to turn
/// that into a process exit code.
pub fn run_agent_cli(args: &[String]) -> bool {
    let request = match parse_cli_request(args) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("duet agent: {error}");
            return false;
        }
    };

    let send_and_read = || -> anyhow::Result<ControlResponse> {
        let path = default_control_socket_path()?;
        let stream = UnixStream::connect(&path).map_err(|error| {
            anyhow::anyhow!(
                "couldn't reach duet at {} ({error}); is duet running?",
                path.display()
            )
        })?;
        let mut payload = serde_json::to_string(&request)?;
        payload.push('\n');
        (&stream).write_all(payload.as_bytes())?;
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply)?;
        Ok(serde_json::from_str(reply.trim_end())?)
    };

    match send_and_read() {
        Ok(response) => {
            let ok = !matches!(response, ControlResponse::Error { .. });
            print_response(&response);
            ok
        }
        Err(error) => {
            eprintln!("duet agent: {error}");
            false
        }
    }
}

fn parse_cli_request(args: &[String]) -> anyhow::Result<ControlRequest> {
    match args {
        [only] if only == "list" => Ok(ControlRequest::List),
        [cmd, target, rest @ ..] if cmd == "send" && !rest.is_empty() => Ok(ControlRequest::Send {
            source_session_id: std::env::var("DUET_SESSION_ID")
                .ok()
                .and_then(|id| Uuid::parse_str(&id).ok()),
            target: target.clone(),
            content: rest.join(" "),
        }),
        _ => anyhow::bail!(r#"usage: duet agent list | duet agent send <agent> "<message>""#),
    }
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
        ControlResponse::Error { error } => {
            eprintln!("duet agent: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_parses_with_no_arguments() {
        assert!(matches!(
            parse_cli_request(&["list".to_string()]).unwrap(),
            ControlRequest::List
        ));
    }

    #[test]
    fn send_joins_trailing_words_into_one_content_string() {
        let args = [
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
    fn send_without_content_is_rejected() {
        assert!(parse_cli_request(&["send".to_string(), "dev".to_string()]).is_err());
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
            ControlRequest::List => panic!("expected Send"),
        }
    }
}
