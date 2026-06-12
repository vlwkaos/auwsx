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
    /// Move the tree selection.
    Down,
    Up,
    Drill,
    Back,
    NextView,
    PrevView,
    /// Re-query the daemon for console state.
    Refresh,
    /// Open create/data-entry forms.
    NewProject,
    NewBacklog,
    NewIssue,
    NewSubtask,
    NewSteering,
    EditSelected,
    EditConfig,
    /// Backlog actions.
    Approve,
    Dismiss,
    Triage,
    Execute,
    ToggleRoutine,
}

/// Map one key press to a console [`Action`]. The console is one tree/detail
/// surface, so bindings are intentionally independent of the previous tab view.
pub fn map_key(_view: View, key: KeyEvent) -> Option<Action> {
    // Ctrl-C always quits, regardless of view or any future per-view binding.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Action::Quit);
    }
    if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
        return None;
    }

    Some(match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('a') => Action::NewProject,
        KeyCode::Char('n') => Action::NewBacklog,
        KeyCode::Char('i') => Action::NewIssue,
        KeyCode::Char('s') => Action::NewSubtask,
        KeyCode::Char('f') => Action::NewSteering,
        KeyCode::Char('e') => Action::EditSelected,
        KeyCode::Char('c') => Action::EditConfig,
        KeyCode::Char('A') => Action::Approve,
        KeyCode::Char('x') => Action::Dismiss,
        KeyCode::Char('T') => Action::Triage,
        KeyCode::Char('E') => Action::Execute,
        KeyCode::Char(' ') => Action::ToggleRoutine,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
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
    fn given_r_when_mapped_then_refresh() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('r'))),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn given_a_when_mapped_then_new_project() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('a'))),
            Some(Action::NewProject)
        );
    }

    #[test]
    fn given_n_when_mapped_then_new_backlog() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('n'))),
            Some(Action::NewBacklog)
        );
    }

    #[test]
    fn given_i_when_mapped_then_new_issue() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('i'))),
            Some(Action::NewIssue)
        );
    }

    #[test]
    fn given_s_when_mapped_then_new_subtask() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('s'))),
            Some(Action::NewSubtask)
        );
    }

    #[test]
    fn given_f_when_mapped_then_new_steering() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('f'))),
            Some(Action::NewSteering)
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
    fn given_capital_a_when_mapped_then_approve() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('A'))),
            Some(Action::Approve)
        );
    }

    #[test]
    fn given_x_when_mapped_then_dismiss() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('x'))),
            Some(Action::Dismiss)
        );
    }

    #[test]
    fn given_capital_t_when_mapped_then_triage() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char('T'))),
            Some(Action::Triage)
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
    fn given_space_when_mapped_then_toggle_routine() {
        assert_eq!(
            map_key(View::Overview, key(KeyCode::Char(' '))),
            Some(Action::ToggleRoutine)
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
    fn given_refresh_key_when_view_changes_then_same_action() {
        let views = [
            View::Overview,
            View::Issue,
            View::Backlog,
            View::Logs,
            View::Config,
        ];
        assert!(views
            .into_iter()
            .all(|view| map_key(view, key(KeyCode::Char('r'))) == Some(Action::Refresh)));
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
    fn given_h_when_mapped_then_none() {
        assert_eq!(map_key(View::Overview, key(KeyCode::Char('h'))), None);
    }

    #[test]
    fn given_l_when_mapped_then_none() {
        assert_eq!(map_key(View::Overview, key(KeyCode::Char('l'))), None);
    }

    #[test]
    fn given_uppercase_r_when_mapped_then_none() {
        assert_eq!(map_key(View::Overview, key(KeyCode::Char('R'))), None);
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
