//! Single source of truth for TUI colors. Every `ui/` module must reference
//! these roles — NO inline `Color::X` at call sites (see AGENTS.md). Centralizing
//! here keeps contrast consistent and prevents the regressions this module was
//! created to fix:
//!   - footer/hint text was `DarkGray` and unreadable → [`HINT`] is a legible gray
//!   - dim content shared `DarkGray` with the unfocused border → [`BORDER`] and
//!     [`TEXT_DIM`] are now distinct (structural chrome vs. readable secondary text)
//!
//! Invariant (asserted in tests): `BORDER != TEXT_DIM` and `BORDER != HINT`, so
//! chrome never collides with content.

use ratatui::style::{Color, Modifier, Style};

// --- palette: semantic roles, tuned for a dark terminal ---------------------

/// Focused border, selected-pane accent, project headers, highlight background.
pub const ACCENT: Color = Color::Rgb(94, 185, 247);
/// Unfocused border + structural separators. Deliberately darker than any text.
pub const BORDER: Color = Color::Rgb(72, 78, 90);
/// Tree branch connectors (├ └). Chrome, slightly lifted from [`BORDER`].
pub const TREE_CONNECTOR: Color = Color::Rgb(96, 103, 117);

/// Primary content text.
pub const TEXT: Color = Color::Rgb(220, 222, 228);
/// Secondary/label text — muted but readable (kv keys, separators-with-text).
pub const TEXT_DIM: Color = Color::Rgb(150, 156, 168);
/// Footer hint line. Readable, a touch dimmer than [`TEXT`].
pub const HINT: Color = Color::Rgb(158, 164, 176);

/// Selected-row foreground (sits on an [`ACCENT`] background).
pub const HIGHLIGHT_FG: Color = Color::Rgb(16, 18, 22);

/// Healthy / live / success.
pub const OK: Color = Color::Rgb(126, 200, 121);
/// Caution / pending / status-message.
pub const WARN: Color = Color::Rgb(231, 190, 99);
/// Error / blocker.
pub const ERR: Color = Color::Rgb(229, 115, 115);

// --- finding severity -------------------------------------------------------

/// Color for a finding severity string (`blocker`/`major`/`minor`/`nit`).
pub fn severity(sev: &str) -> Color {
    match sev {
        "blocker" => ERR,
        "major" => Color::Rgb(240, 142, 120),
        "minor" => WARN,
        _ => TEXT_DIM, // nit
    }
}

/// Color for a backlog approval string (`approved`/`dismissed`/`pending`).
pub fn approval(approval: &str) -> Color {
    match approval {
        "approved" => OK,
        "dismissed" => TEXT_DIM,
        _ => WARN, // pending
    }
}

// --- style helpers (compose the roles above) --------------------------------

/// Border style for a pane; `focused` lifts it to the accent color.
pub fn border(focused: bool) -> Style {
    Style::default().fg(if focused { ACCENT } else { BORDER })
}

/// Bold accent title (panel/block titles).
pub fn title() -> Style {
    Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
}

/// Muted secondary text (kv keys, labels).
pub fn dim() -> Style {
    Style::default().fg(TEXT_DIM)
}

/// Footer hint text.
pub fn hint() -> Style {
    Style::default().fg(HINT)
}

/// Selected-row highlight (accent background, dark foreground).
pub fn highlight(focused: bool) -> Style {
    Style::default()
        .add_modifier(Modifier::BOLD)
        .bg(if focused { ACCENT } else { BORDER })
        .fg(HIGHLIGHT_FG)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- chrome must never collide with content text --------------------

    #[test]
    fn given_palette_when_compared_then_border_differs_from_text_dim() {
        assert_ne!(BORDER, TEXT_DIM);
    }

    #[test]
    fn given_palette_when_compared_then_border_differs_from_hint() {
        assert_ne!(BORDER, HINT);
    }

    #[test]
    fn given_palette_when_compared_then_border_differs_from_text() {
        assert_ne!(BORDER, TEXT);
    }

    #[test]
    fn given_palette_when_compared_then_accent_differs_from_border() {
        assert_ne!(ACCENT, BORDER);
    }

    // --- severity --------------------------------------------------------

    #[test]
    fn given_blocker_when_severity_then_err() {
        assert_eq!(severity("blocker"), ERR);
    }

    #[test]
    fn given_minor_when_severity_then_warn() {
        assert_eq!(severity("minor"), WARN);
    }

    #[test]
    fn given_major_when_severity_then_not_err() {
        assert_ne!(severity("major"), ERR);
    }

    #[test]
    fn given_major_when_severity_then_not_warn() {
        assert_ne!(severity("major"), WARN);
    }

    #[test]
    fn given_unknown_string_when_severity_then_text_dim() {
        assert_eq!(severity("nit"), TEXT_DIM);
    }

    // --- approval --------------------------------------------------------

    #[test]
    fn given_approved_when_approval_then_ok() {
        assert_eq!(approval("approved"), OK);
    }

    #[test]
    fn given_dismissed_when_approval_then_text_dim() {
        assert_eq!(approval("dismissed"), TEXT_DIM);
    }

    #[test]
    fn given_unknown_string_when_approval_then_warn() {
        assert_eq!(approval("pending"), WARN);
    }
}
