//! The session's status (feature `status`, `thread/harnessStatus`) from pi's `get_state` and
//! `get_session_stats`.
//!
//! pi answers with data, not text; the sections and labels follow pi's own `/session` screen of
//! interactive mode (pi 0.85.1 `handleSessionCommand`: "Session Info", "Messages", "Tokens",
//! "Cost", with the same rows and number formats), plus a "State" section for what `get_state`
//! adds (model, thinking level, context, queue modes), worded like pi's settings.

use aas_harness::{StatusRow, StatusSection};

use crate::wire::{PiSessionStats, PiState};

fn row(label: &str, value: impl Into<String>) -> StatusRow {
    StatusRow {
        label: label.to_owned(),
        value: value.into(),
    }
}

/// `n` with thousands separators (JavaScript's `toLocaleString()` for en-US, which pi uses).
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The sections of a running session.
pub fn sections(state: &PiState, stats: &PiSessionStats) -> Vec<StatusSection> {
    let mut out = Vec::new();

    let mut info = Vec::new();
    if let Some(name) = state.session_name.as_deref().filter(|n| !n.is_empty()) {
        info.push(row("Name", name));
    }
    info.push(row(
        "File",
        stats
            .session_file
            .clone()
            .or_else(|| state.session_file.clone())
            .unwrap_or_else(|| "In-memory".into()),
    ));
    if let Some(id) = stats
        .session_id
        .clone()
        .or_else(|| state.session_id.clone())
    {
        info.push(row("ID", id));
    }
    out.push(StatusSection {
        title: "Session Info".into(),
        rows: info,
    });

    let mut current = Vec::new();
    if let Some(model) = &state.model {
        current.push(row("Model", model.qualified_id()));
    }
    if let Some(level) = &state.thinking_level {
        current.push(row("Thinking level", level.clone()));
    }
    if let Some(context) = stats.context_usage.filter(|c| c.context_window > 0) {
        // pi's footer shows `?` while the context is unknown (right after a compaction).
        let percent = context
            .percent
            .map_or_else(|| "?".to_owned(), |p| format!("{p:.1}"));
        let used = context.tokens.map_or_else(|| "?".to_owned(), grouped);
        current.push(row(
            "Context",
            format!(
                "{percent}% of {} ({used} tokens)",
                grouped(context.context_window)
            ),
        ));
    }
    if let Some(mode) = &state.steering_mode {
        current.push(row("Steering mode", mode.clone()));
    }
    if let Some(mode) = &state.follow_up_mode {
        current.push(row("Follow-up mode", mode.clone()));
    }
    if let Some(enabled) = state.auto_compaction_enabled {
        current.push(row("Auto-compact", if enabled { "true" } else { "false" }));
    }
    if let Some(pending) = state.pending_message_count.filter(|n| *n > 0) {
        current.push(row("Pending messages", grouped(pending)));
    }
    if !current.is_empty() {
        out.push(StatusSection {
            title: "State".into(),
            rows: current,
        });
    }

    out.push(StatusSection {
        title: "Messages".into(),
        rows: vec![
            row("Total", grouped(stats.total_messages)),
            row("User", grouped(stats.user_messages)),
            row("Assistant", grouped(stats.assistant_messages)),
            row(
                "Tools",
                format!(
                    "{} calls, {} results",
                    grouped(stats.tool_calls),
                    grouped(stats.tool_results)
                ),
            ),
        ],
    });

    // "Input" is the full prompt volume; with cache activity pi splits it into cached and
    // uncached (cache writes are part of the uncached share).
    let tokens = stats.tokens;
    let prompt = tokens.input + tokens.cache_read + tokens.cache_write;
    let mut token_rows = vec![row("Input", grouped(prompt))];
    if prompt > 0 && (tokens.cache_read > 0 || tokens.cache_write > 0) {
        token_rows.push(row(
            "Cached",
            format!(
                "{} ({:.1}%)",
                grouped(tokens.cache_read),
                tokens.cache_read as f64 * 100.0 / prompt as f64
            ),
        ));
        let mut uncached = grouped(tokens.input + tokens.cache_write);
        if tokens.cache_write > 0 {
            uncached.push_str(&format!(
                " ({} written to cache)",
                grouped(tokens.cache_write)
            ));
        }
        token_rows.push(row("Uncached", uncached));
    }
    token_rows.push(row("Output", grouped(tokens.output)));
    token_rows.push(row("Total", grouped(tokens.total)));
    out.push(StatusSection {
        title: "Tokens".into(),
        rows: token_rows,
    });

    if stats.cost > 0.0 {
        out.push(StatusSection {
            title: "Cost".into(),
            rows: vec![row("Total", format!("${:.3}", stats.cost))],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_are_grouped_like_pi() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(262144), "262,144");
        assert_eq!(grouped(1048576), "1,048,576");
    }

    #[test]
    fn sections_follow_pi_session_screen() {
        // Shapes recorded from pi 0.85.1 (`get_state`, `get_session_stats`).
        let state: PiState = serde_json::from_value(json!({
            "model": {"id": "deepseek/deepseek-v4.1-flash", "provider": "orcarouter", "reasoning": true},
            "thinkingLevel": "low", "isStreaming": false, "isCompacting": false,
            "steeringMode": "one-at-a-time", "followUpMode": "one-at-a-time",
            "sessionFile": "C:\\s\\a.jsonl", "sessionId": "01a0e93f", "sessionName": "rec2 initial name",
            "autoCompactionEnabled": true, "messageCount": 4, "pendingMessageCount": 0
        }))
        .unwrap();
        let stats: PiSessionStats = serde_json::from_value(json!({
            "sessionFile": "C:\\s\\a.jsonl", "sessionId": "01a0e93f", "userMessages": 2,
            "assistantMessages": 2, "toolCalls": 1, "toolResults": 1, "totalMessages": 5,
            "tokens": {"input": 1200, "output": 30, "cacheRead": 3000, "cacheWrite": 0, "total": 4230},
            "cost": 0.0021,
            "contextUsage": {"tokens": 4230, "contextWindow": 1048576, "percent": 0.40340423583984375}
        }))
        .unwrap();
        let sections = sections(&state, &stats);
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Session Info", "State", "Messages", "Tokens", "Cost"]
        );
        let rows = |i: usize| -> Vec<(String, String)> {
            sections[i]
                .rows
                .iter()
                .map(|r| (r.label.clone(), r.value.clone()))
                .collect()
        };
        let pairs = |p: &[(&str, &str)]| -> Vec<(String, String)> {
            p.iter()
                .map(|(l, v)| ((*l).to_owned(), (*v).to_owned()))
                .collect()
        };
        assert_eq!(
            rows(0),
            pairs(&[
                ("Name", "rec2 initial name"),
                ("File", "C:\\s\\a.jsonl"),
                ("ID", "01a0e93f")
            ])
        );
        assert_eq!(
            rows(1),
            pairs(&[
                ("Model", "orcarouter/deepseek/deepseek-v4.1-flash"),
                ("Thinking level", "low"),
                ("Context", "0.4% of 1,048,576 (4,230 tokens)"),
                ("Steering mode", "one-at-a-time"),
                ("Follow-up mode", "one-at-a-time"),
                ("Auto-compact", "true"),
            ])
        );
        assert_eq!(
            rows(2),
            pairs(&[
                ("Total", "5"),
                ("User", "2"),
                ("Assistant", "2"),
                ("Tools", "1 calls, 1 results")
            ])
        );
        assert_eq!(
            rows(3),
            pairs(&[
                ("Input", "4,200"),
                ("Cached", "3,000 (71.4%)"),
                ("Uncached", "1,200"),
                ("Output", "30"),
                ("Total", "4,230")
            ])
        );
        assert_eq!(rows(4), pairs(&[("Total", "$0.002")]));
    }

    #[test]
    fn an_unknown_context_and_no_cost_are_shown_as_pi_does() {
        let state: PiState = serde_json::from_value(json!({"sessionId": "s"})).unwrap();
        let stats: PiSessionStats = serde_json::from_value(json!({
            "contextUsage": {"tokens": null, "contextWindow": 262144, "percent": null}
        }))
        .unwrap();
        let sections = sections(&state, &stats);
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Session Info", "State", "Messages", "Tokens"]);
        assert_eq!(sections[0].rows[0].value, "In-memory");
        assert_eq!(sections[1].rows[0].value, "?% of 262,144 (? tokens)");
    }
}
