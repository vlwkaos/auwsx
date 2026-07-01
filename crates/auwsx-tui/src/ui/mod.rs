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

use crate::app::{App, FieldKind, Focus, Form, FormMode, IssueDetailSection, TreeItem, View};
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
    let model = footer_model(app);
    let global = model.global_text();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(global.len().min(area.width as usize) as u16),
        ])
        .split(area);

    // A status/error message preempts the hint line when present.
    let (text, style) = if !app.status.is_empty() {
        (app.status.clone(), Style::default().fg(theme::WARN))
    } else {
        (model.context_text(), theme::hint())
    };
    frame.render_widget(Paragraph::new(Span::styled(text, style)), cols[0]);
    frame.render_widget(Paragraph::new(Span::styled(global, theme::dim())), cols[1]);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FooterModel {
    context: Vec<String>,
    global: Vec<String>,
}

impl FooterModel {
    fn context_text(&self) -> String {
        self.context.join(" · ")
    }

    fn global_text(&self) -> String {
        format!(" {} ", self.global.join(" · "))
    }
}

fn footer_model(app: &App) -> FooterModel {
    let mut model = FooterModel {
        context: Vec::new(),
        global: global_footer_hints(),
    };
    if !app.connected {
        model.context.push("offline".to_string());
    }
    model.context.extend(footer_context(app).hints(app));
    model
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterContext {
    ConfirmQuit,
    Form(FormMode),
    IssueView,
    Config,
    MoveMode {
        project_scope: bool,
    },
    ProjectKanban,
    IssueDetail {
        section: IssueDetailSection,
        active: bool,
        return_to_kanban: bool,
    },
    Overview,
}

impl FooterContext {
    fn from_app(app: &App) -> Self {
        if app.confirm_quit {
            return Self::ConfirmQuit;
        }
        if let Some(form) = app.form.as_ref() {
            return Self::Form(form.mode);
        }
        if app.view == View::Issue {
            return Self::IssueView;
        }
        if app.view == View::Config {
            return Self::Config;
        }
        if app.move_mode {
            return Self::MoveMode {
                project_scope: matches!(app.selected_tree_item(), Some(TreeItem::Project(_))),
            };
        }
        if app.focus == Focus::ProjectKanban {
            return Self::ProjectKanban;
        }
        if app.focus == Focus::IssueDetail {
            return Self::IssueDetail {
                section: app.selected_issue_section(),
                active: app.issue_section_is_active(),
                return_to_kanban: app.issue_return_focus == Focus::ProjectKanban,
            };
        }
        Self::Overview
    }

    fn hints(self, app: &App) -> Vec<String> {
        match self {
            Self::ConfirmQuit => vec![
                key_hint("y/Enter", "stop daemon"),
                key_hint("n/Esc", "cancel"),
                key_hint("Ctrl-C", "quit only"),
            ],
            Self::Form(FormMode::Navigate) => vec![
                "form nav".to_string(),
                key_hint("j/k", "field"),
                key_hint("Enter", "edit"),
                key_hint("E", "submit"),
                key_hint("Esc", "cancel"),
            ],
            Self::Form(FormMode::Edit) => vec![
                "form edit".to_string(),
                key_hint("Tab", "complete"),
                key_hint("Enter", "done"),
                key_hint("Esc", "done"),
            ],
            Self::IssueView => with_capabilities(
                vec![
                    key_hint("k", "older"),
                    key_hint("j", "newer"),
                    key_hint("PgUp/PgDn", "page"),
                    key_hint("Home", "oldest"),
                    key_hint("End", "newest"),
                ],
                app,
                Some(key_hint("Esc", "back")),
            ),
            Self::Config => {
                let mut hints = vec!["settings".to_string(), key_hint("j/k", "select")];
                hints.extend(app.capabilities().hints.into_iter().map(|hint| hint.label));
                hints.extend([key_hint("Pg", "scroll detail"), key_hint("Esc", "main")]);
                hints
            }
            Self::MoveMode { project_scope } => {
                let mut hints = vec!["move mode".to_string(), key_hint("j/k", "reorder")];
                if project_scope {
                    hints.push(key_hint("h/l", "move profile"));
                }
                hints.extend([key_hint("m", "move exit"), key_hint("Esc", "cancel")]);
                hints
            }
            Self::ProjectKanban => with_capabilities(
                vec![
                    "kanban".to_string(),
                    key_hint("h/l", "column"),
                    key_hint("j/k", "item"),
                    key_hint("Enter", "open"),
                ],
                app,
                Some(key_hint("Esc", "left")),
            ),
            Self::IssueDetail {
                section,
                active,
                return_to_kanban,
            } => {
                let mut hints = issue_detail_hints(section, active);
                hints.extend(app.capabilities().hints.into_iter().map(|hint| hint.label));
                hints.push(issue_detail_escape_hint(active, return_to_kanban));
                hints
            }
            Self::Overview => with_capabilities(
                vec![key_hint("j/k", "move"), key_hint("[/]", "project")],
                app,
                None,
            ),
        }
    }
}

fn footer_context(app: &App) -> FooterContext {
    FooterContext::from_app(app)
}

fn with_capabilities(mut hints: Vec<String>, app: &App, trailing: Option<String>) -> Vec<String> {
    hints.extend(app.capabilities().hints.into_iter().map(|hint| hint.label));
    if let Some(trailing) = trailing {
        hints.push(trailing);
    }
    hints
}

fn issue_detail_hints(section: IssueDetailSection, active: bool) -> Vec<String> {
    if section.is_interactive() && active {
        vec![
            format!("{} active", section.title().to_ascii_lowercase()),
            key_hint("j/k", "scroll"),
            key_hint("h/l", "section"),
            key_hint("Pg", "page"),
        ]
    } else if section.is_interactive() {
        vec![
            "issue detail".to_string(),
            key_hint("j/k", "section"),
            key_hint("h/l", "section"),
            key_hint("Enter", "scroll log"),
        ]
    } else {
        vec![
            "issue detail".to_string(),
            key_hint("j/k", "section"),
            key_hint("h/l", "section"),
            key_hint("Enter", "select"),
        ]
    }
}

fn issue_detail_escape_hint(active: bool, return_to_kanban: bool) -> String {
    if active {
        key_hint("Esc", "section")
    } else if return_to_kanban {
        key_hint("Esc", "kanban")
    } else {
        key_hint("Esc", "left")
    }
}

fn global_footer_hints() -> Vec<String> {
    vec![
        key_hint("Tab", "view"),
        key_hint("Ctrl-,", "config"),
        key_hint("q", "quit"),
        key_hint("Q", "stop+quit"),
        format!("v{}", env!("CARGO_PKG_VERSION")),
    ]
}

fn key_hint(key: &str, label: &str) -> String {
    let mut chars = label.chars();
    if key.chars().count() == 1
        && chars
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case(&key.chars().next().unwrap()))
    {
        format!("({key}){}", chars.as_str())
    } else {
        format!("({key}) {label}")
    }
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
        let editing = active && form.mode == crate::app::FormMode::Edit;
        let label_style = if active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };
        let label = format!("{marker} {:>18}{optional}: ", field.display);
        if editing && field_accepts_cursor(field) {
            active_field_line = Some(lines.len());
            active_field_prefix_chars = label.chars().count();
        }
        let mut row = vec![Span::styled(label, label_style)];
        row.extend(field_value_spans(field, form.cursor, editing));
        row.push(Span::styled(
            format!("  {}", field_kind_tag(field)),
            theme::dim(),
        ));
        if active && !field.help.is_empty() {
            row.extend([
                Span::styled("  ·  ", theme::dim()),
                Span::styled(field.help.to_string(), theme::hint()),
            ]);
        }
        lines.push(Line::from(row));
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
    let controls = match form.mode {
        crate::app::FormMode::Navigate => {
            "Navigate: j/k field · Enter edit · h/l cycle select · (E) submit · Esc cancel"
        }
        crate::app::FormMode::Edit => {
            "Edit: type text · Tab complete · Backspace/Delete edit · Enter/Esc done"
        }
    };
    lines.push(Line::from(Span::styled(controls, theme::hint())));

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
    sections + suggestion_count + usize::from(suggestion_count > 0) + 3
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
    use super::{footer_context, footer_model, form_extra_rows, key_hint, FooterContext};
    use crate::app::{App, FieldKind, Focus, Form, FormField, FormKind, IssueDetailSection, View};
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

    #[test]
    fn given_matching_single_key_when_key_hint_then_parenthesizes_initial() {
        assert_eq!(key_hint("m", "move"), "(m)ove");
        assert_eq!(key_hint("E", "execute"), "(E)xecute");
    }

    #[test]
    fn given_nonmatching_or_compound_key_when_key_hint_then_key_is_separate() {
        assert_eq!(key_hint("Ctrl-,", "config"), "(Ctrl-,) config");
        assert_eq!(key_hint("Enter", "open"), "(Enter) open");
    }

    #[test]
    fn given_default_footer_when_modeled_then_context_and_global_hints_are_separate() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.connected = true;

        let model = footer_model(&app);
        let context = model.context_text();
        let global = model.global_text();

        assert!(context.contains("(a)dd project"));
        assert!(!context.contains("(q)uit"));
        assert!(!context.contains("(Tab) view"));
        assert!(global.contains("(q)uit"));
        assert!(global.contains("(Tab) view"));
        assert!(global.contains("(Ctrl-,) config"));
    }

    #[test]
    fn given_ui_state_when_footer_context_requested_then_modal_depth_wins() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.connected = true;
        app.focus = Focus::ProjectKanban;
        app.confirm_quit = true;

        assert_eq!(footer_context(&app), FooterContext::ConfirmQuit);

        app.confirm_quit = false;
        app.form = Some(Form {
            kind: FormKind::Backlog,
            title: "Backlog",
            fields: vec![FormField {
                label: "text",
                display: "Text",
                section: "Content",
                help: "",
                kind: FieldKind::Text,
                value: String::new(),
                optional: false,
            }],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: crate::app::FormMode::Edit,
        });

        assert_eq!(
            footer_context(&app),
            FooterContext::Form(crate::app::FormMode::Edit)
        );
    }

    #[test]
    fn given_issue_detail_state_when_footer_context_requested_then_section_depth_is_preserved() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.connected = true;
        app.focus = Focus::IssueDetail;
        app.issue_section = IssueDetailSection::Log;
        app.issue_section_mode = crate::app::IssueSectionMode::Active;
        app.issue_return_focus = Focus::ProjectKanban;

        assert_eq!(
            footer_context(&app),
            FooterContext::IssueDetail {
                section: IssueDetailSection::Log,
                active: true,
                return_to_kanban: true,
            }
        );
    }

    #[test]
    fn given_issue_detail_sections_when_footer_modeled_then_only_log_advertises_focus() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.connected = true;
        app.focus = Focus::IssueDetail;
        app.issue_section = IssueDetailSection::Findings;

        let findings = footer_model(&app).context_text();
        assert!(findings.contains("(Enter) select"));
        assert!(!findings.contains("scroll log"));

        app.issue_section = IssueDetailSection::Log;
        let log = footer_model(&app).context_text();
        assert!(log.contains("(Enter) scroll log"));
        assert!(!log.contains("(Enter) select"));
    }

    #[test]
    fn given_active_form_help_when_extra_rows_counted_then_help_stays_inline() {
        let mut form = Form {
            kind: FormKind::Project,
            title: "Project",
            fields: vec![
                FormField {
                    label: "repo_path",
                    display: "Repository path",
                    section: "Repository",
                    help: "Path to the git repository.",
                    kind: FieldKind::Text,
                    value: String::new(),
                    optional: false,
                },
                FormField {
                    label: "branch",
                    display: "Default branch",
                    section: "Repository",
                    help: "",
                    kind: FieldKind::Text,
                    value: "main".into(),
                    optional: false,
                },
            ],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: crate::app::FormMode::Navigate,
        };
        let with_help = form_extra_rows(&form, 0);
        form.current = 1;

        assert_eq!(with_help, form_extra_rows(&form, 0));
    }

    #[test]
    fn given_form_mode_when_footer_modeled_then_hints_match_navigation_depth() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.connected = true;
        app.form = Some(Form {
            kind: FormKind::Backlog,
            title: "Backlog",
            fields: vec![FormField {
                label: "text",
                display: "Text",
                section: "Content",
                help: "",
                kind: FieldKind::Text,
                value: String::new(),
                optional: false,
            }],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: crate::app::FormMode::Navigate,
        });

        let nav = footer_model(&app).context_text();
        assert!(nav.contains("form nav"));
        assert!(nav.contains("(Enter) edit"));
        assert!(nav.contains("(E) submit"));

        app.form.as_mut().expect("form").mode = crate::app::FormMode::Edit;
        let edit = footer_model(&app).context_text();
        assert!(edit.contains("form edit"));
        assert!(edit.contains("(Enter) done"));
        assert!(!edit.contains("(E) submit"));
    }
}
