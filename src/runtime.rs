//! Owns every live session's PTY/process handle, independent of the
//! persisted record that describes it (`store.rs::WorkspaceRecord`) and the
//! GTK widget that displays it (`node.rs::SessionNode`). `App` holds one
//! `SessionRuntime` alongside its `nodes: HashMap<Uuid, NodeEntry>` — the two
//! maps share the same `Uuid` keys (a session's identity is that id, not the
//! `Session`/`SessionNode` instance), but neither owns the other, and
//! `nodes` only ever holds the *active* workspace's entries while this
//! registry holds every live session regardless of which workspace (if any)
//! currently has it on screen.
//!
//! This is a boundary around *what a running session is*: spawn it, send it
//! input, resize it, tell whether it exited, terminate it. Since Milestone 2
//! it genuinely outlives a workspace switch — `App::detach_active_workspace`
//! removes a workspace's widgets without calling `terminate`, so a session
//! started here keeps running in the background until something explicitly
//! terminates it (`App::teardown_active_workspace` on real deletion,
//! `App::unload_workspace`, `App::close_node`, or one of the explicit
//! terminate/restart terminal actions). `is_alive`/`live_ids` are what let a
//! caller tell "still running in the background" apart from "never spawned
//! or already gone" without guessing from `App::nodes` membership, which
//! only reflects the active workspace.

use crate::agent::Launch;
use crate::session::Session;
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// How recently a session must have produced output to count as `Working`
/// rather than merely `Idle`. A few seconds, not milliseconds: agent CLIs
/// routinely pause between a tool call and its result, and treating that
/// pause as "idle" would make the badge flicker on every turn.
const WORKING_WINDOW_SECS: f64 = 5.0;

/// What's knowable right now about a session's agent. `Starting`/`Working`/
/// `Idle` come from real, directly-observed PTY activity (see
/// `SessionRuntime::seconds_since_output`); `Finished`/`Failed` come from a
/// real exit status; `Offline` and `AwaitingAgent` are computed one layer up
/// in `orchestration::activity`, which also knows about node existence and
/// inbox state that this module doesn't. `AwaitingUser` has no reliable
/// signal yet (it would need parsing terminal content, which this milestone
/// explicitly doesn't do) and so is never actually produced — kept
/// representable rather than removed, the same "Unknown is better than false
/// confidence" reasoning this enum has used since Milestone 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActivity {
    Unknown,
    Starting,
    Idle,
    Working,
    AwaitingUser,
    AwaitingAgent,
    Finished,
    Failed,
    Offline,
}

/// The live half of a session: not persisted, not a GTK widget, just "is
/// there a process running, and what does its PTY say". Keyed by the same
/// id a `SessionRecord`/`SessionEntry` uses.
#[derive(Default)]
pub struct SessionRuntime {
    sessions: HashMap<Uuid, Session>,
}

impl SessionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns a process under `id` and tracks it. Replaces whatever was
    /// previously tracked under `id` with no explicit termination of the
    /// old one first — callers that are relaunching under the same id (see
    /// `App::hand_off`) are expected to `terminate` the old session
    /// first, same as before this module existed.
    pub fn spawn(&mut self, id: Uuid, cwd: PathBuf, launch: Launch) -> anyhow::Result<()> {
        let session = Session::spawn(cwd, launch)?;
        self.sessions.insert(id, session);
        Ok(())
    }

    /// Writes to `id`'s PTY input. `false` when `id` names no live session
    /// (already terminated, or never spawned) or the write itself failed —
    /// callers that previously matched on `Option<&mut SessionEntry>` before
    /// calling `Session::write_input` now get the same "did nothing happen"
    /// signal collapsed into one bool, since neither case had a different
    /// response.
    pub fn write_input(&mut self, id: Uuid, bytes: &[u8]) -> bool {
        self.sessions
            .get_mut(&id)
            .is_some_and(|session| session.write_input(bytes).is_ok())
    }

    /// Resizes `id`'s PTY to a `cols`x`rows` character grid. `false` when
    /// `id` names no live session or the resize itself failed.
    pub fn resize(&mut self, id: Uuid, rows: u16, cols: u16) -> bool {
        self.sessions
            .get_mut(&id)
            .is_some_and(|session| session.resize(rows, cols).is_ok())
    }

    /// Drains output received since the last call. Empty when `id` names no
    /// live session, the same as a session that simply produced no output.
    pub fn try_recv_output(&mut self, id: Uuid) -> Vec<Vec<u8>> {
        self.sessions
            .get_mut(&id)
            .map(Session::try_recv_output)
            .unwrap_or_default()
    }

    /// Whether `id`'s process has exited. `false` (not an error) when `id`
    /// names no live session — indistinguishable from "still running" to a
    /// caller that only cares about showing an "exited" badge.
    pub fn has_exited(&self, id: Uuid) -> bool {
        self.sessions
            .get(&id)
            .is_some_and(|session| session.exit_status().is_some())
    }

    /// Whether `id` names a session this registry is currently tracking,
    /// alive or exited-but-not-yet-terminated. `materialize_node` uses this
    /// to tell "this workspace's terminal is already running in the
    /// background, just reattach a widget to it" apart from "spawn a fresh
    /// process" — the core of switching workspaces without killing anything.
    pub fn is_alive(&self, id: Uuid) -> bool {
        self.sessions.contains_key(&id)
    }

    /// Every id this registry is currently tracking, regardless of which
    /// workspace (if any) has it visible. Used to drain backgrounded
    /// sessions' output so their PTY's output channel doesn't grow without
    /// bound while nobody's watching — see `App::pump_output`.
    pub fn live_ids(&self) -> Vec<Uuid> {
        self.sessions.keys().copied().collect()
    }

    /// Seconds since `id`'s session last produced PTY output, refreshed only
    /// as a side effect of `try_recv_output` (same caveat as `has_exited`).
    /// `None` when `id` names no live session, or that session hasn't
    /// produced output yet.
    pub fn seconds_since_output(&self, id: Uuid) -> Option<f64> {
        self.sessions
            .get(&id)
            .and_then(Session::seconds_since_output)
    }

    /// What's knowable about `id`'s agent from its own PTY/exit status
    /// alone — see [`AgentActivity`]'s doc comment for the full picture,
    /// which also folds in node existence and inbox state one layer up.
    /// `Unknown` (not `Offline`) when `id` names no tracked session at all:
    /// this module alone can't tell "never spawned" from "detached and
    /// terminated", which is exactly why `orchestration::activity` exists.
    pub fn activity(&self, id: Uuid) -> AgentActivity {
        let Some(session) = self.sessions.get(&id) else {
            return AgentActivity::Unknown;
        };
        if let Some(status) = session.exit_status() {
            return if status.success() {
                AgentActivity::Finished
            } else {
                AgentActivity::Failed
            };
        }
        match session.seconds_since_output() {
            None => AgentActivity::Starting,
            Some(secs) if secs <= WORKING_WINDOW_SECS => AgentActivity::Working,
            Some(_) => AgentActivity::Idle,
        }
    }

    /// Kills `id`'s process (if any) and stops tracking it. A no-op when
    /// `id` names no live session — every call site that used to do
    /// `if let Some(entry) = ... { entry.session.kill(); }` collapses to an
    /// unconditional call here.
    pub fn terminate(&mut self, id: Uuid) {
        if let Some(mut session) = self.sessions.remove(&id) {
            session.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleeper() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 5".to_string()],
            envs: vec![],
        }
    }

    #[test]
    fn operations_on_an_unknown_id_are_harmless_no_ops() {
        let mut runtime = SessionRuntime::new();
        let id = Uuid::new_v4();
        assert!(!runtime.write_input(id, b"hi"));
        assert!(!runtime.resize(id, 24, 80));
        assert!(runtime.try_recv_output(id).is_empty());
        assert!(!runtime.has_exited(id));
        runtime.terminate(id); // must not panic
    }

    #[test]
    fn spawn_write_and_terminate_round_trip() {
        let mut runtime = SessionRuntime::new();
        let id = Uuid::new_v4();
        runtime.spawn(id, std::env::temp_dir(), sleeper()).unwrap();
        assert!(!runtime.has_exited(id));
        assert!(runtime.write_input(id, b"\n"));

        runtime.terminate(id);
        // Terminated: the id is no longer tracked, so every operation reads
        // exactly like "never spawned" rather than "spawned but dead".
        assert!(!runtime.write_input(id, b"\n"));
        assert!(!runtime.has_exited(id));
    }

    #[test]
    fn spawning_under_an_id_that_already_has_a_session_replaces_it() {
        let mut runtime = SessionRuntime::new();
        let id = Uuid::new_v4();
        runtime.spawn(id, std::env::temp_dir(), sleeper()).unwrap();
        runtime.spawn(id, std::env::temp_dir(), sleeper()).unwrap();
        assert!(runtime.write_input(id, b"\n"));
        runtime.terminate(id);
    }

    /// The exact predicate `materialize_node` relies on to decide "reattach
    /// to the already-running process" vs. "spawn a fresh one" when a
    /// workspace comes back from the background.
    #[test]
    fn is_alive_and_live_ids_reflect_tracked_sessions_independent_of_any_workspace() {
        let mut runtime = SessionRuntime::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert!(!runtime.is_alive(a));

        runtime.spawn(a, std::env::temp_dir(), sleeper()).unwrap();
        runtime.spawn(b, std::env::temp_dir(), sleeper()).unwrap();
        assert!(runtime.is_alive(a));
        assert!(runtime.is_alive(b));
        let mut ids = runtime.live_ids();
        ids.sort();
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(ids, expected);

        runtime.terminate(a);
        assert!(!runtime.is_alive(a));
        assert!(runtime.is_alive(b));
        assert_eq!(runtime.live_ids(), vec![b]);
        runtime.terminate(b);
    }

    #[test]
    fn activity_is_unknown_until_exit_then_reflects_success_or_failure() {
        let mut runtime = SessionRuntime::new();
        let id = Uuid::new_v4();
        assert_eq!(runtime.activity(id), AgentActivity::Unknown); // never spawned
        runtime.spawn(id, std::env::temp_dir(), sleeper()).unwrap();
        assert_eq!(runtime.activity(id), AgentActivity::Starting); // alive, no output yet

        let ok = Uuid::new_v4();
        runtime
            .spawn(
                ok,
                std::env::temp_dir(),
                Launch {
                    program: "sh".to_string(),
                    args: vec!["-c".to_string(), "exit 0".to_string()],
                    envs: vec![],
                },
            )
            .unwrap();
        let fail = Uuid::new_v4();
        runtime
            .spawn(
                fail,
                std::env::temp_dir(),
                Launch {
                    program: "sh".to_string(),
                    args: vec!["-c".to_string(), "exit 1".to_string()],
                    envs: vec![],
                },
            )
            .unwrap();
        // Give both short-lived processes a moment to actually exit. Exit
        // status is only refreshed as a side effect of `try_recv_output`
        // (see `Session::try_recv_output`'s `child.try_wait()` call), so the
        // poll loop has to drive that, not just read `has_exited`.
        for _ in 0..200 {
            runtime.try_recv_output(ok);
            runtime.try_recv_output(fail);
            if runtime.has_exited(ok) && runtime.has_exited(fail) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(runtime.activity(ok), AgentActivity::Finished);
        assert_eq!(runtime.activity(fail), AgentActivity::Failed);
        runtime.terminate(id);
        runtime.terminate(ok);
        runtime.terminate(fail);
    }
}
