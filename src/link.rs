//! Pure byte-forwarding logic for agent-to-agent terminal linking: writes a
//! source session's output chunks into a target session's input. No link
//! storage or UI knowledge lives here — that's `app.rs`'s job.

use crate::session::Session;

pub fn forward(chunks: &[Vec<u8>], target: &mut Session) -> std::io::Result<()> {
    for chunk in chunks {
        target.write_input(chunk)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Launch;

    fn cat() -> Launch {
        Launch {
            program: "cat".to_string(),
            args: vec![],
            envs: vec![],
        }
    }

    #[test]
    fn forward_writes_chunks_into_targets_input_and_cat_echoes_them() {
        let mut target = Session::spawn(std::env::temp_dir(), cat()).unwrap();
        forward(&[b"hello\n".to_vec()], &mut target).unwrap();

        let mut seen = Vec::new();
        for _ in 0..50 {
            seen.extend(target.try_recv_output());
            let joined: Vec<u8> = seen.iter().flatten().copied().collect();
            if String::from_utf8_lossy(&joined).contains("hello") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let joined: Vec<u8> = seen.iter().flatten().copied().collect();
        assert!(String::from_utf8_lossy(&joined).contains("hello"));
    }
}
