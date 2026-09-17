use crate::agent::{Agent, Launch};
use portable_pty::{CommandBuilder, PtyPair, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;

pub struct Tab {
    pub name: String,
    pub cwd: PathBuf,
    pub agent: Agent,
    pair: PtyPair,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    parser: vt100::Parser,
    exit_status: Option<portable_pty::ExitStatus>,
}

impl Tab {
    pub fn spawn(
        name: String,
        cwd: PathBuf,
        agent: Agent,
        launch: Launch,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Tab> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
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

        let parser = vt100::Parser::new(rows, cols, 10_000);

        Ok(Tab {
            name,
            cwd,
            agent,
            pair,
            child,
            writer,
            output_rx: rx,
            parser,
            exit_status: None,
        })
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        self.pair.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser.screen_mut().set_size(rows, cols);
        Ok(())
    }

    pub fn pull_output(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.output_rx.try_recv() {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    changed = true;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if self.exit_status.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exit_status = Some(status);
                changed = true;
            }
        }
        changed
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn exit_status(&self) -> Option<&portable_pty::ExitStatus> {
        self.exit_status.as_ref()
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        let _ = self.child.kill();
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
    fn spawn_reads_child_output_into_screen() {
        let mut tab = Tab::spawn(
            "t".to_string(),
            std::env::temp_dir(),
            Agent::Codex,
            echo_hello(),
            24,
            80,
        )
        .unwrap();

        let mut seen = String::new();
        for _ in 0..50 {
            tab.pull_output();
            seen = tab.screen().contents();
            if seen.contains("hello") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            seen.contains("hello"),
            "expected pty output to contain 'hello', got: {seen:?}"
        );
    }

    #[test]
    fn resize_updates_screen_dimensions() {
        let mut tab = Tab::spawn(
            "t".to_string(),
            std::env::temp_dir(),
            Agent::Codex,
            sleeper(),
            24,
            80,
        )
        .unwrap();
        tab.resize(30, 100).unwrap();
        assert_eq!(tab.screen().size(), (30, 100));
    }

    #[test]
    fn pull_output_detects_child_exit() {
        let mut tab = Tab::spawn(
            "t".to_string(),
            std::env::temp_dir(),
            Agent::Codex,
            echo_hello(),
            24,
            80,
        )
        .unwrap();
        let mut status = None;
        for _ in 0..50 {
            tab.pull_output();
            status = tab.exit_status().cloned();
            if status.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(status.is_some(), "expected child to have exited by now");
    }

    #[test]
    fn exit_status_is_none_while_child_is_running() {
        let mut tab = Tab::spawn(
            "t".to_string(),
            std::env::temp_dir(),
            Agent::Codex,
            sleeper(),
            24,
            80,
        )
        .unwrap();
        tab.pull_output();
        assert!(tab.exit_status().is_none());
    }

    #[test]
    fn spawn_with_missing_binary_errors() {
        let launch = Launch {
            program: "duet-does-not-exist-binary".to_string(),
            args: vec![],
            envs: vec![],
        };
        let result = Tab::spawn(
            "t".to_string(),
            std::env::temp_dir(),
            Agent::Codex,
            launch,
            24,
            80,
        );
        assert!(result.is_err());
    }
}
