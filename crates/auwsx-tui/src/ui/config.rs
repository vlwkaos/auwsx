//! Settings: global profiles, Arsenal presets, prompt review, and UX guidance.

use super::theme;
use crate::app::{App, SettingsRow};
use auwsx_core::db::arsenal::ArsenalPreset;
use auwsx_core::db::memory_presets::MemoryPreset;
use auwsx_core::db::profiles::Profile;
use auwsx_core::prompt;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(40)])
        .split(area);
    render_nav(frame, app, chunks[0]);
    render_detail(frame, app, chunks[1]);
}

fn render_nav(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.settings_rows();
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .map(|(idx, row)| nav_line(app, idx, row))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::border(app.focus == crate::app::Focus::Settings))
                .title(Span::styled(" Settings ", theme::title())),
        ),
        area,
    );
}

fn render_detail(frame: &mut Frame, app: &App, area: Rect) {
    let (title, lines) = detail_lines(app);
    let scroll = app
        .config_scroll
        .min(lines.len().saturating_sub(area.height as usize)) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::border(app.focus == crate::app::Focus::Settings))
                    .title(Span::styled(format!(" {title} "), theme::title())),
            ),
        area,
    );
}

fn nav_line(app: &App, idx: usize, row: &SettingsRow) -> Line<'static> {
    let selected = idx == app.settings_sel;
    let marker = if selected { ">" } else { " " };
    let label = match row {
        SettingsRow::RuntimeDefaults => "Overview".to_string(),
        SettingsRow::ArsenalOverview => "Arsenal presets".to_string(),
        SettingsRow::MemoryOverview => "Memory presets".to_string(),
        SettingsRow::ProfilesOverview => "Profiles".to_string(),
        SettingsRow::Profile(id) => {
            let name = app
                .profiles
                .iter()
                .find(|profile| profile.id == *id)
                .map(|profile| profile.name.as_str())
                .unwrap_or("profile");
            format!("Profile: {name}")
        }
        SettingsRow::ArsenalPreset(idx) => app
            .arsenal_presets
            .get(*idx)
            .map(|preset| format!("Arsenal: {}", preset.name))
            .unwrap_or_else(|| "Arsenal preset".to_string()),
        SettingsRow::MemoryPreset(idx) => app
            .memory_presets
            .get(*idx)
            .map(|preset| format!("Memory: {}", preset.name))
            .unwrap_or_else(|| "Memory preset".to_string()),
        SettingsRow::PromptCatalog => "Prompt Catalog".to_string(),
        SettingsRow::PipelineUxStandard => "Pipeline UX Standard".to_string(),
    };
    let style = if selected {
        Style::default()
            .fg(theme::HIGHLIGHT_FG)
            .bg(theme::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::TEXT)
    };
    Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(label, style),
    ])
}

fn detail_lines(app: &App) -> (String, Vec<Line<'static>>) {
    match app.selected_settings_row() {
        SettingsRow::RuntimeDefaults => ("Settings Overview".into(), runtime_default_lines(app)),
        SettingsRow::ArsenalOverview => ("Arsenal Presets".into(), arsenal_overview_lines(app)),
        SettingsRow::MemoryOverview => ("Memory Presets".into(), memory_overview_lines(app)),
        SettingsRow::ProfilesOverview => ("Profiles".into(), profiles_overview_lines(app)),
        SettingsRow::Profile(id) => {
            let profile = app.profiles.iter().find(|profile| profile.id == id);
            ("Profile".into(), profile_lines(app, profile))
        }
        SettingsRow::ArsenalPreset(idx) => {
            let preset = app.arsenal_presets.get(idx);
            (
                preset
                    .map(|preset| format!("Arsenal - {}", preset.name))
                    .unwrap_or_else(|| "Arsenal".into()),
                arsenal_detail_lines(preset),
            )
        }
        SettingsRow::MemoryPreset(idx) => {
            let preset = app.memory_presets.get(idx);
            (
                preset
                    .map(|preset| format!("Memory - {}", preset.name))
                    .unwrap_or_else(|| "Memory".into()),
                memory_detail_lines(preset),
            )
        }
        SettingsRow::PromptCatalog => ("Prompt Catalog".into(), prompt_catalog_lines()),
        SettingsRow::PipelineUxStandard => (
            "Pipeline UX Standard".into(),
            pipeline_ux_standard_lines(app),
        ),
    }
}

fn runtime_default_lines(app: &App) -> Vec<Line<'static>> {
    vec![
        section("Global Settings"),
        kv("arsenal presets", app.arsenal_presets.len().to_string()),
        kv("memory presets", app.memory_presets.len().to_string()),
        kv("profiles", app.profiles.len().to_string()),
        kv("prompt phases", prompt::preview_count().to_string()),
        sep(),
        section("Project Settings"),
        kv("where", "select a project in Overview and press e".into()),
        kv(
            "project-only",
            "merge, schedule, concurrency, timeouts, skill path".into(),
        ),
        sep(),
        section("Pipeline Standard"),
        kv(
            "editable",
            if app.global_settings.is_some() {
                "yes"
            } else {
                "not loaded"
            }
            .into(),
        ),
    ]
}

fn memory_overview_lines(app: &App) -> Vec<Line<'static>> {
    let builtin = app
        .memory_presets
        .iter()
        .filter(|preset| preset.builtin)
        .count();
    let custom = app.memory_presets.len().saturating_sub(builtin);
    let active = app
        .global_settings
        .as_ref()
        .map(|settings| settings.memory_preset_name.as_str())
        .unwrap_or("not loaded");
    vec![
        section("Memory"),
        kv("active", active.into()),
        kv("builtin", builtin.to_string()),
        kv("custom", custom.to_string()),
        sep(),
        section("Purpose"),
        Line::raw("Memory presets wire retrieve, save, dream, and deepsleep."),
        Line::raw("Use Settings edit to choose a preset; CLI edits preset commands."),
    ]
}

fn arsenal_overview_lines(app: &App) -> Vec<Line<'static>> {
    let builtin = app
        .arsenal_presets
        .iter()
        .filter(|preset| preset.builtin)
        .count();
    let custom = app.arsenal_presets.len().saturating_sub(builtin);
    vec![
        section("Arsenal"),
        kv("builtin", builtin.to_string()),
        kv("custom", custom.to_string()),
        kv("new preset", "press a or Enter here".into()),
        sep(),
        section("Purpose"),
        Line::raw("Arsenal presets are global reusable per-role agent commands."),
        Line::raw("Projects consume one preset; edit command templates here."),
    ]
}

fn profiles_overview_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        section("Profiles"),
        kv("count", app.profiles.len().to_string()),
        kv("purpose", "group projects in the left column".into()),
        sep(),
    ];
    for profile in &app.profiles {
        let count = app
            .projects
            .iter()
            .filter(|project| project.profile_id == profile.id)
            .count();
        lines.push(kv(&profile.name, format!("{count} projects")));
    }
    lines
}

fn profile_lines(app: &App, profile: Option<&Profile>) -> Vec<Line<'static>> {
    let Some(profile) = profile else {
        return vec![section("Profile"), kv("state", "missing".into())];
    };
    let count = app
        .projects
        .iter()
        .filter(|project| project.profile_id == profile.id)
        .count();
    vec![
        section("Profile"),
        kv("name", profile.name.clone()),
        kv("projects", count.to_string()),
        kv(
            "active",
            (app.active_profile_name() == profile.name).to_string(),
        ),
    ]
}

fn arsenal_detail_lines(preset: Option<&ArsenalPreset>) -> Vec<Line<'static>> {
    let Some(preset) = preset else {
        return vec![section("Arsenal"), kv("state", "missing".into())];
    };
    vec![
        section("Arsenal Preset"),
        kv("name", preset.name.clone()),
        kv(
            "scope",
            if preset.builtin { "builtin" } else { "custom" }.into(),
        ),
        sep(),
        section("Role Commands"),
        kv("main", preset.main_agent_cmd.clone()),
        kv("route", preset.route_agent_cmd.clone()),
        kv("plan", preset.plan_agent_cmd.clone()),
        kv("work", preset.work_agent_cmd.clone()),
        kv(
            "review",
            preset
                .review_agent_cmd
                .clone()
                .unwrap_or_else(|| "(falls back to work)".into()),
        ),
    ]
}

fn memory_detail_lines(preset: Option<&MemoryPreset>) -> Vec<Line<'static>> {
    let Some(preset) = preset else {
        return vec![section("Memory"), kv("state", "missing".into())];
    };
    vec![
        section("Memory Preset"),
        kv("name", preset.name.clone()),
        kv(
            "scope",
            if preset.builtin { "builtin" } else { "custom" }.into(),
        ),
        sep(),
        section("Interfaces"),
        kv(
            "retrieve",
            format!(
                "{} {}",
                preset.retrieve_kind,
                preset.retrieve_cmd.as_deref().unwrap_or("")
            ),
        ),
        kv(
            "save",
            format!(
                "{} {}",
                preset.save_kind,
                preset.save_cmd.as_deref().unwrap_or("")
            ),
        ),
        kv(
            "dream",
            format!(
                "{} {}",
                preset.dream_kind,
                preset.dream_cmd.as_deref().unwrap_or("")
            ),
        ),
        kv(
            "deepsleep",
            format!(
                "{} {}",
                preset.deepsleep_kind,
                preset.deepsleep_cmd.as_deref().unwrap_or("")
            ),
        ),
    ]
}

fn section(label: &str) -> Line<'static> {
    Line::from(Span::styled(label.to_string(), theme::title()))
}

fn prompt_catalog_lines() -> Vec<Line<'static>> {
    let mut lines = vec![
        section("Prompt Catalog"),
        kv("source", "auwsx_core::prompt::preview_catalog()".into()),
        kv(
            "live context",
            "issue subtasks, queue messages, and findings are injected at run time".into(),
        ),
        kv(
            "human gates",
            "PLAN_READY, READY_TO_MERGE, blocked, done, failed, abandoned do not spawn prompts"
                .into(),
        ),
    ];
    for preview in prompt::preview_catalog() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("{} prompt", preview.status.as_str()),
            theme::title(),
        )));
        for line in preview.text.lines() {
            lines.push(Line::from(Span::styled(line.to_string(), theme::TEXT)));
        }
    }
    lines
}

fn pipeline_ux_standard_lines(app: &App) -> Vec<Line<'static>> {
    let guidance = app
        .global_settings
        .as_ref()
        .map(|settings| settings.pipeline_ux_guidance.as_str())
        .unwrap_or("not loaded");
    let preset = app
        .global_settings
        .as_ref()
        .map(|settings| settings.memory_preset_name.as_str())
        .unwrap_or("not loaded");
    let mut lines = vec![
        section("Persisted Worker Guidance"),
        kv("memory preset", preset.into()),
        kv("edit", "press e or Enter".into()),
        sep(),
    ];
    for line in guidance.lines() {
        lines.push(Line::from(Span::styled(line.to_string(), theme::TEXT)));
    }
    lines
}

fn kv(key: &str, val: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:>18}: "), theme::dim()),
        Span::styled(val, Style::default().fg(theme::TEXT)),
    ])
}

fn sep() -> Line<'static> {
    Line::from(Span::styled("  ─", Style::default().fg(theme::BORDER)))
}
