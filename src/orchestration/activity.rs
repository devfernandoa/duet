//! Combines `SessionRuntime::activity` (what a session's own PTY/exit status
//! says) with facts only the orchestration layer knows — whether a node for
//! this agent still exists at all, and whether its inbox has messages
//! waiting — into the final `AgentActivity` shown in the UI and `duetctl`.
//! Kept separate from `runtime.rs` deliberately: `runtime.rs` has no concept
//! of nodes or inboxes, and folding this in there would make it depend
//! upward on orchestration, backwards from CLAUDE.md's dependency direction.

use crate::runtime::{AgentActivity, SessionRuntime};
use uuid::Uuid;

/// `exists`: whether a `NodeRecord` for this agent id is still present (the
/// human hasn't deleted it). `inbox_len`: how many messages are still
/// waiting for this agent — see `bus::MessageBus::inbox_len`.
pub fn activity_for(
    id: Uuid,
    exists: bool,
    runtime: &SessionRuntime,
    inbox_len: usize,
) -> AgentActivity {
    if !runtime.is_alive(id) {
        // `runtime.activity` alone can't distinguish "never spawned" from
        // "detached and terminated" — this module can, because it also
        // knows whether the node still exists.
        return if exists {
            AgentActivity::Offline
        } else {
            AgentActivity::Unknown
        };
    }
    let base = runtime.activity(id);
    if inbox_len > 0
        && matches!(
            base,
            AgentActivity::Starting | AgentActivity::Working | AgentActivity::Idle
        )
    {
        return AgentActivity::AwaitingAgent;
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Launch;

    fn sleeper() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 5".to_string()],
            envs: vec![],
        }
    }

    #[test]
    fn a_node_whose_process_never_ran_is_offline_not_unknown() {
        let runtime = SessionRuntime::new();
        let id = Uuid::new_v4();
        assert_eq!(activity_for(id, true, &runtime, 0), AgentActivity::Offline);
        assert_eq!(activity_for(id, false, &runtime, 0), AgentActivity::Unknown);
    }

    #[test]
    fn a_pending_inbox_overrides_starting_working_and_idle() {
        let mut runtime = SessionRuntime::new();
        let id = Uuid::new_v4();
        runtime.spawn(id, std::env::temp_dir(), sleeper()).unwrap();
        assert_eq!(
            activity_for(id, true, &runtime, 1),
            AgentActivity::AwaitingAgent
        );
        assert_eq!(activity_for(id, true, &runtime, 0), AgentActivity::Starting);
        runtime.terminate(id);
    }

    #[test]
    fn a_finished_process_is_reported_regardless_of_inbox_state() {
        let mut runtime = SessionRuntime::new();
        let id = Uuid::new_v4();
        runtime
            .spawn(
                id,
                std::env::temp_dir(),
                Launch {
                    program: "sh".to_string(),
                    args: vec!["-c".to_string(), "exit 0".to_string()],
                    envs: vec![],
                },
            )
            .unwrap();
        for _ in 0..200 {
            runtime.try_recv_output(id);
            if runtime.has_exited(id) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(activity_for(id, true, &runtime, 3), AgentActivity::Finished);
        runtime.terminate(id);
    }
}
