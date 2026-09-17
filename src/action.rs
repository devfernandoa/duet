use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    NewTab,
    NewCodexTab,
    CloseTab,
    NextTab,
    PrevTab,
    SwitchAgent,
    SwitchAccount,
    RestartTab,
    Quit,
    Forward(Vec<u8>),
}

pub fn map_key(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('t') => return Action::NewTab,
            KeyCode::Char('n') => return Action::NewCodexTab,
            KeyCode::Char('w') => return Action::CloseTab,
            KeyCode::Right => return Action::NextTab,
            KeyCode::Left => return Action::PrevTab,
            KeyCode::Char('a') => return Action::SwitchAgent,
            KeyCode::Char('g') => return Action::SwitchAccount,
            KeyCode::Char('r') => return Action::RestartTab,
            KeyCode::Char('q') => return Action::Quit,
            _ => {}
        }
    }
    Action::Forward(key_to_bytes(key))
}

fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let byte = (c.to_ascii_uppercase() as u8).wrapping_sub(b'@');
                vec![byte]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_t_maps_to_new_tab() {
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::NewTab);
    }

    #[test]
    fn ctrl_n_maps_to_new_codex_tab() {
        let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::NewCodexTab);
    }

    #[test]
    fn ctrl_w_maps_to_close_tab() {
        let key = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::CloseTab);
    }

    #[test]
    fn ctrl_right_and_left_map_to_next_and_prev_tab() {
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
            Action::NextTab
        );
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            Action::PrevTab
        );
    }

    #[test]
    fn ctrl_a_maps_to_switch_agent() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::SwitchAgent);
    }

    #[test]
    fn ctrl_g_maps_to_switch_account() {
        let key = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::SwitchAccount);
    }

    #[test]
    fn ctrl_r_maps_to_restart_tab() {
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::RestartTab);
    }

    #[test]
    fn ctrl_q_maps_to_quit() {
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(map_key(key), Action::Quit);
    }

    #[test]
    fn plain_char_forwards_utf8_bytes() {
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(map_key(key), Action::Forward(vec![b'x']));
    }

    #[test]
    fn enter_forwards_carriage_return() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(map_key(key), Action::Forward(vec![b'\r']));
    }

    #[test]
    fn backspace_forwards_del_byte() {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(map_key(key), Action::Forward(vec![0x7f]));
    }

    #[test]
    fn arrow_keys_forward_escape_sequences() {
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Action::Forward(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn unmodified_char_that_collides_with_a_ctrl_binding_is_not_special() {
        // Plain 't' (no Ctrl) must just be forwarded, not treated as NewTab.
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(map_key(key), Action::Forward(vec![b't']));
    }
}
