//! Agent enumeration and launch configuration for claude and codex CLI tools.

use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Agent {
    Claude,
    Codex,
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
        args.push(prompt.to_string());
    }
    Launch {
        program: "claude".to_string(),
        args,
        envs: claude_config_env(config_dir),
    }
}

pub fn codex_launch(resume: bool, initial_prompt: Option<&str>) -> Launch {
    let mut args = Vec::new();
    if resume {
        args.push("resume".to_string());
        args.push("--last".to_string());
    }
    if let Some(prompt) = initial_prompt {
        args.push(prompt.to_string());
    }
    Launch {
        program: "codex".to_string(),
        args,
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
        assert_eq!(launch.args, vec!["--resume", &id.to_string(), "hello"]);
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
        let launch = codex_launch(false, Some("hi"));
        assert_eq!(launch.program, "codex");
        assert_eq!(launch.args, vec!["hi"]);
    }

    #[test]
    fn codex_resume_launch_uses_resume_last() {
        let launch = codex_launch(true, Some("hi"));
        assert_eq!(launch.args, vec!["resume", "--last", "hi"]);
    }

    #[test]
    fn codex_resume_launch_without_prompt() {
        let launch = codex_launch(true, None);
        assert_eq!(launch.args, vec!["resume", "--last"]);
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
}
