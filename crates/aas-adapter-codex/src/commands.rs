//! Harness commands for the composer, and the construction of `turn/start` input.
//!
//! Codex slash commands are client-side features, so the adapter advertises the ones it can
//! execute through the protocol and intercepts them only when the *entire* turn text is exactly
//! `/<name>` (plus arguments where the command takes them). Skills are advertised as `$name`;
//! a `$name` token in the text whose name exactly matches a known skill is sent as a `skill`
//! input next to the text (the same thing the Codex clients do).
//!
//! The descriptions of `init` and `goal` are the TUI's own (codex-cli 0.148.0).

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
    /// `/init` → a turn whose input is Codex's own `/init` prompt
    /// ([`crate::texts::INIT_PROMPT`]).
    Init,
    /// `/goal …` → the `thread/goal/*` requests.
    Goal(GoalCommand),
}

impl Intercept {
    /// The command as the user types it.
    pub fn command(&self) -> &'static str {
        match self {
            Intercept::Compact => "/compact",
            Intercept::Review { .. } => "/review",
            Intercept::Init => "/init",
            Intercept::Goal(_) => "/goal",
        }
    }
}

/// The forms of `/goal` (the TUI's: `/goal [<objective>|clear|edit|pause|resume]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCommand {
    /// `/goal`: the current goal (`thread/goal/get`).
    Show,
    /// `/goal <objective>`: the objective becomes the thread's goal and is pursued now
    /// (`thread/goal/set { objective, status: "active" }`; a goal that exists keeps its
    /// counters).
    Set { objective: String },
    /// `/goal edit <objective>`: the goal's objective changes, its status stays
    /// (`thread/goal/set { objective }`).
    Edit { objective: String },
    /// `/goal clear` (`thread/goal/clear`).
    Clear,
    /// `/goal pause` (`thread/goal/set { status: "paused" }`).
    Pause,
    /// `/goal resume` (`thread/goal/set { status: "active" }`).
    Resume,
}

/// Usage of `/goal`, in the TUI's words.
pub const GOAL_USAGE: &str = "Usage: /goal [<objective>|clear|edit|pause|resume]";

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
        Command {
            name: "init".into(),
            description: Some("create an AGENTS.md file with instructions for Codex".into()),
            source: CommandSource::Harness,
            argument_hint: None,
            action: CommandAction::InsertText {
                text: "/init".into(),
            },
        },
        Command {
            name: "goal".into(),
            description: Some("set or view the goal for a long-running task".into()),
            source: CommandSource::Harness,
            argument_hint: Some("[<objective>|clear|edit|pause|resume]".into()),
            action: CommandAction::InsertText {
                text: "/goal ".into(),
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
/// `Err` is a `/goal` whose arguments do not fit its usage (the message says how to use it).
pub fn parse_intercept(input: &TurnInput) -> Option<Result<Intercept, String>> {
    let [TurnInputPart::Text(text)] = input.parts.as_slice() else {
        return None;
    };
    let text = text.trim();
    let (name, args) = match text.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (text, ""),
    };
    match name {
        "/compact" if args.is_empty() => Some(Ok(Intercept::Compact)),
        "/init" if args.is_empty() => Some(Ok(Intercept::Init)),
        "/review" => Some(Ok(Intercept::Review {
            instructions: (!args.is_empty()).then(|| args.to_owned()),
        })),
        "/goal" => Some(parse_goal(args).map(Intercept::Goal)),
        _ => None,
    }
}

/// The arguments of `/goal`. The first word names a sub-command only when it is exactly one of
/// the TUI's (`clear`, `edit`, `pause`, `resume`); anything else is the objective.
fn parse_goal(args: &str) -> Result<GoalCommand, String> {
    let (word, rest) = match args.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (args, ""),
    };
    Ok(match (word, rest.is_empty()) {
        ("", _) => GoalCommand::Show,
        ("clear", true) => GoalCommand::Clear,
        ("pause", true) => GoalCommand::Pause,
        ("resume", true) => GoalCommand::Resume,
        ("edit", false) => GoalCommand::Edit {
            objective: rest.to_owned(),
        },
        ("edit", true) => {
            return Err(format!(
                "{GOAL_USAGE}. `/goal edit` takes the new objective: /goal edit <objective>"
            ));
        }
        ("clear" | "pause" | "resume", false) => {
            return Err(format!("{GOAL_USAGE}. `/goal {word}` takes no arguments"));
        }
        _ => GoalCommand::Set {
            objective: args.to_owned(),
        },
    })
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

    fn intercept(text: &str) -> Option<Intercept> {
        parse_intercept(&TurnInput::text(text)).map(|r| r.unwrap())
    }

    #[test]
    fn intercepts_require_exact_commands() {
        assert_eq!(intercept("/compact"), Some(Intercept::Compact));
        assert_eq!(intercept("  /compact  "), Some(Intercept::Compact));
        assert_eq!(intercept("/compact now"), None);
        assert_eq!(
            intercept("/review"),
            Some(Intercept::Review { instructions: None })
        );
        assert_eq!(
            intercept("/review focus on errors"),
            Some(Intercept::Review {
                instructions: Some("focus on errors".into())
            })
        );
        assert_eq!(intercept("/reviewer"), None);
        assert_eq!(intercept("please /compact"), None);
        assert_eq!(intercept("/init"), Some(Intercept::Init));
        // `/init` takes no arguments: anything more is an ordinary message.
        assert_eq!(intercept("/init for the backend"), None);
        assert_eq!(intercept("/initial"), None);
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
    fn goal_forms_follow_the_tui() {
        let goal = |text: &str| match intercept(text) {
            Some(Intercept::Goal(g)) => g,
            other => panic!("{text}: {other:?}"),
        };
        assert_eq!(goal("/goal"), GoalCommand::Show);
        assert_eq!(goal("/goal   "), GoalCommand::Show);
        assert_eq!(goal("/goal clear"), GoalCommand::Clear);
        assert_eq!(goal("/goal pause"), GoalCommand::Pause);
        assert_eq!(goal("/goal resume"), GoalCommand::Resume);
        assert_eq!(
            goal("/goal make the tests pass"),
            GoalCommand::Set {
                objective: "make the tests pass".into()
            }
        );
        // Only the exact sub-command words are sub-commands.
        assert_eq!(
            goal("/goal clearing the backlog"),
            GoalCommand::Set {
                objective: "clearing the backlog".into()
            }
        );
        assert_eq!(
            goal("/goal edit  ship v2 "),
            GoalCommand::Edit {
                objective: "ship v2".into()
            }
        );
        for bad in [
            "/goal edit",
            "/goal clear now",
            "/goal pause it",
            "/goal resume x",
        ] {
            let err = parse_intercept(&TurnInput::text(bad)).unwrap().unwrap_err();
            assert!(err.starts_with(GOAL_USAGE), "{bad}: {err}");
        }
        assert_eq!(intercept("/goals"), None);
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
        assert_eq!(
            names,
            ["compact", "review", "init", "goal", "pdf", "pdf-extra"]
        );
        assert_eq!(
            cmds[4].action,
            CommandAction::InsertText {
                text: "$pdf ".into()
            }
        );
    }
}
