//! Keybind table + dispatch. Plan Step 7 — "Keybind global table".
//!
//! Each KeyEvent maps to an Action; the Action is dispatched against the
//! current View + selection state. Unbound combinations are ignored.

/// Actions emitted by the input layer; consumed by app::App::handle_action.
#[derive(Debug, Clone)]
pub enum Action {
    SwitchView(super::app::View),
    CyclePane,
    CycleArtifactTab,
    MoveDown,
    MoveUp,
    Drill,
    PopBack,

    NewEntity,         // n — context-sensitive (draft / routine)
    NewTaskDirect,     // N
    EditSelected,      // e
    DeleteSingle,      // x
    DeleteConfirm,     // X
    BulkClear,         // D
    NewFollowup,       // f
    RunNow,            // r
    TriageNow,         // T
    ToggleEnabled,     // t
    CompleteTask,      // c
    AttachSession,     // a
    ViewAgentLog,      // v
    Quit,              // q
}

// TODO: pub fn dispatch(key: crossterm::event::KeyEvent, view: View, sel: &Selection) -> Option<Action>
