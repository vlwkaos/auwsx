//! Rendering. [`draw`] lays out one operator console: tree on the left,
//! contextual detail on the right, and a wsx-style status/footer line.

mod ask;
mod config;
mod issue;
mod logs;
mod overview;
pub(crate) mod schedule;
pub(crate) mod theme;
pub(crate) mod vm;

use crate::app::{
    format_key_hint as key_hint, App, FieldKind, Focus, Form, FormMode, IssueDetailSection,
    ProjectDetailSection, TreeItem, View,
};
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
    context: Vec<FooterHint>,
    global: Vec<FooterHint>,
}

impl FooterModel {
    fn context_text(&self) -> String {
        render_footer_hints(&self.context)
    }

    fn global_text(&self) -> String {
        format!(" {} ", render_footer_hints(&self.global))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FooterHint {
    Text(String),
    Key { key: String, label: String },
}

impl FooterHint {
    fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    fn key(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self::Key {
            key: key.into(),
            label: label.into(),
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Key { key, label } => key_hint(key, label),
        }
    }
}

fn render_footer_hints(hints: &[FooterHint]) -> String {
    hints
        .iter()
        .map(FooterHint::render)
        .collect::<Vec<_>>()
        .join(" · ")
}

fn footer_model(app: &App) -> FooterModel {
    let mut model = FooterModel {
        context: Vec::new(),
        global: global_footer_hints(),
    };
    if !app.connected {
        model.context.push(FooterHint::text("offline"));
    }
    model.context.extend(footer_context(app).hints(app));
    model
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterContext {
    ConfirmQuit,
    Form(FormMode),
    Config,
    MoveMode {
        project_scope: bool,
    },
    ProjectDetail(ProjectDetailSection),
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
        if app.view == View::Config {
            return Self::Config;
        }
        if app.move_mode {
            return Self::MoveMode {
                project_scope: matches!(app.selected_tree_item(), Some(TreeItem::Project(_))),
            };
        }
        if app.focus == Focus::ProjectDetail {
            return Self::ProjectDetail(app.selected_project_section());
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

    fn hints(self, app: &App) -> Vec<FooterHint> {
        match self {
            Self::ConfirmQuit => vec![
                FooterHint::key("y/Enter", "stop daemon"),
                FooterHint::key("n/Esc", "cancel"),
                FooterHint::key("Ctrl-C", "quit only"),
            ],
            Self::Form(FormMode::Navigate) => vec![
                FooterHint::text("form nav"),
                FooterHint::key("j/k", "field"),
                FooterHint::key("Enter", "edit"),
                FooterHint::key("E", "submit"),
                FooterHint::key("Esc", "cancel"),
            ],
            Self::Form(FormMode::Edit) => vec![
                FooterHint::text("form edit"),
                FooterHint::key("Tab", "complete"),
                FooterHint::key("Enter", "done"),
                FooterHint::key("Esc", "done"),
            ],
            Self::Config => {
                let mut hints = vec![
                    FooterHint::text("settings"),
                    FooterHint::key("j/k", "select"),
                ];
                hints.extend(
                    app.capabilities()
                        .hints
                        .into_iter()
                        .map(|hint| FooterHint::key(hint.key, hint.label)),
                );
                hints.extend([
                    FooterHint::key("Pg", "scroll detail"),
                    FooterHint::key("Esc", "main"),
                ]);
                hints
            }
            Self::MoveMode { project_scope } => {
                let mut hints = vec![
                    FooterHint::text("move mode"),
                    FooterHint::key("j/k", "reorder"),
                ];
                if project_scope {
                    hints.push(FooterHint::key("h/l", "move profile"));
                }
                hints.extend([
                    FooterHint::key("m", "move exit"),
                    FooterHint::key("Esc", "cancel"),
                ]);
                hints
            }
            Self::ProjectDetail(section) => with_capabilities(
                vec![
                    FooterHint::text(format!("project {}", section.title().to_ascii_lowercase())),
                    FooterHint::key("j/k", "section"),
                    FooterHint::key("h/l", "section"),
                ],
                app,
                Some(FooterHint::key("Esc", "left")),
            ),
            Self::ProjectKanban => with_capabilities(
                vec![
                    FooterHint::text("kanban"),
                    FooterHint::key("h/l", "column"),
                    FooterHint::key("j/k", "item"),
                ],
                app,
                Some(FooterHint::key("Esc", "left")),
            ),
            Self::IssueDetail {
                section,
                active,
                return_to_kanban,
            } => {
                let mut hints = issue_detail_hints(section, active);
                hints.extend(
                    app.capabilities()
                        .hints
                        .into_iter()
                        .map(|hint| FooterHint::key(hint.key, hint.label)),
                );
                hints.push(issue_detail_escape_hint(active, return_to_kanban));
                hints
            }
            Self::Overview => with_capabilities(
                vec![
                    FooterHint::key("j/k", "move"),
                    FooterHint::key("[/]", "project"),
                ],
                app,
                None,
            ),
        }
    }
}

fn footer_context(app: &App) -> FooterContext {
    FooterContext::from_app(app)
}

fn with_capabilities(
    mut hints: Vec<FooterHint>,
    app: &App,
    trailing: Option<FooterHint>,
) -> Vec<FooterHint> {
    hints.extend(
        app.capabilities()
            .hints
            .into_iter()
            .map(|hint| FooterHint::key(hint.key, hint.label)),
    );
    if let Some(trailing) = trailing {
        hints.push(trailing);
    }
    hints
}

fn issue_detail_hints(section: IssueDetailSection, active: bool) -> Vec<FooterHint> {
    if section.is_interactive() && active {
        vec![
            FooterHint::text(format!("{} active", section.title().to_ascii_lowercase())),
            FooterHint::key("j/k", "scroll"),
            FooterHint::key("h/l", "section"),
            FooterHint::key("Pg", "page"),
        ]
    } else if section.is_interactive() {
        vec![
            FooterHint::text("issue detail"),
            FooterHint::key("j/k", "section"),
            FooterHint::key("h/l", "section"),
            FooterHint::key("Enter", "scroll log"),
        ]
    } else {
        vec![
            FooterHint::text("issue detail"),
            FooterHint::key("j/k", "section"),
            FooterHint::key("h/l", "section"),
        ]
    }
}

fn issue_detail_escape_hint(active: bool, return_to_kanban: bool) -> FooterHint {
    if active {
        FooterHint::key("Esc", "section")
    } else if return_to_kanban {
        FooterHint::key("Esc", "kanban")
    } else {
        FooterHint::key("Esc", "left")
    }
}

fn global_footer_hints() -> Vec<FooterHint> {
    vec![
        FooterHint::key("Tab", "view"),
        FooterHint::key("Ctrl-,", "config"),
        FooterHint::key("q", "quit"),
        FooterHint::key("Q", "stop+quit"),
        FooterHint::text(format!("v{}", env!("CARGO_PKG_VERSION"))),
    ]
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
    let label_width = form_label_width(form);
    for (idx, field) in form.fields.iter().enumerate() {
        if field.section != prev_section {
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
        let label = format!("{marker} {:<label_width$}{optional}: ", field.display);
        if editing && field_accepts_cursor(field) {
            active_field_line = Some(lines.len());
            active_field_prefix_chars = label.chars().count();
        }
        let mut row = vec![Span::styled(label, label_style)];
        row.extend(field_value_spans(field, form.cursor, editing));
        if active {
            row.push(Span::styled(
                format!("  {}", field_kind_tag(field)),
                theme::dim(),
            ));
            if !field.help.is_empty() {
                row.extend([
                    Span::styled("  ·  ", theme::dim()),
                    Span::styled(field.help.to_string(), theme::hint()),
                ]);
            }
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

fn form_label_width(form: &Form) -> usize {
    form.fields
        .iter()
        .map(|field| field.display.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 18)
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
    sections + suggestion_count + usize::from(suggestion_count > 0)
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
    use super::{
        footer_context, footer_model, form_extra_rows, form_label_width, key_hint, FooterContext,
        FooterHint,
    };
    use crate::app::{
        App, FieldKind, Focus, Form, FormField, FormKind, IssueDetailSection, ProjectChildren, View,
    };
    use crate::ui::draw;
    use auwsx_core::agent::ExitKind;
    use auwsx_core::db::agent_runs::{AgentRun, Role};
    use auwsx_core::db::issues::Issue;
    use auwsx_core::db::projects::{CompletionPolicy, MergeMode, Project};
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

    fn rendered_app_lines(app: App, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        term.backend()
            .buffer()
            .content()
            .chunks(w as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect()
    }

    #[test]
    fn draw_overview_normal_no_panic() {
        draw_view(View::Overview, 100, 30);
    }

    #[test]
    fn draw_issue_renders_phase_reports() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        let issue = Issue {
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
        };
        app.projects.push(Project {
            id: 1,
            profile_id: 1,
            profile_order: 0,
            name: "p".to_string(),
            repo_path: ".".to_string(),
            default_branch: "main".to_string(),
            arsenal_preset_name: None,
            main_agent_cmd: "agent".to_string(),
            route_agent_cmd: "agent".to_string(),
            plan_agent_cmd: "agent".to_string(),
            work_agent_cmd: "agent".to_string(),
            review_agent_cmd: None,
            main_agent_cmd_override: None,
            route_agent_cmd_override: None,
            plan_agent_cmd_override: None,
            work_agent_cmd_override: None,
            review_agent_cmd_override: None,
            completion_policy: CompletionPolicy::Manual,
            completion_soft_timeout_min: 0,
            plan_gate_timeout_min: 0,
            iteration_timeout_min: 30,
            main_job_timeout_min: 30,
            review_max_rounds: 1,
            conflict_max_attempts: 1,
            max_concurrency: 1,
            schedule_cron: None,
            merge_mode: MergeMode::Local,
            skill_path: None,
            deepsleep_cron: None,
            last_deepsleep_at: None,
            created_at: 1,
        });
        app.expanded.insert(1);
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue.clone()],
                ..ProjectChildren::default()
            },
        );
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, crate::app::TreeItem::Issue { id: 7, .. }))
            .unwrap();
        app.focus = Focus::IssueDetail;
        app.detail.issue = Some(issue);
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
            phase_report: Some(
                "Implemented archive view.\n\
                 - Verification: cargo test --package auwsx-tui"
                    .to_string(),
            ),
            spawned_at: 1,
            exited_at: Some(2),
            note: None,
        }];

        let rendered = rendered_app(app, 120, 40);

        assert!(rendered.contains("phase report"));
        assert!(rendered.contains("Implemented archive view"));
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
        assert_eq!(key_hint("a", "steer"), "(a)steer");
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
    fn given_footer_when_modeled_then_global_hints_keep_key_structure() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.connected = true;

        let model = footer_model(&app);

        assert!(model.global.iter().any(|hint| matches!(
            hint,
            FooterHint::Key { key, label } if key == "Ctrl-," && label == "config"
        )));
        assert!(model.global.iter().any(|hint| matches!(
            hint,
            FooterHint::Key { key, label } if key == "q" && label == "quit"
        )));
        assert!(!model.context.iter().any(|hint| matches!(
            hint,
            FooterHint::Key { key, .. } if key == "q"
        )));
    }

    #[test]
    fn given_footer_when_rendered_then_global_hints_are_right_aligned() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.connected = true;

        let lines = rendered_app_lines(app, 120, 30);
        let footer = lines.last().expect("footer line");
        let context_pos = footer.find("(j/k) move").expect("context hint");
        let global_pos = footer.find("(q)uit").expect("global hint");

        assert!(context_pos < 8, "context hint should stay left: {footer:?}");
        assert!(
            global_pos > 80,
            "global hint should be right aligned: {footer:?}"
        );
        assert!(footer.contains("(Ctrl-,) config"));
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
        assert!(!findings.contains("(Enter)"));
        assert!(!findings.contains("scroll log"));

        app.issue_section = IssueDetailSection::Log;
        let log = footer_model(&app).context_text();
        assert!(log.contains("(Enter) scroll log"));
    }

    #[test]
    fn given_empty_kanban_lane_when_footer_modeled_then_enter_is_not_advertised() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.connected = true;
        app.focus = Focus::ProjectKanban;

        let context = footer_model(&app).context_text();

        assert!(context.contains("kanban"));
        assert!(!context.contains("(Enter)"));
    }

    #[test]
    fn given_single_field_form_when_rendered_then_label_is_compact() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
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

        let rendered = rendered_app(app, 90, 20);

        assert!(rendered.contains("> Text:"));
        assert!(!rendered.contains(">               Text:"));
        assert_eq!(rendered.matches("nav ·").count(), 1);
        assert!(!rendered.contains("Nav · j/k field"));
    }

    #[test]
    fn given_project_form_when_label_width_requested_then_matches_real_fields() {
        let form = Form {
            kind: FormKind::Project,
            title: "Project",
            fields: vec![
                FormField {
                    label: "name",
                    display: "Name",
                    section: "Project",
                    help: "",
                    kind: FieldKind::Text,
                    value: String::new(),
                    optional: false,
                },
                FormField {
                    label: "schedule_cron",
                    display: "Scheduler cadence",
                    section: "Schedule",
                    help: "",
                    kind: FieldKind::Text,
                    value: String::new(),
                    optional: true,
                },
            ],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: crate::app::FormMode::Navigate,
        };

        assert_eq!(form_label_width(&form), "Scheduler cadence".len());
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

        assert_eq!(with_help, 1);
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
