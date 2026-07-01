//! Shared issue-detail log rendering helpers.

use super::theme;
use crate::app::App;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

pub(super) fn log_block_with_title_focused(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    section: impl AsRef<str>,
    focused: bool,
) {
    log_block_inner(frame, area, app, Some(section.as_ref()), focused);
}

fn log_block_inner(frame: &mut Frame, area: Rect, app: &App, section: Option<&str>, focused: bool) {
    frame.render_widget(Clear, area);
    let base_title = app
        .log_tail_path
        .as_ref()
        .and_then(|path| path.rsplit('/').next())
        .map(|name| match section {
            Some(section) => format!(" {section}: {name} "),
            None => format!(" Agent log: {name} "),
        })
        .unwrap_or_else(|| match section {
            Some(section) => format!(" {section} "),
            None => " Agent log ".to_string(),
        });
    let log_width = area.width.saturating_sub(4).max(20) as usize;
    let lines = readable_log_lines(&app.log_tail, log_width);
    let body = if lines.is_empty() {
        vec![Line::styled("No agent log yet.", theme::dim())]
    } else {
        lines
    };
    let visible_rows = area.height.saturating_sub(2) as usize;
    let visible_rows = visible_rows.max(1);
    let max_start = body.len().saturating_sub(visible_rows);
    let scroll_from_bottom = app.issue_log_scroll.min(max_start);
    let start = max_start.saturating_sub(scroll_from_bottom);
    let end = (start + visible_rows).min(body.len());
    let title = if max_start == 0 {
        base_title
    } else {
        format!("{} {}/{} ", base_title.trim_end(), end, body.len())
    };
    frame.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::border(focused))
                    .title(Span::styled(title, theme::title())),
            )
            .scroll((start as u16, 0)),
        area,
    );
}

fn readable_log_lines(text: &str, max_chars: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<_> = text
        .lines()
        .flat_map(|line| readable_log_event(line, max_chars))
        .collect();
    let overflow = lines.len().saturating_sub(600);
    if overflow > 0 {
        lines.drain(0..overflow);
        lines.insert(
            0,
            Line::styled(
                format!("... {overflow} older log lines omitted"),
                theme::dim(),
            ),
        );
    }
    lines
}

fn readable_log_event(raw: &str, max_chars: usize) -> Vec<Line<'static>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return text_block("raw", trimmed, 8, max_chars);
    };
    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let item = value.get("item");
    if event_type == "auwsx.event" {
        return readable_system_event(value.get("event"));
    }
    match (
        event_type,
        item.and_then(|v| v.get("type")).and_then(|v| v.as_str()),
    ) {
        ("thread.started", _) => value
            .get("thread_id")
            .and_then(|v| v.as_str())
            .map(|id| Line::styled(format!("thread started {id}"), theme::dim()))
            .or_else(|| Some(Line::styled("thread started", theme::dim())))
            .into_iter()
            .collect(),
        ("turn.started", _) => vec![Line::styled("turn started", theme::dim())],
        ("turn.completed", _) => vec![Line::styled("turn completed", theme::dim())],
        ("item.started", Some("command_execution")) => vec![Line::raw(format!(
            "cmd started: {}",
            command_text(item, max_chars.saturating_sub(14).max(4))
        ))],
        ("item.completed", Some("command_execution")) => {
            let code = item
                .and_then(|v| v.get("exit_code"))
                .and_then(|v| v.as_i64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".to_string());
            let output = item
                .and_then(|v| v.get("aggregated_output"))
                .and_then(|v| v.as_str())
                .map(first_useful_output_line)
                .filter(|line| !line.is_empty());
            let suffix = output
                .map(|line| format!(" | {}", truncate(&line, max_chars / 2)))
                .unwrap_or_default();
            vec![Line::raw(format!(
                "cmd exit {code}: {}{suffix}",
                command_text(
                    item,
                    max_chars.saturating_sub(suffix.chars().count() + 16).max(4)
                )
            ))]
        }
        ("item.completed", Some("agent_message")) => item
            .and_then(|v| v.get("text"))
            .and_then(|v| v.as_str())
            .map(|text| text_block("agent", text, 24, max_chars))
            .unwrap_or_default(),
        ("item.started", Some(item_type)) => {
            vec![Line::styled(format!("{item_type} started"), theme::dim())]
        }
        ("item.completed", Some(item_type)) => {
            vec![Line::styled(format!("{item_type} completed"), theme::dim())]
        }
        ("item.failed", Some(item_type)) => vec![Line::raw(format!("{item_type} failed"))],
        _ if !event_type.is_empty() => vec![Line::styled(event_type.to_string(), theme::dim())],
        _ => text_block("json", trimmed, 8, max_chars),
    }
}

fn readable_system_event(event: Option<&serde_json::Value>) -> Vec<Line<'static>> {
    let Some(event) = event else {
        return vec![Line::styled("auwsx event", theme::dim())];
    };
    match event.get("kind").and_then(|v| v.as_str()) {
        Some("spawn") => {
            let role = json_str(event, "role").unwrap_or("agent");
            let phase = json_str(event, "phase").unwrap_or("?");
            let run_id = json_i64(event, "run_id")
                .map(|id| id.to_string())
                .unwrap_or_else(|| "?".to_string());
            let cmd = json_str(event, "cmd")
                .map(|s| truncate(s, 140))
                .unwrap_or_else(|| "(unknown command)".to_string());
            let socket = json_str(event, "socket")
                .map(|s| format!(" | socket {}", truncate(s, 80)))
                .unwrap_or_default();
            let agent_socket = json_str(event, "agent_socket")
                .map(|s| format!(" | agent_socket {}", truncate(s, 80)))
                .unwrap_or_default();
            vec![Line::styled(
                format!("auwsx spawn run #{run_id} {role}/{phase}: {cmd}{socket}{agent_socket}"),
                theme::dim(),
            )]
        }
        Some("finish") => {
            let run_id = json_i64(event, "run_id")
                .map(|id| id.to_string())
                .unwrap_or_else(|| "?".to_string());
            let exit_kind = json_str(event, "exit_kind").unwrap_or("?");
            let exit_code = json_i64(event, "exit_code")
                .map(|code| format!("/{code}"))
                .unwrap_or_default();
            let status_after = json_str(event, "status_after").unwrap_or("(none)");
            let error = json_str(event, "error")
                .map(|e| format!(" | {}", truncate(e, 100)))
                .unwrap_or_default();
            vec![Line::styled(
                format!(
                    "auwsx finish run #{run_id}: {exit_kind}{exit_code}, status {status_after}{error}"
                ),
                theme::dim(),
            )]
        }
        Some("status") => {
            let from = json_str(event, "from").unwrap_or("(none)");
            let to = json_str(event, "to").unwrap_or("?");
            let result = json_str(event, "result").unwrap_or("?");
            let force = event
                .get("force")
                .and_then(|v| v.as_bool())
                .map(|v| if v { " forced" } else { "" })
                .unwrap_or("");
            let error = json_str(event, "error")
                .map(|e| format!(" | {}", truncate(e, 100)))
                .unwrap_or_default();
            vec![Line::styled(
                format!("auwsx status{force}: {from} -> {to} {result}{error}"),
                theme::dim(),
            )]
        }
        Some("proxy_ready") => {
            let path = json_str(event, "path").unwrap_or("(unknown)");
            let exists = event
                .get("exists")
                .and_then(|v| v.as_bool())
                .map(|v| if v { "exists" } else { "missing" })
                .unwrap_or("unknown");
            vec![Line::styled(
                format!("auwsx proxy ready: {} ({exists})", truncate(path, 100)),
                theme::dim(),
            )]
        }
        Some("proxy_accept") => {
            let path = json_str(event, "path").unwrap_or("(unknown)");
            vec![Line::styled(
                format!("auwsx proxy accept: {}", truncate(path, 100)),
                theme::dim(),
            )]
        }
        Some("proxy_upstream_error") | Some("proxy_copy_error") => {
            let kind = json_str(event, "kind").unwrap_or("proxy_error");
            let error = json_str(event, "error").unwrap_or("(unknown error)");
            vec![Line::styled(
                format!("auwsx {kind}: {}", truncate(error, 120)),
                Style::default().fg(theme::WARN),
            )]
        }
        Some("proxy_drop") => {
            let path = json_str(event, "path").unwrap_or("(unknown)");
            let existed = event
                .get("exists_before_remove")
                .and_then(|v| v.as_bool())
                .map(|v| if v { "removed" } else { "already missing" })
                .unwrap_or("closed");
            vec![Line::styled(
                format!("auwsx proxy drop: {} ({existed})", truncate(path, 100)),
                theme::dim(),
            )]
        }
        Some(kind) => vec![Line::styled(format!("auwsx {kind}"), theme::dim())],
        None => vec![Line::styled("auwsx event", theme::dim())],
    }
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

fn json_i64(value: &serde_json::Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|v| v.as_i64())
}

fn command_text(item: Option<&serde_json::Value>, max: usize) -> String {
    item.and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
        .map(display_command)
        .map(|command| redact_secrets(&command))
        .map(|command| truncate(&command, max))
        .unwrap_or_else(|| "(unknown command)".to_string())
}

fn first_useful_output_line(text: &str) -> String {
    normalize_log_text(text)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "---")
        .map(redact_secrets)
        .unwrap_or_default()
}

fn display_command(command: &str) -> String {
    let trimmed = command.trim();
    for prefix in ["/bin/zsh -lc ", "zsh -lc ", "/bin/bash -lc ", "bash -lc "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return strip_outer_quotes(rest.trim()).to_string();
        }
    }
    trimmed.to_string()
}

fn strip_outer_quotes(text: &str) -> &str {
    let bytes = text.as_bytes();
    if bytes.len() < 2 {
        return text;
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
        &text[1..text.len() - 1]
    } else {
        text
    }
}

fn text_block(label: &str, text: &str, max_lines: usize, max_chars: usize) -> Vec<Line<'static>> {
    let normalized = normalize_log_text(text);
    let mut source_lines: Vec<String> = normalized
        .lines()
        .map(str::trim_end)
        .map(str::to_string)
        .collect();
    while source_lines
        .first()
        .is_some_and(|line| line.trim().is_empty())
    {
        source_lines.remove(0);
    }
    while source_lines
        .last()
        .is_some_and(|line| line.trim().is_empty())
    {
        source_lines.pop();
    }

    if source_lines.is_empty() {
        return vec![Line::styled(format!("{label}: (empty)"), theme::dim())];
    }

    let total = source_lines.len();
    let mut out = vec![Line::styled(format!("{label}:"), theme::dim())];
    for line in source_lines.iter().take(max_lines) {
        out.extend(wrap_log_text_line(
            &redact_secrets(line),
            max_chars.saturating_sub(2).max(4),
            "  ",
        ));
    }
    if total > max_lines {
        out.push(Line::styled(
            format!("  ... {} more lines", total - max_lines),
            theme::dim(),
        ));
    }
    out
}

fn normalize_log_text(text: &str) -> String {
    let expanded = text
        .replace("\\r\\n", "\n")
        .replace("\\n", "\n")
        .replace("\\t", "    ");
    strip_terminal_sequences(&expanded)
}

fn strip_terminal_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while !rest.is_empty() {
        match rest.find('\x1b') {
            Some(0) if rest.starts_with("\x1b[") => {
                let after = &rest[2..];
                rest = if let Some(end) = after.find(|c: char| c.is_ascii_alphabetic()) {
                    &after[end + 1..]
                } else {
                    ""
                };
            }
            Some(0) if rest.starts_with("\x1b]") => {
                let after = &rest[2..];
                rest = if let Some(end) = after.find('\x07') {
                    &after[end + 1..]
                } else if let Some(end) = after.find("\x1b\\") {
                    &after[end + 2..]
                } else {
                    ""
                };
            }
            Some(0) => {
                rest = &rest[1..];
            }
            Some(pos) => {
                push_printable(&rest[..pos], &mut out);
                rest = &rest[pos..];
            }
            None => {
                push_printable(rest, &mut out);
                break;
            }
        }
    }
    out
}

fn push_printable(text: &str, out: &mut String) {
    for ch in text.chars() {
        if ch == '\n' || ch == '\t' || (ch >= ' ' && ch != '\x7f') {
            out.push(ch);
        }
    }
}

fn redact_secrets(text: &str) -> String {
    let trimmed = text.trim_start();
    let upper = trimmed.to_ascii_uppercase();
    let secret_key = upper.split(['=', ':', ' ']).next().is_some_and(|key| {
        key.contains("TOKEN")
            || key.contains("SECRET")
            || key.contains("PASSWORD")
            || key.contains("API_KEY")
            || key == "KEY"
    });
    if secret_key {
        if let Some((left, _)) = text.split_once('=') {
            return format!("{left}=<redacted>");
        }
        if let Some((left, _)) = text.split_once(':') {
            return format!("{left}: <redacted>");
        }
        return "<redacted>".to_string();
    }

    let mut out = Vec::new();
    for word in text.split_whitespace() {
        let redacted = word.starts_with("sk-")
            || word.starts_with("ghp_")
            || word.starts_with("github_pat_")
            || word.starts_with("xoxb-");
        if redacted {
            out.push("<redacted>");
        } else {
            out.push(word);
        }
    }
    if out.len() == text.split_whitespace().count() && out.contains(&"<redacted>") {
        out.join(" ")
    } else {
        text.to_string()
    }
}

fn wrap_log_text_line(line: &str, max_chars: usize, prefix: &str) -> Vec<Line<'static>> {
    if line.is_empty() {
        return vec![Line::raw(prefix.to_string())];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in line.chars() {
        current.push(ch);
        if current.chars().count() >= max_chars {
            out.push(Line::raw(format!("{prefix}{current}")));
            current.clear();
        }
    }
    if !current.is_empty() {
        out.push(Line::raw(format!("{prefix}{current}")));
    }
    out
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(max_chars) {
        out.push(ch);
    }
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn readable(raw: &str) -> Vec<Line<'static>> {
        readable_log_event(raw, 120)
    }

    #[test]
    fn given_codex_thread_event_when_readable_log_line_then_short_label() {
        let raw = r#"{"type":"thread.started","thread_id":"abc"}"#;

        let lines = readable(raw);
        let text = line_text(&lines[0]);

        assert_eq!(text, "thread started abc");
    }

    #[test]
    fn given_codex_command_event_when_readable_log_line_then_shell_wrapper_removed() {
        let raw = r#"{"type":"item.started","item":{"type":"command_execution","command":"/bin/zsh -lc 'cargo test'"}}"#;

        let lines = readable(raw);
        let text = line_text(&lines[0]);

        assert_eq!(text, "cmd started: cargo test");
    }

    #[test]
    fn given_unknown_json_event_when_readable_log_line_then_not_raw_json() {
        let raw = r#"{"type":"session.updated","value":1}"#;

        let lines = readable(raw);
        let text = line_text(&lines[0]);

        assert_eq!(text, "session.updated");
    }

    #[test]
    fn given_auwsx_spawn_event_when_readable_log_line_then_shows_command_and_socket() {
        let raw = r#"{"type":"auwsx.event","event":{"kind":"spawn","run_id":7,"role":"plan","phase":"NEW","cmd":"codex exec --sandbox workspace-write --json {prompt}","socket":"/cache/auwsx.sock","control_outbox":"/worktree/.auwsx/control/run-1.jsonl"}}"#;

        let lines = readable(raw);
        let text = line_text(&lines[0]);

        assert_eq!(
            text,
            "auwsx spawn run #7 plan/NEW: codex exec --sandbox workspace-write --json {prompt} | socket /cache/auwsx.sock"
        );
    }

    #[test]
    fn given_auwsx_status_event_when_readable_log_line_then_shows_transition_result() {
        let raw = r#"{"type":"auwsx.event","event":{"kind":"status","from":"NEW","to":"PLAN_READY","force":false,"result":"ok"}}"#;

        let lines = readable(raw);
        let text = line_text(&lines[0]);

        assert_eq!(text, "auwsx status: NEW -> PLAN_READY ok");
    }

    #[test]
    fn given_auwsx_finish_event_when_readable_log_line_then_shows_exit_and_status() {
        let raw = r#"{"type":"auwsx.event","event":{"kind":"finish","run_id":8,"exit_kind":"exited","exit_code":0,"status_after":"WORKING"}}"#;

        let lines = readable(raw);
        let text = line_text(&lines[0]);

        assert_eq!(text, "auwsx finish run #8: exited/0, status WORKING");
    }

    #[test]
    fn given_agent_message_with_literal_newlines_when_readable_log_event_then_multiline_block() {
        let raw = r#"{"type":"item.completed","item":{"type":"agent_message","text":"first\\nsecond\\nthird"}}"#;

        let lines = readable(raw);
        let text: Vec<_> = lines.iter().map(line_text).collect();

        assert_eq!(text, vec!["agent:", "  first", "  second", "  third"]);
    }

    #[test]
    fn given_long_agent_message_when_readable_log_event_then_omits_tail() {
        let raw = r#"{"type":"item.completed","item":{"type":"agent_message","text":"1\\n2\\n3\\n4\\n5\\n6\\n7\\n8\\n9\\n10\\n11\\n12\\n13\\n14\\n15\\n16\\n17\\n18\\n19\\n20\\n21\\n22\\n23\\n24\\n25"}}"#;

        let lines = readable(raw);
        let last = line_text(lines.last().expect("last"));

        assert_eq!(last, "  ... 1 more lines");
    }

    #[test]
    fn given_secret_env_line_when_readable_log_event_then_value_is_redacted() {
        let raw = r#"{"type":"item.completed","item":{"type":"command_execution","command":"env","aggregated_output":"ANTHROPIC_TOKEN=sk-ant-example\nSAFE=value"}}"#;

        let lines = readable(raw);
        let text = line_text(&lines[0]);

        assert_eq!(text, "cmd exit ?: env | ANTHROPIC_TOKEN=<redacted>");
    }

    #[test]
    fn given_long_agent_line_when_readable_log_event_then_wraps_to_width() {
        let raw = r#"{"type":"item.completed","item":{"type":"agent_message","text":"abcdefghijklmnopqrstuvwxyz"}}"#;

        let lines = readable_log_event(raw, 12);
        let text: Vec<_> = lines.iter().map(line_text).collect();

        assert_eq!(
            text,
            vec!["agent:", "  abcdefghij", "  klmnopqrst", "  uvwxyz"]
        );
    }

    #[test]
    fn given_ansi_and_control_output_when_readable_log_event_then_strips_terminal_sequences() {
        let raw = r#"{"type":"item.completed","item":{"type":"agent_message","text":"\u001b[31mred\u001b[0m\u0007\\n\u001b]0;title\u0007plain"}}"#;

        let lines = readable(raw);
        let text: Vec<_> = lines.iter().map(line_text).collect();

        assert_eq!(text, vec!["agent:", "  red", "  plain"]);
    }
}
