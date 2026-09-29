//! `thread/harnessStatus` for Codex: what the app-server reports about the thread, its goal, the
//! account and its rate limits, as sections of label and value (docs/adapters/codex.md §15).
//!
//! Every value comes from an explicit field of Codex's answers; the adapter only renders it as
//! text (numbers, windows, times until a reset). Nothing is estimated.

use aas_harness::{StatusRow, StatusSection};
use serde_json::Value;

use crate::wire::{
    AccountReadResponse, CreditsSnapshot, RateLimitSnapshot, RateLimitWindow, ThreadTokenUsage,
    WireGoal,
};

fn row(label: &str, value: impl Into<String>) -> StatusRow {
    StatusRow {
        label: label.to_owned(),
        value: value.into(),
    }
}

/// What the session knows about its thread (see [`thread_section`]).
#[derive(Debug, Clone, Default)]
pub struct ThreadFacts {
    pub thread_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Label of the permission preset in effect.
    pub permissions: Option<String>,
    /// `Some(true)`: plan mode, `Some(false)`: default mode, `None`: not known.
    pub plan: Option<bool>,
    /// The service tier as the fast mode state words it.
    pub service_tier: Option<String>,
    pub usage: Option<ThreadTokenUsage>,
}

/// The thread's own section.
pub fn thread_section(facts: &ThreadFacts) -> StatusSection {
    let mut rows = vec![row("Thread", facts.thread_id.clone())];
    if let Some(model) = &facts.model {
        rows.push(row("Model", model.clone()));
    }
    rows.push(row(
        "Reasoning effort",
        facts
            .effort
            .clone()
            .unwrap_or_else(|| "model default".to_owned()),
    ));
    if let Some(plan) = facts.plan {
        rows.push(row(
            "Collaboration mode",
            if plan { "Plan" } else { "Default" },
        ));
    }
    if let Some(tier) = &facts.service_tier {
        rows.push(row("Service tier", tier.clone()));
    }
    if let Some(permissions) = &facts.permissions {
        rows.push(row("Permissions", permissions.clone()));
    }
    if let Some(usage) = &facts.usage {
        if let Some(window) = usage.model_context_window.filter(|w| *w > 0) {
            let used = usage.last.total_tokens;
            rows.push(row(
                "Context window",
                format!(
                    "{} of {} tokens ({}% used)",
                    thousands(used),
                    thousands(window),
                    used.saturating_mul(100) / window
                ),
            ));
        }
        let t = &usage.total;
        rows.push(row(
            "Tokens used",
            format!(
                "{} (input {}, cached input {}, output {}, reasoning {})",
                thousands(t.total_tokens),
                thousands(t.input_tokens),
                thousands(t.cached_input_tokens),
                thousands(t.output_tokens),
                thousands(t.reasoning_output_tokens)
            ),
        ));
    }
    StatusSection {
        title: "Codex thread".into(),
        rows,
    }
}

/// The goal's section (`/goal`).
pub fn goal_section(goal: &WireGoal) -> StatusSection {
    let mut rows = vec![
        row("Objective", goal.objective.clone()),
        row("Status", goal_status_words(&goal.status)),
        row("Tokens used", thousands(goal.tokens_used)),
        row("Time used", duration_words(goal.time_used_seconds)),
    ];
    if let Some(budget) = goal.token_budget {
        rows.push(row("Token budget", thousands(budget)));
    }
    StatusSection {
        title: "Goal".into(),
        rows,
    }
}

/// Codex's goal status in words (`ThreadGoalStatus`).
pub fn goal_status_words(status: &str) -> String {
    match status {
        "active" => "active".into(),
        "paused" => "paused".into(),
        "blocked" => "blocked".into(),
        "usageLimited" => "stopped at the usage limit".into(),
        "budgetLimited" => "stopped at its token budget".into(),
        "complete" => "complete".into(),
        other => other.into(),
    }
}

/// The account's section, from `account/read`.
pub fn account_section(account: Result<&AccountReadResponse, &str>) -> StatusSection {
    let rows = match account {
        Err(error) => vec![row("Not available", error)],
        Ok(resp) => match &resp.account {
            None if resp.requires_openai_auth => vec![row("Sign-in", "not signed in")],
            None => vec![row(
                "Sign-in",
                "not required by the configured model provider",
            )],
            Some(account) => account_rows(account),
        },
    };
    StatusSection {
        title: "Account".into(),
        rows,
    }
}

fn account_rows(account: &Value) -> Vec<StatusRow> {
    let text = |key: &str| account.get(key).and_then(Value::as_str).map(str::to_owned);
    let kind = text("type").unwrap_or_default();
    let mut rows = vec![row(
        "Signed in with",
        match kind.as_str() {
            "chatgpt" => "ChatGPT".to_owned(),
            "apiKey" => "API key".to_owned(),
            "amazonBedrock" => "Amazon Bedrock".to_owned(),
            other => other.to_owned(),
        },
    )];
    if let Some(email) = text("email") {
        rows.push(row("Email", email));
    }
    if let Some(plan) = text("planType") {
        rows.push(row("Plan", plan));
    }
    rows
}

/// Where the rate limits come from: `account/rateLimits/read`, or the rolling updates Codex
/// sent after model calls when the read failed.
pub enum RateLimits<'a> {
    Read(&'a RateLimitSnapshot),
    Rolling {
        snapshot: Option<&'a RateLimitSnapshot>,
        read_error: &'a str,
    },
}

/// The rate limits' section. `now` is the current Unix time in seconds (for "resets in").
pub fn rate_limit_section(limits: RateLimits<'_>, now: i64) -> StatusSection {
    let rows = match limits {
        RateLimits::Read(s) if !s.is_empty() => snapshot_rows(s, now),
        RateLimits::Read(_) => vec![row(
            "Not reported",
            "the model provider reports no Codex rate limits",
        )],
        RateLimits::Rolling {
            snapshot: Some(s), ..
        } if !s.is_empty() => snapshot_rows(s, now),
        RateLimits::Rolling { read_error, .. } => vec![row("Not available", read_error)],
    };
    StatusSection {
        title: "Rate limits".into(),
        rows,
    }
}

fn snapshot_rows(s: &RateLimitSnapshot, now: i64) -> Vec<StatusRow> {
    let mut rows = Vec::new();
    if let Some(w) = &s.primary {
        rows.push(row("Primary limit", window_words(w, now)));
    }
    if let Some(w) = &s.secondary {
        rows.push(row("Secondary limit", window_words(w, now)));
    }
    if let Some(c) = &s.credits {
        rows.push(row("Credits", credits_words(c)));
    }
    if let Some(plan) = &s.plan_type {
        rows.push(row("Plan", plan.clone()));
    }
    if let Some(reached) = &s.rate_limit_reached_type {
        rows.push(row("Limit reached", reached.clone()));
    }
    rows
}

/// `13% used · 5h window · resets in 58m`.
pub fn window_words(w: &RateLimitWindow, now: i64) -> String {
    let mut parts = vec![format!("{}% used", trim_float(w.used_percent))];
    if let Some(mins) = w.window_duration_mins {
        parts.push(format!("{} window", duration_words(mins * 60)));
    }
    if let Some(at) = w.resets_at {
        let left = at.saturating_sub(now);
        parts.push(if left > 0 {
            format!("resets in {}", duration_words(left as u64))
        } else {
            "reset due".to_owned()
        });
    }
    parts.join(" \u{b7} ")
}

fn credits_words(c: &CreditsSnapshot) -> String {
    if c.unlimited {
        return "unlimited".into();
    }
    match (&c.balance, c.has_credits) {
        (Some(balance), _) => balance.clone(),
        (None, true) => "available".into(),
        (None, false) => "none".into(),
    }
}

fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

/// A duration in the two largest units: `45s`, `58m`, `1h 23m`, `5h`, `7d`, `2d 3h`.
pub fn duration_words(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    let two = |big: u64, big_unit: &str, small: u64, small_unit: &str| {
        if small == 0 {
            format!("{big}{big_unit}")
        } else {
            format!("{big}{big_unit} {small}{small_unit}")
        }
    };
    if secs >= DAY {
        two(secs / DAY, "d", secs % DAY / HOUR, "h")
    } else if secs >= HOUR {
        two(secs / HOUR, "h", secs % HOUR / MIN, "m")
    } else if secs >= MIN {
        format!("{}m", secs / MIN)
    } else {
        format!("{secs}s")
    }
}

/// `12,428`.
pub fn thousands(n: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_and_durations_are_worded() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(12428), "12,428");
        assert_eq!(thousands(996147), "996,147");
        assert_eq!(duration_words(45), "45s");
        assert_eq!(duration_words(58 * 60 + 3), "58m");
        assert_eq!(duration_words(300 * 60), "5h");
        assert_eq!(duration_words(83 * 60), "1h 23m");
        assert_eq!(duration_words(10080 * 60), "7d");
        assert_eq!(duration_words(51 * 3600), "2d 3h");
    }

    #[test]
    fn recorded_rate_limits_are_worded() {
        // `account/rateLimits/updated` with Codex's headers (recording `tiers`).
        let update: crate::wire::RateLimitsUpdated = serde_json::from_value(json!({"rateLimits":{
            "limitId":"codex","limitName":null,
            "primary":{"usedPercent":13,"windowDurationMins":300,"resetsAt":1790623669},
            "secondary":{"usedPercent":40,"windowDurationMins":10080,"resetsAt":1790706469},
            "credits":{"hasCredits":true,"unlimited":false,"balance":"17.5"},
            "individualLimit":null,"spendControlReached":null,"planType":null,"rateLimitReachedType":null}}))
        .unwrap();
        let now = 1790623669 - 3480;
        let section = rate_limit_section(RateLimits::Read(&update.rate_limits), now);
        let rows: Vec<(&str, &str)> = section
            .rows
            .iter()
            .map(|r| (r.label.as_str(), r.value.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "Primary limit",
                    "13% used \u{b7} 5h window \u{b7} resets in 58m"
                ),
                (
                    "Secondary limit",
                    "40% used \u{b7} 7d window \u{b7} resets in 23h 58m"
                ),
                ("Credits", "17.5"),
            ]
        );
        // A provider without Codex's headers (the user's DeepSeek): every field null.
        let empty: crate::wire::RateLimitsUpdated = serde_json::from_value(json!({"rateLimits":{
            "limitId":"codex","limitName":null,"primary":null,"secondary":null,"credits":null,
            "individualLimit":null,"spendControlReached":null,"planType":null,"rateLimitReachedType":null}}))
        .unwrap();
        let section = rate_limit_section(
            RateLimits::Rolling {
                snapshot: Some(&empty.rate_limits),
                read_error: "codex account authentication required to read rate limits",
            },
            now,
        );
        assert_eq!(section.rows.len(), 1);
        assert_eq!(
            section.rows[0].value,
            "codex account authentication required to read rate limits"
        );
        // A sparse update keeps what an earlier one reported.
        let mut merged = update.rate_limits.clone();
        merged.merge(empty.rate_limits);
        assert_eq!(merged, update.rate_limits);
    }

    #[test]
    fn accounts_are_worded() {
        let none: AccountReadResponse =
            serde_json::from_value(json!({"account":null,"requiresOpenaiAuth":false})).unwrap();
        assert_eq!(
            account_section(Ok(&none)).rows[0].value,
            "not required by the configured model provider"
        );
        let chatgpt: AccountReadResponse = serde_json::from_value(json!({
            "account":{"type":"chatgpt","email":"a@example.com","planType":"pro"},
            "requiresOpenaiAuth":true}))
        .unwrap();
        let rows: Vec<String> = account_section(Ok(&chatgpt))
            .rows
            .iter()
            .map(|r| format!("{}={}", r.label, r.value))
            .collect();
        assert_eq!(
            rows,
            ["Signed in with=ChatGPT", "Email=a@example.com", "Plan=pro"]
        );
        assert_eq!(account_section(Err("boom")).rows[0].value, "boom");
    }
}
