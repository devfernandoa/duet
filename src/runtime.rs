//! Owns every live session's PTY/process handle, independent of the
//! persisted record that describes it (`store.rs::SessionRecord`) and the
//! GTK widget that displays it (`node.rs::SessionNode`). `App` holds one
//! `SessionRuntime` alongside its `sessions: HashMap<Uuid, SessionEntry>` —
//! the two maps share the same `Uuid` keys (a session's identity is that
//! id, not the `Session`/`SessionNode` instance), but neither owns the
//! other.
//!
//! This is a boundary around *what a running session is*: spawn it, send it
//! input, resize it, tell whether it exited, terminate it. It is
//! deliberately not yet a registry that outlives a workspace switch —
//! `App::teardown_active_workspace` still terminates every session when the
//! active workspace changes, exactly as it did before this module existed.
//! See the crate-level note in `app.rs` near `teardown_active_workspace` for
//! what Milestone 2's background workspaces will need to change here (in
//! short: stop terminating on switch, and key a single app-wide
//! `SessionRuntime` by session id only, independent of which workspace is
//! currently showing its widgets).

use crate::agent::Launch;
use crate::session::Session;
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

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
    /// `App::switch_agent`) are expected to `terminate` the old session
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
}
