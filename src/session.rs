//! Owns one agent's PTY and child process. Rendering is the caller's job
//! (a `vte4::Terminal` fed via `try_recv_output`), not this module's.

use crate::agent::Launch;
use portable_pty::{CommandBuilder, PtyPair, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::thread;

const OUTPUT_TAIL_LIMIT: usize = 512;

fn trim_output_tail(text: &mut String) {
    if text.len() <= OUTPUT_TAIL_LIMIT {
        return;
    }
    let mut start = text.len() - OUTPUT_TAIL_LIMIT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    *text = text.split_off(start);
}

pub struct Session {
    pair: PtyPair,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    exit_status: Option<portable_pty::ExitStatus>,
    missing_conversation: bool,
    output_tail: String,
}

impl Session {
    pub fn spawn(cwd: PathBuf, launch: Launch) -> anyhow::Result<Session> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&launch.program);
        cmd.args(&launch.args);
        for (key, value) in &launch.envs {
            cmd.env(key, value);
        }
        cmd.cwd(&cwd);

        let child = pair.slave.spawn_command(cmd)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx) = channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Session {
            pair,
            child,
            writer,
            output_rx: rx,
            exit_status: None,
            missing_conversation: false,
            output_tail: String::new(),
        })
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(2);
        self.pair.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    /// Drains every output chunk received since the last call. The caller
    /// (a `vte4::Terminal`, or a linked session's input) decides what to do
    /// with the bytes; this module only knows about the PTY, not rendering.
    pub fn try_recv_output(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.output_tail
                .push_str(&String::from_utf8_lossy(&chunk).to_ascii_lowercase());
            trim_output_tail(&mut self.output_tail);
            let missing_session = self.output_tail.contains("no conversation found")
                || self.output_tail.contains("session id not found")
                || (self.output_tail.contains("conversation")
                    && self.output_tail.contains("does not exist"));
            if missing_session {
                self.missing_conversation = true;
            }
            chunks.push(chunk);
        }
        if self.exit_status.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exit_status = Some(status);
        }
        chunks
    }

    pub fn exit_status(&self) -> Option<&portable_pty::ExitStatus> {
        self.exit_status.as_ref()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn missing_conversation(&self) -> bool {
        self.missing_conversation
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_hello() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "printf hello".to_string()],
            envs: vec![],
        }
    }

    fn sleeper() -> Launch {
        Launch {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 5".to_string()],
            envs: vec![],
        }
    }

    #[test]
    fn spawn_reads_child_output() {
        let mut session = Session::spawn(std::env::temp_dir(), echo_hello()).unwrap();
        let mut seen = Vec::new();
        for _ in 0..50 {
            seen.extend(session.try_recv_output());
            let joined: Vec<u8> = seen.iter().flatten().copied().collect();
            if String::from_utf8_lossy(&joined).contains("hello") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let joined: Vec<u8> = seen.iter().flatten().copied().collect();
        assert!(
            String::from_utf8_lossy(&joined).contains("hello"),
            "expected pty output to contain 'hello'"
        );
    }

    #[test]
    fn trimming_unicode_output_keeps_a_valid_utf8_boundary() {
        let mut text = "a".repeat(510);
        text.push_str("🦀xy");
        trim_output_tail(&mut text);
        assert!(text.is_char_boundary(0));
        assert!(text.ends_with("🦀xy"));
        assert!(text.len() <= OUTPUT_TAIL_LIMIT);
    }

    #[test]
    fn pull_output_detects_child_exit() {
        let mut session = Session::spawn(std::env::temp_dir(), echo_hello()).unwrap();
        let mut status = None;
        for _ in 0..50 {
            session.try_recv_output();
            status = session.exit_status().cloned();
            if status.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(status.is_some(), "expected child to have exited by now");
    }

    #[test]
    fn exit_status_is_none_while_child_is_running() {
        let mut session = Session::spawn(std::env::temp_dir(), sleeper()).unwrap();
        session.try_recv_output();
        assert!(session.exit_status().is_none());
    }

    #[test]
    fn spawn_with_missing_binary_errors() {
        let launch = Launch {
            program: "duet-does-not-exist-binary".to_string(),
            args: vec![],
            envs: vec![],
        };
        let result = Session::spawn(std::env::temp_dir(), launch);
        assert!(result.is_err());
    }
}
