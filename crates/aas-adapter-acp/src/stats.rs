//! Devin's own figures of a session (Cognition's notifications, recorded from Devin CLI
//! 3000.11.3; docs/adapters/acp.md §17.3), shown as the harness's status
//! (`thread/harnessStatus`, design.md §9.6) in Devin's own groups and words. Nothing here is
//! interpreted: the values are only rendered for display.
//!
//! * `_cognition.ai/turn_stats {sessionId, turnClientMessageId, turnRequestId,
//!   responseDimensions}` after each model response of a turn; the values accumulate within the
//!   turn (the last one of a turn is the turn's). A dimension is `{uid, groupTitle, label,
//!   kind}`, with `kind` one of `{type: "cumulativeMetric", value: <number>, prefix, tail,
//!   pluralTail}`, `{type: "metric", value: <string>}` and `{type: "copyableCode", value}` (the
//!   last one is in the binary's serde names, not recorded). The dimensions come from Devin's
//!   server and differ by account.
//! * `_cognition.ai/billingInformation {title, body}`: not seen in any recording; its field
//!   names are the binary's serde names (`BillingInformationNotification`), and a string near
//!   it says it is sent when the agent continued past the per-turn billing threshold. It is
//!   shown as a notice and in the status.

use aas_harness::protocol::{StatusRow, StatusSection};
use serde::Deserialize;
use serde_json::Value;

/// `_cognition.ai/turn_stats`.
pub const TURN_STATS: &str = "_cognition.ai/turn_stats";
/// `_cognition.ai/billingInformation`.
pub const BILLING_INFORMATION: &str = "_cognition.ai/billingInformation";

/// Params of `turn_stats`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStats {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub turn_client_message_id: Option<String>,
    #[serde(default)]
    pub response_dimensions: Vec<Value>,
}

/// Params of `billingInformation`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingInformation {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

impl BillingInformation {
    /// The notification's text (title, then body); `None` when it has neither.
    pub fn text(&self) -> Option<String> {
        let parts: Vec<&str> = [self.title.as_deref(), self.body.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }
}

/// Words of the sections this adapter adds around Devin's groups.
const LAST_TURN: &str = "last turn";
const BILLING_SECTION: &str = "Billing information";
const BILLING_LABEL: &str = "Billing";

/// The status sections: the last turn's statistics in Devin's groups (in the order Devin sent
/// them), then the last billing information.
pub fn sections(
    stats: Option<&TurnStats>,
    billing: Option<&BillingInformation>,
) -> Vec<StatusSection> {
    let mut out: Vec<StatusSection> = Vec::new();
    let mut groups: Vec<(String, Vec<StatusRow>)> = Vec::new();
    for dimension in stats
        .map(|s| s.response_dimensions.as_slice())
        .unwrap_or(&[])
    {
        let Some((group, row)) = dimension_row(dimension) else {
            tracing::debug!(%dimension, "a turn statistic without a label or a value; not shown");
            continue;
        };
        match groups.iter_mut().find(|(g, _)| *g == group) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((group, vec![row])),
        }
    }
    for (group, rows) in groups {
        out.push(StatusSection {
            title: format!("{group} ({LAST_TURN})"),
            rows,
        });
    }
    if let Some(b) = billing
        && let Some(body) = b.body.as_deref().map(str::trim).filter(|s| !s.is_empty())
    {
        out.push(StatusSection {
            title: BILLING_SECTION.into(),
            rows: vec![StatusRow {
                label: b
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(BILLING_LABEL)
                    .to_owned(),
                value: body.to_owned(),
            }],
        });
    } else if let Some(title) = billing
        .and_then(|b| b.title.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push(StatusSection {
            title: BILLING_SECTION.into(),
            rows: vec![StatusRow {
                label: BILLING_LABEL.into(),
                value: title.to_owned(),
            }],
        });
    }
    out
}

/// One dimension as `(groupTitle, row)`. `None` without a label or a value to show.
fn dimension_row(d: &Value) -> Option<(String, StatusRow)> {
    let label = d.get("label").and_then(Value::as_str)?.trim();
    if label.is_empty() {
        return None;
    }
    let group = d
        .get("groupTitle")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let value = render_kind(d.get("kind")?)?;
    Some((
        group,
        StatusRow {
            label: label.to_owned(),
            value,
        },
    ))
}

/// The text of a dimension's value, as Devin's kinds describe it: a cumulative metric is
/// `prefix`, the number, and `tail` (for one) or `pluralTail`; a metric and a copyable code are
/// their value. Another kind shows its `value` as it is.
fn render_kind(kind: &Value) -> Option<String> {
    let value = kind.get("value")?;
    let text = match (kind.get("type").and_then(Value::as_str), value) {
        (Some("cumulativeMetric"), Value::Number(n)) => {
            let n = n.as_f64()?;
            let str_of = |key: &str| kind.get(key).and_then(Value::as_str).unwrap_or_default();
            let tail = if n == 1.0 {
                str_of("tail")
            } else {
                kind.get("pluralTail")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| str_of("tail"))
            };
            format!("{}{}{tail}", str_of("prefix"), number(n))
        }
        (_, Value::String(s)) => s.clone(),
        (_, Value::Number(n)) => n.to_string(),
        (_, Value::Bool(b)) => b.to_string(),
        _ => return None,
    };
    Some(text)
}

/// A number as Devin's figures read: whole numbers without a fraction (they arrive as
/// `8276.0`).
fn number(n: f64) -> String {
    // Exactly representable integers (|n| < 2^53) print without the fraction.
    const EXACT: f64 = 9_007_199_254_740_992.0;
    if n.fract() == 0.0 && n.abs() < EXACT {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn row(label: &str, value: &str) -> StatusRow {
        StatusRow {
            label: label.into(),
            value: value.into(),
        }
    }

    /// The dimensions recorded from Devin CLI 3000.11.3 (revert3.jsonl).
    #[test]
    fn recorded_turn_stats_render_in_devins_groups() {
        let stats: TurnStats = serde_json::from_value(json!({
            "sessionId": "s", "turnClientMessageId": "m", "turnRequestId": "r",
            "responseDimensions": [
                {"uid": "agent_messages", "groupTitle": "Response Statistics", "label": "Agent messages",
                 "kind": {"type": "cumulativeMetric", "value": 1.0, "prefix": "", "tail": " message", "pluralTail": " messages"}},
                {"uid": "model", "groupTitle": "Response Statistics", "label": "Model",
                 "kind": {"type": "metric", "value": "SWE-2 High"}},
                {"uid": "input_tokens", "groupTitle": "Token Usage", "label": "Input tokens",
                 "kind": {"type": "cumulativeMetric", "value": 8276.0, "prefix": "", "tail": " token", "pluralTail": " tokens"}},
                {"uid": "cost", "groupTitle": "Token Usage", "label": "Cost",
                 "kind": {"type": "cumulativeMetric", "value": 0.25, "prefix": "$", "tail": ""}},
                {"uid": "odd", "groupTitle": "Token Usage", "label": "",
                 "kind": {"type": "metric", "value": "x"}},
                {"uid": "code", "groupTitle": "Other", "label": "Request",
                 "kind": {"type": "copyableCode", "value": "req-1"}}
            ]
        }))
        .unwrap();
        assert_eq!(
            sections(Some(&stats), None),
            vec![
                StatusSection {
                    title: "Response Statistics (last turn)".into(),
                    rows: vec![
                        row("Agent messages", "1 message"),
                        row("Model", "SWE-2 High")
                    ]
                },
                StatusSection {
                    title: "Token Usage (last turn)".into(),
                    rows: vec![row("Input tokens", "8276 tokens"), row("Cost", "$0.25")]
                },
                StatusSection {
                    title: "Other (last turn)".into(),
                    rows: vec![row("Request", "req-1")]
                },
            ]
        );
        assert_eq!(sections(None, None), Vec::new());
    }

    #[test]
    fn billing_information_is_shown_with_its_title() {
        let b = BillingInformation {
            session_id: None,
            title: Some("Usage".into()),
            body: Some("The agent continued past the threshold.".into()),
        };
        assert_eq!(
            b.text().as_deref(),
            Some("Usage\nThe agent continued past the threshold.")
        );
        assert_eq!(
            sections(None, Some(&b)),
            vec![StatusSection {
                title: "Billing information".into(),
                rows: vec![row("Usage", "The agent continued past the threshold.")]
            }]
        );
        let title_only = BillingInformation {
            title: Some("Usage".into()),
            ..BillingInformation::default()
        };
        assert_eq!(
            sections(None, Some(&title_only))[0].rows,
            vec![row("Billing", "Usage")]
        );
        assert_eq!(BillingInformation::default().text(), None);
        assert_eq!(
            sections(None, Some(&BillingInformation::default())),
            Vec::new()
        );
    }

    #[test]
    fn numbers_read_like_devins() {
        assert_eq!(number(8276.0), "8276");
        assert_eq!(number(0.5), "0.5");
        assert_eq!(number(-3.0), "-3");
    }
}
