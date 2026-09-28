//! The composer's `/` commands of a pi session.
//!
//! pi's RPC mode lists extension commands, prompt templates and skills (`get_commands`); they
//! are sent as prompt text. pi's built-in TUI commands are not available over RPC, except
//! those the RPC protocol offers as commands of its own. The adapter adds those and executes
//! them itself when the whole turn is exactly the command:
//!
//! * `/compact [instructions]` → the RPC command `compact` (`customInstructions` = the rest).
//!
//! A command of the same name listed by `get_commands` (an extension's own `/compact`) takes
//! precedence: the adapter then neither adds nor intercepts it.

use aas_harness::{Command, TurnInput, TurnInputPart};
use aas_protocol::{CommandAction, CommandSource};

use crate::wire::PiCommand;

/// Name of the built-in compaction command.
pub const COMPACT: &str = "compact";

fn to_command(c: PiCommand) -> Command {
    Command {
        action: CommandAction::InsertText {
            text: format!("/{} ", c.name),
        },
        name: c.name,
        description: c.description,
        source: CommandSource::Harness,
        argument_hint: None,
    }
}

fn compact_command() -> Command {
    Command {
        name: COMPACT.into(),
        description: Some("Summarize the conversation to free context (pi compaction)".into()),
        source: CommandSource::Harness,
        argument_hint: Some("[instructions]".into()),
        action: CommandAction::InsertText {
            text: format!("/{COMPACT} "),
        },
    }
}

/// Whether the adapter handles `/compact` itself for a session with these pi commands.
pub fn builtin_compact(pi_commands: &[PiCommand]) -> bool {
    !pi_commands.iter().any(|c| c.name == COMPACT)
}

/// The commands shown for a session: pi's own, then the built-in ones pi does not shadow.
pub fn commands(pi_commands: Vec<PiCommand>) -> Vec<Command> {
    let add_compact = builtin_compact(&pi_commands);
    let mut out: Vec<Command> = pi_commands.into_iter().map(to_command).collect();
    if add_compact {
        out.push(compact_command());
    }
    out
}

/// Recognises `/compact [instructions]` as the whole turn (a single text part). Returns the
/// instructions (`None` when there are none).
pub fn parse_compact(input: &TurnInput) -> Option<Option<String>> {
    let [TurnInputPart::Text(text)] = input.parts.as_slice() else {
        return None;
    };
    let text = text.trim();
    let (name, rest) = match text.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (text, ""),
    };
    (name.strip_prefix('/') == Some(COMPACT)).then(|| (!rest.is_empty()).then(|| rest.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn pi(name: &str) -> PiCommand {
        PiCommand {
            name: name.into(),
            description: Some("d".into()),
            source: Some("extension".into()),
        }
    }

    #[test]
    fn compact_is_added_unless_pi_has_its_own() {
        let names = |c: Vec<Command>| c.into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names(commands(vec![pi("skill:x")])), ["skill:x", "compact"]);
        assert_eq!(names(commands(vec![pi("compact")])), ["compact"]);
        let c = commands(vec![pi("skill:x")]);
        assert_eq!(
            c[0].action,
            CommandAction::InsertText {
                text: "/skill:x ".into()
            }
        );
        assert_eq!(
            c[1].action,
            CommandAction::InsertText {
                text: "/compact ".into()
            }
        );
        assert_eq!(c[1].source, CommandSource::Harness);
    }

    #[test]
    fn only_the_whole_turn_is_a_compaction() {
        assert_eq!(parse_compact(&TurnInput::text("/compact")), Some(None));
        assert_eq!(parse_compact(&TurnInput::text("  /compact  ")), Some(None));
        assert_eq!(
            parse_compact(&TurnInput::text("/compact keep the API decisions")),
            Some(Some("keep the API decisions".into()))
        );
        assert_eq!(parse_compact(&TurnInput::text("/compacted")), None);
        assert_eq!(parse_compact(&TurnInput::text("please /compact")), None);
        let with_image = TurnInput {
            parts: vec![
                TurnInputPart::Text("/compact".into()),
                TurnInputPart::Image {
                    path: PathBuf::from("a.png"),
                    mime: "image/png".into(),
                },
            ],
        };
        assert_eq!(parse_compact(&with_image), None);
    }
}
