use crate::agent::Launch;
use anyhow::{Context, Result, bail};
use std::path::Path;

pub fn run_and_capture(launch: &Launch, cwd: &Path) -> Result<String> {
    let mut cmd = launch.to_std_command();
    cmd.current_dir(cwd);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run {}", launch.program))?;
    if !output.status.success() {
        bail!(
            "{} exited with {}: {}",
            launch.program,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        bail!("{} produced no output", launch.program);
    }
    Ok(text)
}

pub fn summarize_claude(
    session_id: uuid::Uuid,
    config_dir: Option<&Path>,
    cwd: &Path,
) -> Result<String> {
    run_and_capture(
        &crate::agent::claude_summarize_launch(session_id, config_dir),
        cwd,
    )
}

pub fn summarize_codex(cwd: &Path) -> Result<String> {
    run_and_capture(&crate::agent::codex_summarize_launch(), cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_and_capture_returns_trimmed_stdout() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "printf 'hello world\\n'".to_string()],
            envs: vec![],
        };
        let result = run_and_capture(&launch, &std::env::temp_dir()).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn run_and_capture_errors_on_nonzero_exit() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 3".to_string()],
            envs: vec![],
        };
        assert!(run_and_capture(&launch, &std::env::temp_dir()).is_err());
    }

    #[test]
    fn run_and_capture_errors_on_empty_output() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "true".to_string()],
            envs: vec![],
        };
        assert!(run_and_capture(&launch, &std::env::temp_dir()).is_err());
    }

    #[test]
    fn run_and_capture_errors_on_missing_binary() {
        let launch = Launch {
            program: "duet-does-not-exist-binary".to_string(),
            args: vec![],
            envs: vec![],
        };
        assert!(run_and_capture(&launch, &std::env::temp_dir()).is_err());
    }

    #[test]
    fn run_and_capture_sets_working_directory() {
        let launch = Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "pwd".to_string()],
            envs: vec![],
        };
        let tmp = std::env::temp_dir();
        let result = run_and_capture(&launch, &tmp).unwrap();
        // Canonicalize both sides: on macOS /tmp is a symlink, and this keeps the
        // assertion honest on any platform without hardcoding a path shape.
        assert_eq!(
            std::fs::canonicalize(result).unwrap(),
            std::fs::canonicalize(&tmp).unwrap()
        );
    }
}
