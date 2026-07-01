//! Rendering. [`draw`] lays out one operator console: tree on the left,
//! contextual detail on the right, and a wsx-style status/footer line.

mod ask;
mod backlog;
mod config;
mod issue;
mod logs;
mod overview;
pub(crate) mod schedule;
pub(crate) mod theme;
pub(crate) mod vm;

use crate::app::{App, FieldKind, Form, View};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

/// Accent for the focused/selected element across all views. Re-exported from
/// [`theme`] so existing `ACCENT` references resolve to the themed color.
pub(crate) use theme::ACCENT;

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

    frame.render_widget(Clear, chunks[0]);
    frame.render_widget(Clear, chunks[1]);
    match app.view {
        View::Overview => overview::render(frame, app, chunks[0]),
        View::Issue => issue::render(frame, app, chunks[0]),
        View::Backlog => backlog::render(frame, app, chunks[0]),
        View::Logs => logs::render(frame, app, chunks[0]),
        View::Config => config::render(frame, app, chunks[0]),
        View::Ask => ask::render(frame, app, chunks[0]),
    }
    draw_footer(frame, app, chunks[1]);
    if app.form.is_some() {
        draw_form(frame, app);
    }
    if app.confirm_quit {
        draw_confirm_quit(frame, app);
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    // Global hints and version stay pinned right; contextual hints use the left.
    let global = format!(
        " (Ctrl-,) config · (q)uit · v{} ",
        env!("CARGO_PKG_VERSION")
    );
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(global.len() as u16)])
        .split(area);

    // A status/error message preempts the hint line when present.
    let (text, style) = if !app.status.is_empty() {
        (app.status.clone(), Style::default().fg(theme::WARN))
    } else {
        (footer_hint(app), theme::hint())
    };
    frame.render_widget(Paragraph::new(Span::styled(text, style)), cols[0]);
    frame.render_widget(Paragraph::new(Span::styled(global, theme::dim())), cols[1]);
}

fn footer_hint(app: &App) -> String {
    let prefix = footer_prefix(app);
    if app.confirm_quit {
        return format!("{prefix}(y/Enter) stop daemon · (n/Esc) cancel · (Ctrl-C) quit only");
    }
    if app.form.is_some() {
        return format!("{prefix}(Tab/Enter) next · (Shift-Tab) previous · (Esc) cancel");
    }
    if app.view == View::Issue {
        let mut parts = vec![
            "(k) older".to_string(),
            "(j) newer".to_string(),
            "PgUp/PgDn page".to_string(),
            "Home oldest".to_string(),
            "End newest".to_string(),
        ];
        parts.extend(app.capabilities().hints.into_iter().map(|hint| hint.label));
        parts.extend(["(Esc) back", "(Tab) view"].map(str::to_string));
        return format!("{prefix}{}", parts.join(" · "));
    }
    if app.view == View::Config {
        let mut parts = vec!["settings".to_string(), "(j/k) select".to_string()];
        parts.extend(app.capabilities().hints.into_iter().map(|hint| hint.label));
        parts.extend(["Pg scroll detail", "(Esc) main"].map(str::to_string));
        return format!("{prefix}{}", parts.join(" · "));
    }
    if app.move_mode {
        return format!(
            "{prefix}move mode · (j/k) local reorder · (h/l) move profile · (m)ove exit · (Esc) cancel"
        );
    }
    if app.focus == crate::app::Focus::ProjectKanban {
        let caps = app.capabilities();
        let mut parts = vec![
            "kanban".to_string(),
            "(h/l) column".to_string(),
            "(j/k) item".to_string(),
            "(Enter) open".to_string(),
        ];
        parts.extend(caps.hints.into_iter().map(|hint| hint.label));
        return format!("{prefix}{} · (Esc) left", parts.join(" · "));
    }
    if app.focus == crate::app::Focus::IssueDetail {
        let mut parts = if app.issue_section_sel == 3 && app.issue_section_interactive {
            vec![
                "log active".to_string(),
                "(j/k) scroll".to_string(),
                "(h/l) section".to_string(),
                "Pg page".to_string(),
            ]
        } else if app.issue_section_sel == 3 {
            vec![
                "issue detail".to_string(),
                "(j/k) section".to_string(),
                "(h/l) section".to_string(),
                "(Enter) scroll log".to_string(),
            ]
        } else {
            vec![
                "issue detail".to_string(),
                "(j/k) section".to_string(),
                "(h/l) section".to_string(),
            ]
        };
        parts.extend(app.capabilities().hints.into_iter().map(|hint| hint.label));
        let esc = if app.issue_section_interactive {
            "(Esc) section"
        } else if app.issue_return_focus == crate::app::Focus::ProjectKanban {
            "(Esc) kanban"
        } else {
            "(Esc) left"
        };
        parts.push(esc.to_string());
        return format!("{prefix}{}", parts.join(" · "));
    }

    let mut parts = footer_parts(app, ["(j/k) move", "([/]) project"]);
    parts.extend(app.capabilities().hints.into_iter().map(|hint| hint.label));
    parts.extend(["(Tab) view", "(Q) stop+quit"].map(str::to_string));
    parts.join(" · ")
}

fn footer_prefix(app: &App) -> &'static str {
    if app.connected {
        ""
    } else {
        "offline · "
    }
}

fn footer_parts<const N: usize>(app: &App, parts: [&str; N]) -> Vec<String> {
    let mut out = Vec::new();
    if !app.connected {
        out.push("offline".to_string());
    }
    out.extend(parts.map(str::to_string));
    out
}

fn draw_form(frame: &mut Frame, app: &App) {
    let Some(form) = app.form.as_ref() else {
        return;
    };
    let suggestions = app.active_suggestions();
    let extra = form_extra_rows(form, suggestions.len()) as u16;
    let area = centered_rect(
        82,
        form_height(form.fields.len() as u16, extra),
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();
    let mut prev_section = "";
    let mut active_field_line = None;
    let mut active_field_prefix_chars = 0usize;
    for (idx, field) in form.fields.iter().enumerate() {
        if field.section != prev_section {
            if !prev_section.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.push(Line::from(Span::styled(
                field.section.to_string(),
                theme::title(),
            )));
            prev_section = field.section;
        }
        let marker = if idx == form.current { ">" } else { " " };
        let optional = if field.optional { " optional" } else { "" };
        let active = idx == form.current;
        let label_style = if active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };
        let label = format!("{marker} {:>18}{optional}: ", field.display);
        if active && field_accepts_cursor(field) {
            active_field_line = Some(lines.len());
            active_field_prefix_chars = label.chars().count();
        }
        let mut row = vec![Span::styled(label, label_style)];
        row.extend(field_value_spans(field, form.cursor, active));
        row.push(Span::styled(
            format!("  {}", field_kind_tag(field)),
            theme::dim(),
        ));
        lines.push(Line::from(row));
        if active && !field.help.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(field.help.to_string(), theme::hint()),
            ]));
        }
    }

    if !suggestions.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Suggestions  j/k choose · Tab accept",
            theme::dim(),
        )));
        let selected = app.selected_suggestion_index();
        for (idx, suggestion) in suggestions.iter().enumerate() {
            let selected_style = if idx == selected {
                Style::default()
                    .fg(theme::HIGHLIGHT_FG)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::TEXT)
            };
            let marker = if idx == selected { ">" } else { " " };
            lines.push(Line::from(vec![
                Span::styled(format!("    {marker} "), selected_style),
                Span::styled(suggestion.clone(), selected_style),
            ]));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Enter next/submit · h/l cycle select · Tab complete · Backspace edit · Esc cancel",
        theme::hint(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border(true))
        .title(Span::styled(format!(" {} ", form.title), theme::title()));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
    if let Some(line_idx) = active_field_line {
        let x = area
            .x
            .saturating_add(1)
            .saturating_add((active_field_prefix_chars + form.cursor) as u16)
            .min(area.x.saturating_add(area.width.saturating_sub(2)));
        let y = area
            .y
            .saturating_add(1)
            .saturating_add(line_idx as u16)
            .min(area.y.saturating_add(area.height.saturating_sub(2)));
        frame.set_cursor_position((x, y));
    }
}

fn field_kind_tag(field: &crate::app::FormField) -> String {
    match &field.kind {
        FieldKind::Text => "text".to_string(),
        FieldKind::Number { unit: Some(unit) } => format!("number · {unit}"),
        FieldKind::Number { unit: None } => "number".to_string(),
        FieldKind::TextArea => "text".to_string(),
        FieldKind::Select { options } => format!("select · {}", options.join("/")),
        FieldKind::Combo { free_text: true } => "autocomplete".to_string(),
        FieldKind::Combo { free_text: false } => "preset".to_string(),
    }
}

fn field_accepts_cursor(field: &crate::app::FormField) -> bool {
    !matches!(
        field.kind,
        FieldKind::Select { .. } | FieldKind::Combo { free_text: false }
    )
}

fn field_value_spans(
    field: &crate::app::FormField,
    cursor: usize,
    active: bool,
) -> Vec<Span<'static>> {
    match &field.kind {
        FieldKind::Select { options } => select_value_spans(&field.value, options, active),
        _ if active => editable_value_spans(&field.value, cursor),
        _ if field.value.is_empty() => vec![Span::styled("(blank)", theme::dim())],
        _ => vec![Span::styled(
            field.value.clone(),
            Style::default().fg(theme::TEXT),
        )],
    }
}

fn select_value_spans(value: &str, options: &[&str], active: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (idx, option) in options.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw(" "));
        }
        let selected = *option == value;
        let style = if selected {
            Style::default()
                .fg(theme::HIGHLIGHT_FG)
                .bg(if active { theme::ACCENT } else { theme::BORDER })
                .add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };
        spans.push(Span::styled(format!(" {option} "), style));
    }
    spans
}

fn editable_value_spans(value: &str, cursor: usize) -> Vec<Span<'static>> {
    let byte_idx = value
        .char_indices()
        .nth(cursor)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len());
    let (before, after) = value.split_at(byte_idx);
    let mut after_chars = after.chars();
    let caret = after_chars.next().unwrap_or(' ');
    let rest = after_chars.collect::<String>();
    vec![
        Span::styled(before.to_string(), Style::default().fg(ACCENT)),
        Span::styled(
            caret.to_string(),
            Style::default()
                .fg(theme::HIGHLIGHT_FG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(rest, Style::default().fg(ACCENT)),
    ]
}

fn form_extra_rows(form: &Form, suggestion_count: usize) -> usize {
    let mut sections = 0;
    let mut prev_section = "";
    for field in &form.fields {
        if field.section != prev_section {
            sections += 1;
            prev_section = field.section;
        }
    }
    let help = form
        .current_field()
        .is_some_and(|field| !field.help.is_empty()) as usize;
    sections + help + suggestion_count + usize::from(suggestion_count > 0) + 3
}

fn draw_confirm_quit(frame: &mut Frame, _app: &App) {
    let area = centered_rect(54, 7, frame.area());
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "Stop the daemon and quit auwsx?",
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "The daemon stops scheduling/running agents.",
            theme::hint(),
        )),
        Line::from(Span::styled(
            "[y] stop daemon + quit    [n/Esc] cancel",
            theme::hint(),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::WARN))
        .title(Span::styled(" Quit ", theme::title()));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn form_height(field_count: u16, extra: u16) -> u16 {
    field_count
        .saturating_add(4)
        .saturating_add(extra)
        .clamp(6, 28)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width.saturating_sub(2)).max(1);
    let h = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

// --- shared helpers used by the view modules --------------------------------

/// Build a bordered, selection-highlighting list and render it statefully.
/// `focused` accents the border + highlight for the active pane.
pub(crate) fn render_list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    items: Vec<ListItem>,
    selected: usize,
    focused: bool,
) {
    let is_empty = items.is_empty();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::border(focused))
                .title(Span::styled(format!(" {title} "), theme::title())),
        )
        .highlight_style(theme::highlight(focused))
        .highlight_symbol("▌");
    let mut state = ListState::default();
    // No phantom highlight on an empty list.
    state.select((!is_empty).then_some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use crate::app::{App, View};
    use crate::ui::draw;
    use auwsx_core::agent::ExitKind;
    use auwsx_core::db::agent_runs::{AgentRun, Role};
    use auwsx_core::db::issues::Issue;
    use auwsx_core::state::IssueStatus;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw_view(view: View, w: u16, h: u16) {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.view = view;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
    }

    fn rendered_empty_app() -> String {
        let app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn rendered_view(view: View, w: u16, h: u16) -> String {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.view = view;
        rendered_app(app, w, h)
    }

    fn rendered_app(app: App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn draw_overview_normal_no_panic() {
        draw_view(View::Overview, 100, 30);
    }

    #[test]
    fn draw_issue_normal_no_panic() {
        draw_view(View::Issue, 100, 30);
    }

    #[test]
    fn draw_issue_renders_phase_reports() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.view = View::Issue;
        app.detail.issue = Some(Issue {
            id: 7,
            project_id: 1,
            title: "archive view".to_string(),
            description: None,
            agent_summary: None,
            progress_report: None,
            result_report: None,
            status: IssueStatus::Working,
            branch: None,
            worktree_path: None,
            agent_session: None,
            review_round: 0,
            conflict_attempts: 0,
            wait_until: None,
            absorbed_into_id: None,
            has_pending_steering: false,
            created_at: 1,
            updated_at: 1,
        });
        app.detail.runs = vec![AgentRun {
            id: 3,
            issue_id: Some(7),
            main_job_id: None,
            role: Role::Work,
            phase: "WORKING".to_string(),
            agent_cmd: "agent".to_string(),
            status_before: Some("WORKING".to_string()),
            status_after: Some("READY_TO_MERGE".to_string()),
            pid: None,
            exit_code: Some(0),
            exit_kind: Some(ExitKind::Exited),
            prompt_path: None,
            log_path: None,
            phase_report: Some("Implemented archive view and verified it.".to_string()),
            spawned_at: 1,
            exited_at: Some(2),
            note: None,
        }];

        let rendered = rendered_app(app, 120, 40);

        assert!(rendered.contains("Phase reports"));
        assert!(rendered.contains("Implemented archive view"));
    }

    #[test]
    fn draw_backlog_normal_no_panic() {
        draw_view(View::Backlog, 100, 30);
    }

    #[test]
    fn draw_logs_normal_no_panic() {
        draw_view(View::Logs, 100, 30);
    }

    #[test]
    fn draw_config_normal_no_panic() {
        draw_view(View::Config, 100, 30);
    }

    #[test]
    fn draw_config_renders_prompt_catalog() {
        assert!(rendered_view(View::Config, 120, 50).contains("Prompt Catalog"));
    }

    #[test]
    fn draw_ask_normal_no_panic() {
        draw_view(View::Ask, 100, 30);
    }

    #[test]
    fn draw_overview_tiny_no_panic() {
        draw_view(View::Overview, 8, 4);
    }

    #[test]
    fn draw_issue_tiny_no_panic() {
        draw_view(View::Issue, 8, 4);
    }

    #[test]
    fn draw_backlog_tiny_no_panic() {
        draw_view(View::Backlog, 8, 4);
    }

    #[test]
    fn draw_logs_tiny_no_panic() {
        draw_view(View::Logs, 8, 4);
    }

    #[test]
    fn draw_config_tiny_no_panic() {
        draw_view(View::Config, 8, 4);
    }

    #[test]
    fn draw_ask_tiny_no_panic() {
        draw_view(View::Ask, 8, 4);
    }

    #[test]
    fn draw_empty_app_renders_tree_title() {
        assert!(rendered_empty_app().contains("auwsx"));
    }

    #[test]
    fn draw_empty_app_renders_project_row() {
        assert!(rendered_empty_app().contains("Project"));
    }
}
