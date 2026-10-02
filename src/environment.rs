//! What it means to run a terminal's command in a given environment. Two
//! environments exist today: a plain local PTY (today's only behavior, and
//! still the default), and a local tmux-backed one whose whole point is
//! durability — the real shell/agent process lives inside tmux's own
//! long-running server, not as a direct child of `duet`, so it survives
//! `duet` itself exiting and restarting.
//!
//! This module deliberately does NOT duplicate `session.rs`'s PTY-reading/
//! writing/buffering machinery for the tmux case. A tmux-backed session is
//! still spawned, read, written to, resized and killed through exactly the
//! same `Session`/`RuntimeRegistry` path as a plain one (`runtime.rs`
//! remains the only place that owns a live process) — the only thing that
//! differs is *what command* gets run: `tmux new-session -A -s <name> --
//! <program> <args...>` instead of `<program> <args...>` directly. `tmux`'s
//! own `-A` flag ("attach if the session exists, else create it") is what
//! gives durable spawn/attach/reconnect semantics and the "never create a
//! duplicate session" guarantee for free, keyed off a name derived
//! deterministically from the terminal's own stable `Uuid` — no separate
//! bookkeeping of a Duet-id-to-tmux-session-name table is needed because the
//! mapping is a pure function of the id.
//!
//! [`EnvironmentKind`] itself lives in `model.rs`, not here — it's pure
//! persisted data, and this module (which shells out to `tmux`) depends on
//! it, not the other way around.

use crate::agent::Launch;
pub use crate::model::EnvironmentKind;
use crate::runtime::SessionRuntime;
use uuid::Uuid;

/// The deterministic tmux session name for a terminal node's id. Pure
/// function of the id, not stored anywhere separately — this IS the
/// stable Duet-session-to-tmux-session mapping the environment trait needs,
/// and it costs nothing to recompute.
fn tmux_session_name(terminal_id: Uuid) -> String {
    format!("duet-{terminal_id}")
}

/// Builds the `Launch` that should actually be handed to
/// `runtime::RuntimeRegistry::spawn` for `kind`. For `LocalPty` this is
/// `launch` unchanged — today's only behavior, untouched. For `LocalTmux`,
/// wraps it in `tmux new-session -A -s <name> -- <program> <args...>`,
/// which attaches to the terminal's own still-alive tmux session if one
/// already exists (across a `duet` restart, or after `terminate_terminal`
/// detached from it — see that function's doc comment) rather than ever
/// creating a second one.
pub fn prepare_launch(kind: EnvironmentKind, terminal_id: Uuid, launch: Launch) -> Launch {
    match kind {
        EnvironmentKind::LocalPty => launch,
        EnvironmentKind::LocalTmux => {
            let mut args = vec![
                "new-session".to_string(),
                "-A".to_string(),
                "-s".to_string(),
                tmux_session_name(terminal_id),
                "--".to_string(),
                launch.program,
            ];
            args.extend(launch.args);
            Launch {
                program: "tmux".to_string(),
                args,
                envs: launch.envs,
            }
        }
    }
}

/// Best-effort `tmux kill-session` for a terminal that's being genuinely
/// terminated (not just switched away from) or deleted — the difference
/// between "detach" and "terminate" for a tmux-backed terminal. Detaching
/// (killing only `duet`'s own tmux *client* process, which is all
/// `RuntimeRegistry::terminate` ever does, local-PTY or tmux alike) leaves
/// the real shell running inside tmux's server, exactly the point of this
/// environment; a user who explicitly asks to terminate a terminal expects
/// the underlying work to actually stop, so that path additionally reaches
/// past the client to the server itself. Errors (tmux not installed, the
/// session already gone, ...) are deliberately swallowed — this is cleanup,
/// not a user-facing operation in its own right, and a session that's
/// already gone is already the desired end state.
pub fn kill_tmux_session(terminal_id: Uuid) {
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &tmux_session_name(terminal_id)])
        .output();
}

/// Genuinely terminates `terminal_id`'s process: stops `runtime` tracking it
/// (the local-PTY case, and the only thing that happens for a `LocalTmux`
/// terminal that's merely being detached — see `kill_tmux_session`'s doc)
/// and, for `LocalTmux`, additionally kills the underlying tmux session so
/// the real work actually stops. The single call site every genuine-
/// termination path (close a node, delete/unload a workspace, hand off to a
/// different agent, restart a terminal) should use instead of repeating the
/// `runtime.terminate` + conditional `kill_tmux_session` pair inline — one
/// seam to extend if a third backend ever needs its own termination step.
pub fn terminate(runtime: &mut SessionRuntime, terminal_id: Uuid, kind: EnvironmentKind) {
    runtime.terminate(terminal_id);
    if kind == EnvironmentKind::LocalTmux {
        kill_tmux_session(terminal_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_pty_launch_is_unchanged() {
        let launch = Launch {
            program: "claude".to_string(),
            args: vec!["--session-id".to_string(), "abc".to_string()],
            envs: vec![("FOO".to_string(), "bar".to_string())],
        };
        let prepared = prepare_launch(EnvironmentKind::LocalPty, Uuid::nil(), launch.clone());
        assert_eq!(prepared, launch);
    }

    #[test]
    fn local_tmux_wraps_the_command_with_attach_or_create() {
        let id = Uuid::nil();
        let launch = Launch {
            program: "claude".to_string(),
            args: vec!["--session-id".to_string(), "abc".to_string()],
            envs: vec![("FOO".to_string(), "bar".to_string())],
        };
        let prepared = prepare_launch(EnvironmentKind::LocalTmux, id, launch);
        assert_eq!(prepared.program, "tmux");
        assert_eq!(
            prepared.args,
            vec![
                "new-session",
                "-A",
                "-s",
                &tmux_session_name(id),
                "--",
                "claude",
                "--session-id",
                "abc",
            ]
        );
        // Envs pass through unchanged to the wrapped command.
        assert_eq!(prepared.envs, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn tmux_session_name_is_a_pure_function_of_the_id() {
        let id = Uuid::new_v4();
        assert_eq!(tmux_session_name(id), tmux_session_name(id));
        assert_ne!(tmux_session_name(id), tmux_session_name(Uuid::new_v4()));
    }

    #[test]
    fn environment_kind_defaults_to_local_pty() {
        assert_eq!(EnvironmentKind::default(), EnvironmentKind::LocalPty);
    }

    /// `terminate` must stop `runtime` tracking the id for both kinds (the
    /// one guarantee every call site relies on); the `LocalTmux` extra step
    /// (actually killing a real tmux session) isn't exercised here since
    /// tmux isn't assumed to be installed wherever this test suite runs —
    /// `kill_tmux_session`'s own errors are deliberately swallowed, so a
    /// missing `tmux` binary can't make this test flaky either way.
    #[test]
    fn terminate_always_stops_the_runtime_from_tracking_the_id() {
        let mut runtime = SessionRuntime::new();
        let id = Uuid::new_v4();
        runtime
            .spawn(
                id,
                std::env::temp_dir(),
                Launch {
                    program: "sh".to_string(),
                    args: vec!["-c".to_string(), "sleep 5".to_string()],
                    envs: vec![],
                },
            )
            .unwrap();
        assert!(runtime.is_alive(id));
        terminate(&mut runtime, id, EnvironmentKind::LocalPty);
        assert!(!runtime.is_alive(id));

        let id = Uuid::new_v4();
        runtime
            .spawn(
                id,
                std::env::temp_dir(),
                Launch {
                    program: "sh".to_string(),
                    args: vec!["-c".to_string(), "sleep 5".to_string()],
                    envs: vec![],
                },
            )
            .unwrap();
        assert!(runtime.is_alive(id));
        terminate(&mut runtime, id, EnvironmentKind::LocalTmux);
        assert!(!runtime.is_alive(id));
    }
}
