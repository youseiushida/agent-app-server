//! Harness commands for the composer, and the construction of `turn/start` input.
//!
//! Codex slash commands are client-side features, so the adapter advertises the ones it can
//! execute through the protocol and intercepts them only when the *entire* turn text is exactly
//! `/<name>` (plus arguments where the command takes them). Skills are advertised as `$name`;
//! a `$name` token in the text whose name exactly matches a known skill is sent as a `skill`
//! input next to the text (the same thing the Codex clients do).

use aas_harness::protocol::{Command, CommandAction, CommandSource};
use aas_harness::{TurnInput, TurnInputPart};
use serde_json::{Value, json};

use crate::wire::SkillMetadata;

/// A skill known for the session's working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: String,
}

impl SkillInfo {
    pub fn from_metadata(m: &SkillMetadata) -> Self {
        Self {
            name: m.name.clone(),
            description: m
                .short_description
                .clone()
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| m.description.clone()),
            path: m.path.clone(),
        }
    }
}

/// Commands the adapter executes itself (exact match of the whole turn text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intercept {
    /// `/compact` → `thread/compact/start`.
    Compact,
    /// `/review [instructions]` → `review/start` (inline). Without instructions the target is
    /// the uncommitted changes.
    Review { instructions: Option<String> },
}

/// Commands shown in the composer's `/` menu.
pub fn commands(skills: &[SkillInfo]) -> Vec<Command> {
    let mut out = vec![
        Command {
            name: "compact".into(),
            description: Some(
                "Summarize the conversation to free context (Codex compaction)".into(),
            ),
            source: CommandSource::Harness,
            argument_hint: None,
            action: CommandAction::InsertText {
                text: "/compact".into(),
            },
        },
        Command {
            name: "review".into(),
            description: Some(
                "Code review of the uncommitted changes, or of what the instructions describe"
                    .into(),
            ),
            source: CommandSource::Harness,
            argument_hint: Some("[instructions]".into()),
            action: CommandAction::InsertText {
                text: "/review ".into(),
            },
        },
    ];
    for skill in skills {
        out.push(Command {
            name: skill.name.clone(),
            description: Some(if skill.description.is_empty() {
                "Skill".to_owned()
            } else {
                format!("Skill: {}", skill.description)
            }),
            source: CommandSource::Harness,
            argument_hint: None,
            action: CommandAction::InsertText {
                text: format!("${} ", skill.name),
            },
        });
    }
    out
}

/// Recognises an advertised command. Only a turn consisting of a single text part is eligible.
pub fn parse_intercept(input: &TurnInput) -> Option<Intercept> {
    let [TurnInputPart::Text(text)] = input.parts.as_slice() else {
        return None;
    };
    let text = text.trim();
    let (name, args) = match text.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (text, ""),
    };
    match name {
        "/compact" if args.is_empty() => Some(Intercept::Compact),
        "/review" => Some(Intercept::Review {
            instructions: (!args.is_empty()).then(|| args.to_owned()),
        }),
        _ => None,
    }
}

/// `review/start` target for an intercepted `/review`.
pub fn review_target(instructions: Option<&str>) -> Value {
    match instructions {
        Some(i) => json!({"type": "custom", "instructions": i}),
        None => json!({"type": "uncommittedChanges"}),
    }
}

fn is_token_end(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '"' | '\''
        )
}

/// Skills referenced as `$name` in `text` (exact names, token boundaries), each once, in
/// order of first appearance.
pub fn referenced_skills<'a>(text: &str, skills: &'a [SkillInfo]) -> Vec<&'a SkillInfo> {
    let mut out: Vec<&SkillInfo> = Vec::new();
    for (i, _) in text.match_indices('$') {
        let preceded_ok = text[..i]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        if !preceded_ok {
            continue;
        }
        let after = &text[i + 1..];
        // Longest exact match wins (skill names may be prefixes of one another).
        let best = skills
            .iter()
            .filter(|s| {
                after.starts_with(s.name.as_str())
                    && after[s.name.len()..]
                        .chars()
                        .next()
                        .is_none_or(is_token_end)
            })
            .max_by_key(|s| s.name.len());
        if let Some(skill) = best
            && !out.iter().any(|s| s.name == skill.name)
        {
            out.push(skill);
        }
    }
    out
}

/// `UserInput` items for `turn/start` / `turn/steer`.
///
/// Text and `@` mentions are rendered into one text item (mentions as `@path` — the protocol's
/// `mention` input type addresses apps/plugins, not files, and Codex clients insert file paths
/// as text). Images become `localImage` items; referenced skills become `skill` items.
pub fn user_inputs(input: &TurnInput, skills: &[SkillInfo]) -> Vec<Value> {
    let text = input.to_plain_text();
    let mut items = Vec::new();
    if !text.is_empty() {
        items.push(json!({"type": "text", "text": text, "text_elements": []}));
    }
    for (path, _mime) in input.images() {
        items.push(json!({"type": "localImage", "path": path.to_string_lossy()}));
    }
    for skill in referenced_skills(&text, skills) {
        items.push(json!({"type": "skill", "name": skill.name, "path": skill.path}));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skills() -> Vec<SkillInfo> {
        vec![
            SkillInfo {
                name: "pdf".into(),
                description: "PDF tools".into(),
                path: "/s/pdf/SKILL.md".into(),
            },
            SkillInfo {
                name: "pdf-extra".into(),
                description: String::new(),
                path: "/s/pdfx/SKILL.md".into(),
            },
        ]
    }

    #[test]
    fn intercepts_require_exact_commands() {
        assert_eq!(
            parse_intercept(&TurnInput::text("/compact")),
            Some(Intercept::Compact)
        );
        assert_eq!(
            parse_intercept(&TurnInput::text("  /compact  ")),
            Some(Intercept::Compact)
        );
        assert_eq!(parse_intercept(&TurnInput::text("/compact now")), None);
        assert_eq!(
            parse_intercept(&TurnInput::text("/review")),
            Some(Intercept::Review { instructions: None })
        );
        assert_eq!(
            parse_intercept(&TurnInput::text("/review focus on errors")),
            Some(Intercept::Review {
                instructions: Some("focus on errors".into())
            })
        );
        assert_eq!(parse_intercept(&TurnInput::text("/reviewer")), None);
        assert_eq!(parse_intercept(&TurnInput::text("please /compact")), None);
        let with_image = TurnInput {
            parts: vec![
                TurnInputPart::Text("/compact".into()),
                TurnInputPart::Image {
                    path: PathBuf::from("a.png"),
                    mime: "image/png".into(),
                },
            ],
        };
        assert_eq!(parse_intercept(&with_image), None);
    }

    #[test]
    fn skills_are_matched_exactly_with_longest_name() {
        let s = skills();
        let found: Vec<&str> = referenced_skills("use $pdf-extra and $pdf, not x$pdf or $pdfs", &s)
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(found, ["pdf-extra", "pdf"]);
    }

    #[test]
    fn user_inputs_render_text_images_and_skills() {
        let input = TurnInput {
            parts: vec![
                TurnInputPart::Text("fix".into()),
                TurnInputPart::Mention {
                    relative: "src/a.rs".into(),
                    absolute: PathBuf::from("/p/src/a.rs"),
                },
                TurnInputPart::Text(" with $pdf".into()),
                TurnInputPart::Image {
                    path: PathBuf::from("/tmp/x.png"),
                    mime: "image/png".into(),
                },
            ],
        };
        let items = user_inputs(&input, &skills());
        assert_eq!(
            items,
            vec![
                json!({"type":"text","text":"fix @src/a.rs with $pdf","text_elements":[]}),
                json!({"type":"localImage","path":"/tmp/x.png"}),
                json!({"type":"skill","name":"pdf","path":"/s/pdf/SKILL.md"}),
            ]
        );
    }

    #[test]
    fn advertised_commands_include_skills() {
        let cmds = commands(&skills());
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["compact", "review", "pdf", "pdf-extra"]);
        assert_eq!(
            cmds[2].action,
            CommandAction::InsertText {
                text: "$pdf ".into()
            }
        );
    }
}
