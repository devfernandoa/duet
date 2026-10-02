//! Agent provider enumeration and launch configuration. Each `Agent` variant
//! owns exactly its own command construction, resume behavior, and metadata
//! (a provider concern); nothing here knows about PTYs, GTK, or the canvas —
//! that split is `session.rs`/`node.rs`'s job, unchanged by what kind of
//! agent is running inside.

use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Agent {
    Claude,
    Codex,
    OpenCode,
    Shell,
    /// A user-configured command: `program` plus any fixed `args`, always
    /// launched fresh — no resume/session-id concept, no handoff/summarize
    /// support. The "simple command configuration" this milestone scopes a
    /// custom provider to, not a plugin system.
    Custom {
        program: String,
        args: Vec<String>,
    },
}

impl Agent {
    /// What to show the user — the provider's own name for the built-in
    /// kinds, or the configured program for a custom one (there's no other
    /// name to show).
    pub fn display_name(&self) -> String {
        match self {
            Agent::Claude => "Claude".to_string(),
            Agent::Codex => "Codex".to_string(),
            Agent::OpenCode => "OpenCode".to_string(),
            Agent::Shell => "Shell".to_string(),
            Agent::Custom { program, .. } => program.clone(),
        }
    }

    /// Whether duet can meaningfully hand a session of this agent off to
    /// "the other" agent (summarize the conversation, then relaunch the
    /// other one with that summary) — only Claude and Codex have the
    /// resumable-session-plus-non-interactive-summarize support
    /// `handoff.rs` needs. The other providers are plain interactive
    /// processes with no equivalent hook, so handoff is refused for them
    /// rather than silently doing something meaningless.
    pub fn supports_handoff(&self) -> bool {
        matches!(self, Agent::Claude | Agent::Codex)
    }

    /// Whether this agent uses duet's per-account Claude config-dir
    /// isolation (`account.rs`) — Claude-only; every other provider ignores
    /// accounts entirely.
    pub fn supports_accounts(&self) -> bool {
        matches!(self, Agent::Claude)
    }

    /// Builds the `Launch` for this agent. `request`'s Claude-only fields are
    /// simply ignored by every other provider — one call site per launch
    /// instead of callers re-matching `Agent` themselves to pick which free
    /// function to call.
    pub fn launch(&self, request: LaunchRequest) -> Launch {
        match self {
            Agent::Claude => claude_launch(
                request.claude_session_id.unwrap_or_else(Uuid::new_v4),
                request.resume,
                request.initial_prompt,
                request.claude_config_dir,
            ),
            Agent::Codex => codex_launch(
                request.resume,
                request.initial_prompt,
                request.codex_home_dir,
            ),
            Agent::OpenCode => opencode_launch(),
            Agent::Shell => shell_launch(),
            Agent::Custom { program, args } => custom_launch(program, args),
        }
    }
}

/// What's needed to build a `Launch` for any `Agent`. Fields only some
/// providers consult (Claude's session id/config dir, Codex's home dir) are
/// simply ignored by the providers that don't use them.
#[derive(Debug, Default)]
pub struct LaunchRequest<'a> {
    pub resume: bool,
    pub initial_prompt: Option<&'a str>,
    pub claude_session_id: Option<Uuid>,
    pub claude_config_dir: Option<&'a Path>,
    pub codex_home_dir: Option<&'a Path>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
}

impl Launch {
    pub fn to_std_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args);
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
        cmd
    }
}

pub const SUMMARY_PROMPT: &str =
    "Summarize this session in a few sentences so another agent can continue the work.";

fn claude_config_env(config_dir: Option<&Path>) -> Vec<(String, String)> {
    match config_dir {
        Some(dir) => vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().to_string(),
        )],
        None => Vec::new(),
    }
}

pub fn claude_launch(
    session_id: Uuid,
    resume: bool,
    initial_prompt: Option<&str>,
    config_dir: Option<&Path>,
) -> Launch {
    let mut args = vec![
        if resume { "--resume" } else { "--session-id" }.to_string(),
        session_id.to_string(),
    ];
    if let Some(prompt) = initial_prompt {
        args.push("--".to_string());
        args.push(prompt.to_string());
    }
    Launch {
        program: "claude".to_string(),
        args,
        envs: claude_config_env(config_dir),
    }
}

fn codex_home_env(home_dir: Option<&Path>) -> Vec<(String, String)> {
    match home_dir {
        Some(dir) => vec![("CODEX_HOME".to_string(), dir.to_string_lossy().to_string())],
        None => Vec::new(),
    }
}

/// `home_dir`, when given, isolates this launch's `CODEX_HOME` — Codex's
/// local app-server daemon only tolerates one live interactive session per
/// `CODEX_HOME`, so two concurrent Codex terminals sharing the real
/// `~/.codex` fail with "already running in another app". Isolating by
/// terminal (see `store::default_codex_home_dir`) fixes that at the cost of
/// each new Codex terminal needing its own `codex login` — there is no
/// `CLAUDE_CONFIG_DIR`-style "several concurrent sessions, one shared
/// login" option for Codex today.
pub fn codex_launch(resume: bool, initial_prompt: Option<&str>, home_dir: Option<&Path>) -> Launch {
    let mut args = Vec::new();
    if resume {
        args.push("resume".to_string());
        args.push("--last".to_string());
    }
    if let Some(prompt) = initial_prompt {
        args.push("--".to_string());
        args.push(prompt.to_string());
    }
    Launch {
        program: "codex".to_string(),
        args,
        envs: codex_home_env(home_dir),
    }
}

/// OpenCode has no duet-tracked resumable session concept yet (unlike
/// Claude's pinned session id or even Codex's directory-scoped `--last`), so
/// every launch is simply a fresh interactive start.
pub fn opencode_launch() -> Launch {
    Launch {
        program: "opencode".to_string(),
        args: Vec::new(),
        envs: Vec::new(),
    }
}

/// Launches the user's own shell (`$SHELL`, falling back to `sh` if unset or
/// empty — the standard Unix convention) with no arguments, for a plain
/// terminal with none of the other providers' agent-specific behavior.
pub fn shell_launch() -> Launch {
    let program = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "sh".to_string());
    Launch {
        program,
        args: Vec::new(),
        envs: Vec::new(),
    }
}

/// Launches exactly the user-configured `program`/`args`, unmodified — no
/// resume flag, no initial-prompt injection, since a custom command's own
/// argv is the one thing duet is told to run verbatim.
pub fn custom_launch(program: &str, args: &[String]) -> Launch {
    Launch {
        program: program.to_string(),
        args: args.to_vec(),
        envs: Vec::new(),
    }
}

pub fn claude_summarize_launch(session_id: Uuid, config_dir: Option<&Path>) -> Launch {
    Launch {
        program: "claude".to_string(),
        args: vec![
            "-p".to_string(),
            "--resume".to_string(),
            session_id.to_string(),
            SUMMARY_PROMPT.to_string(),
        ],
        envs: claude_config_env(config_dir),
    }
}

pub fn codex_summarize_launch() -> Launch {
    Launch {
        program: "codex".to_string(),
        args: vec![
            "exec".to_string(),
            "resume".to_string(),
            "--last".to_string(),
            SUMMARY_PROMPT.to_string(),
        ],
        envs: Vec::new(),
    }
}

/// Appends `DUET_SESSION_ID` so a session's own shell can tell `duet agent
/// send`/`duet agent list` (`control.rs`) which live session issued the
/// command. Applied uniformly to every `Launch` after `Agent::launch` builds
/// the rest of its envs, so no provider above needs to know anything about
/// messaging.
pub fn with_session_env(mut launch: Launch, session_id: Uuid) -> Launch {
    launch
        .envs
        .push(("DUET_SESSION_ID".to_string(), session_id.to_string()));
    launch
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use uuid::Uuid;

    #[test]
    fn claude_fresh_launch_pins_session_id() {
        let id = Uuid::nil();
        let launch = claude_launch(id, false, None, None);
        assert_eq!(launch.program, "claude");
        assert_eq!(launch.args, vec!["--session-id", &id.to_string()]);
        assert!(launch.envs.is_empty());
    }

    #[test]
    fn claude_resume_launch_uses_resume_flag() {
        let id = Uuid::nil();
        let launch = claude_launch(id, true, None, None);
        assert_eq!(launch.args, vec!["--resume", &id.to_string()]);
    }

    #[test]
    fn claude_launch_appends_initial_prompt() {
        let id = Uuid::nil();
        let launch = claude_launch(id, true, Some("hello"), None);
        assert_eq!(
            launch.args,
            vec!["--resume", &id.to_string(), "--", "hello"]
        );
    }

    #[test]
    fn claude_launch_injects_config_dir_env() {
        let id = Uuid::nil();
        let dir = Path::new("/tmp/duet-accounts/work");
        let launch = claude_launch(id, false, None, Some(dir));
        assert_eq!(
            launch.envs,
            vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/tmp/duet-accounts/work".to_string()
            )]
        );
    }

    #[test]
    fn codex_fresh_launch_has_no_resume_flags() {
        let launch = codex_launch(false, Some("hi"), None);
        assert_eq!(launch.program, "codex");
        assert_eq!(launch.args, vec!["--", "hi"]);
        assert!(launch.envs.is_empty());
    }

    #[test]
    fn codex_resume_launch_uses_resume_last() {
        let launch = codex_launch(true, Some("hi"), None);
        assert_eq!(launch.args, vec!["resume", "--last", "--", "hi"]);
    }

    #[test]
    fn codex_resume_launch_without_prompt() {
        let launch = codex_launch(true, None, None);
        assert_eq!(launch.args, vec!["resume", "--last"]);
    }

    #[test]
    fn codex_launch_injects_codex_home_env_when_given() {
        let dir = Path::new("/tmp/duet-codex/some-terminal-id");
        let launch = codex_launch(false, None, Some(dir));
        assert_eq!(
            launch.envs,
            vec![(
                "CODEX_HOME".to_string(),
                "/tmp/duet-codex/some-terminal-id".to_string()
            )]
        );
    }

    #[test]
    fn claude_summarize_uses_print_mode_and_resume() {
        let id = Uuid::nil();
        let launch = claude_summarize_launch(id, None);
        assert_eq!(launch.program, "claude");
        assert_eq!(launch.args[0], "-p");
        assert_eq!(launch.args[1], "--resume");
        assert_eq!(launch.args[2], id.to_string());
        assert_eq!(launch.args[3], SUMMARY_PROMPT);
    }

    #[test]
    fn codex_summarize_uses_exec_resume_last() {
        let launch = codex_summarize_launch();
        assert_eq!(launch.program, "codex");
        assert_eq!(
            launch.args,
            vec!["exec", "resume", "--last", SUMMARY_PROMPT]
        );
    }

    #[test]
    fn to_std_command_sets_program_args_and_envs() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "true".to_string()],
            envs: vec![("FOO".to_string(), "bar".to_string())],
        };
        let cmd = launch.to_std_command();
        assert_eq!(cmd.get_program(), "sh");
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("true")]
        );
        assert_eq!(
            cmd.get_envs().collect::<Vec<_>>(),
            vec![(
                std::ffi::OsStr::new("FOO"),
                Some(std::ffi::OsStr::new("bar"))
            )]
        );
    }

    #[test]
    fn with_session_env_appends_duet_session_id() {
        let id = Uuid::nil();
        let launch = with_session_env(
            Launch {
                program: "sh".to_string(),
                args: vec![],
                envs: vec![("FOO".to_string(), "bar".to_string())],
            },
            id,
        );
        assert_eq!(
            launch.envs,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("DUET_SESSION_ID".to_string(), id.to_string()),
            ]
        );
    }

    #[test]
    fn opencode_launch_is_a_plain_fresh_start() {
        let launch = opencode_launch();
        assert_eq!(launch.program, "opencode");
        assert!(launch.args.is_empty());
        assert!(launch.envs.is_empty());
    }

    #[test]
    fn shell_launch_has_no_args_and_a_nonempty_program() {
        let launch = shell_launch();
        assert!(!launch.program.is_empty());
        assert!(launch.args.is_empty());
    }

    #[test]
    fn custom_launch_runs_program_and_args_verbatim() {
        let launch = custom_launch("mytool", &["--flag".to_string(), "value".to_string()]);
        assert_eq!(launch.program, "mytool");
        assert_eq!(launch.args, vec!["--flag", "value"]);
        assert!(launch.envs.is_empty());
    }

    #[test]
    fn display_name_uses_the_provider_name_or_the_custom_program() {
        assert_eq!(Agent::Claude.display_name(), "Claude");
        assert_eq!(Agent::Codex.display_name(), "Codex");
        assert_eq!(Agent::OpenCode.display_name(), "OpenCode");
        assert_eq!(Agent::Shell.display_name(), "Shell");
        assert_eq!(
            Agent::Custom {
                program: "mytool".to_string(),
                args: vec![],
            }
            .display_name(),
            "mytool"
        );
    }

    #[test]
    fn only_claude_and_codex_support_handoff() {
        assert!(Agent::Claude.supports_handoff());
        assert!(Agent::Codex.supports_handoff());
        assert!(!Agent::OpenCode.supports_handoff());
        assert!(!Agent::Shell.supports_handoff());
        assert!(
            !Agent::Custom {
                program: "mytool".to_string(),
                args: vec![],
            }
            .supports_handoff()
        );
    }

    #[test]
    fn only_claude_supports_accounts() {
        assert!(Agent::Claude.supports_accounts());
        assert!(!Agent::Codex.supports_accounts());
        assert!(!Agent::OpenCode.supports_accounts());
        assert!(!Agent::Shell.supports_accounts());
    }

    #[test]
    fn launch_dispatches_to_the_right_provider_builder() {
        let id = Uuid::nil();
        let request = LaunchRequest {
            resume: true,
            claude_session_id: Some(id),
            ..Default::default()
        };
        assert_eq!(Agent::Claude.launch(request).program, "claude");

        let request = LaunchRequest {
            resume: true,
            ..Default::default()
        };
        assert_eq!(Agent::Codex.launch(request).args, vec!["resume", "--last"]);

        assert_eq!(
            Agent::OpenCode.launch(LaunchRequest::default()).program,
            "opencode"
        );

        let custom = Agent::Custom {
            program: "mytool".to_string(),
            args: vec!["--flag".to_string()],
        };
        let launch = custom.launch(LaunchRequest::default());
        assert_eq!(launch.program, "mytool");
        assert_eq!(launch.args, vec!["--flag"]);
    }

    #[test]
    fn claude_launch_via_request_generates_a_session_id_when_none_given() {
        let launch = Agent::Claude.launch(LaunchRequest::default());
        // No session id was supplied, so one was generated — the launch must
        // still pin *some* id, not fall back to resuming nothing.
        assert_eq!(launch.args[0], "--session-id");
        assert!(Uuid::parse_str(&launch.args[1]).is_ok());
    }
}
