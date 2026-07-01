//! Keybind table. [`map_key`] is pure (view + key → optional [`Action`]) so the
//! whole grammar is unit-testable without a terminal; the async loop in
//! [`crate::app`] interprets the returned action against live state.

use crate::app::View;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A semantic action decoded from one key press. `None` from [`map_key`] means
/// the key is unbound in the current view (ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    /// Shift+Q: stop the daemon, then quit (opens a confirm popup first).
    QuitWithDaemon,
    /// Move the active cursor, or scroll an active issue-detail log section.
    Down,
    Up,
    Left,
    Right,
    PageDown,
    PageUp,
    Top,
    Bottom,
    Drill,
    Back,
    NextView,
    PrevView,
    /// Open create/data-entry forms.
    Add,
    EditSelected,
    Ask,
    Settings,
    RemoteConfig,
    MoveMode,
    PrevProject,
    NextProject,
    /// Context actions.
    ApproveOrToggle,
    DeleteSelected,
    Execute,
}

/// Map one key press to a console [`Action`]. The console is one tree/detail
/// surface, so bindings are intentionally independent of the previous tab view.
pub fn map_key(_view: View, key: KeyEvent) -> Option<Action> {
    // Ctrl-C always quits, regardless of view or any future per-view binding.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Action::Quit);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(',') {
        return Some(Action::Settings);
    }
    if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
        return None;
    }

    Some(match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('Q') => Action::QuitWithDaemon,
        KeyCode::Char('a') => Action::Add,
        KeyCode::Char('A') => Action::ApproveOrToggle,
        KeyCode::Char('?') => Action::Ask,
        KeyCode::Char('e') => Action::EditSelected,
        KeyCode::Char('R') => Action::RemoteConfig,
        KeyCode::Char('m') => Action::MoveMode,
        KeyCode::Char('d') => Action::DeleteSelected,
        KeyCode::Char('E') => Action::Execute,
        KeyCode::Char('[') => Action::PrevProject,
        KeyCode::Char(']') => Action::NextProject,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Left | KeyCode::Char('h') => Action::Left,
        KeyCode::Right | KeyCode::Char('l') => Action::Right,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::Home => Action::Top,
        KeyCode::End => Action::Bottom,
        KeyCode::Tab => Action::NextView,
        KeyCode::BackTab => Action::PrevView,
        KeyCode::Enter => Action::Drill,
        KeyCode::Esc => Action::Back,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn modified(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn given_ctrl_c_when_mapped_then_quit() {
        assert_eq!(
            map_key(
                View::Overview,
                modified(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(Action::Quit)
        );
    }

    #[test]
    fn given_q_when_mapped_then_quit() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn given_a_when_mapped_then_add() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('a'))),
            Some(Action::Add)
        );
    }

    #[test]
    fn given_capital_a_when_mapped_then_approve_or_toggle() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('A'))),
            Some(Action::ApproveOrToggle)
        );
    }

    #[test]
    fn given_question_mark_when_mapped_then_ask() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('?'))),
            Some(Action::Ask)
        );
    }

    #[test]
    fn given_e_when_mapped_then_edit_selected() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('e'))),
            Some(Action::EditSelected)
        );
    }

    #[test]
    fn given_ctrl_comma_when_mapped_then_settings() {
        assert_eq!(
            map_key(
                View::Overview,
                modified(KeyCode::Char(','), KeyModifiers::CONTROL)
            ),
            Some(Action::Settings)
        );
    }

    #[test]
    fn given_d_when_mapped_then_delete_selected() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('d'))),
            Some(Action::DeleteSelected)
        );
    }

    #[test]
    fn given_brackets_when_mapped_then_project_jump() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('['))),
            Some(Action::PrevProject)
        );
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char(']'))),
            Some(Action::NextProject)
        );
    }

    #[test]
    fn given_m_when_mapped_then_move_mode() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('m'))),
            Some(Action::MoveMode)
        );
    }

    #[test]
    fn given_capital_e_when_mapped_then_execute() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('E'))),
            Some(Action::Execute)
        );
    }

    #[test]
    fn given_down_arrow_when_mapped_then_down() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Down)),
            Some(Action::Down)
        );
    }

    #[test]
    fn given_j_when_mapped_then_down() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('j'))),
            Some(Action::Down)
        );
    }

    #[test]
    fn given_up_arrow_when_mapped_then_up() {
        assert_eq!(map_key(View::Overview, key(KeyCode::Up)), Some(Action::Up));
    }

    #[test]
    fn given_k_when_mapped_then_up() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('k'))),
            Some(Action::Up)
        );
    }

    #[test]
    fn given_page_down_when_mapped_then_page_down() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::PageDown)),
            Some(Action::PageDown)
        );
    }

    #[test]
    fn given_page_up_when_mapped_then_page_up() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::PageUp)),
            Some(Action::PageUp)
        );
    }

    #[test]
    fn given_home_when_mapped_then_top() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Home)),
            Some(Action::Top)
        );
    }

    #[test]
    fn given_end_when_mapped_then_bottom() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::End)),
            Some(Action::Bottom)
        );
    }

    #[test]
    fn given_enter_when_mapped_then_drill() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Enter)),
            Some(Action::Drill)
        );
    }

    #[test]
    fn given_escape_when_mapped_then_back() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Esc)),
            Some(Action::Back)
        );
    }

    #[test]
    fn given_z_when_mapped_then_none() {
        assert_eq!(map_key(View::Overview, key(KeyCode::Char('z'))), None);
    }

    #[test]
    fn given_tab_when_mapped_then_next_view() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Tab)),
            Some(Action::NextView)
        );
    }

    #[test]
    fn given_backtab_when_mapped_then_prev_view() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::BackTab)),
            Some(Action::PrevView)
        );
    }

    #[test]
    fn given_digit_when_mapped_then_none() {
        assert_eq!(map_key(View::Overview, key(KeyCode::Char('1'))), None);
    }

    #[test]
    fn given_h_when_mapped_then_left() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('h'))),
            Some(Action::Left)
        );
    }

    #[test]
    fn given_l_when_mapped_then_right() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('l'))),
            Some(Action::Right)
        );
    }

    #[test]
    fn given_uppercase_r_when_mapped_then_remote_config() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('R'))),
            Some(Action::RemoteConfig)
        );
    }

    #[test]
    fn given_removed_user_facing_keys_when_mapped_then_none() {
        for code in [
            KeyCode::Char('r'),
            KeyCode::Char('i'),
            KeyCode::Char('s'),
            KeyCode::Char('f'),
            KeyCode::Char('T'),
            KeyCode::Char('p'),
            KeyCode::Char('n'),
            KeyCode::Char('S'),
            KeyCode::Char('x'),
            KeyCode::Char(' '),
        ] {
            assert_eq!(map_key(View::Overview, key(code)), None);
        }
    }

    #[test]
    fn given_capital_q_when_mapped_then_quit_with_daemon() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('Q'))),
            Some(Action::QuitWithDaemon)
        );
    }

    #[test]
    fn given_ctrl_q_when_mapped_then_none() {
        assert_eq!(
            map_key(
                View::Overview,
                modified(KeyCode::Char('q'), KeyModifiers::CONTROL)
            ),
            None
        );
    }
}
