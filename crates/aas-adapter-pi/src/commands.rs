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
//!
//! Extension commands (`source: "extension"`) are also told apart from other prompts: pi runs
//! them as soon as the prompt arrives, even while a run goes on, and answers the prompt only
//! once the command's handler has returned (see [`is_extension_command`]). While a run goes on,
//! pi's `steer` refuses them ("cannot be queued"), so a steer that is an extension command is
//! sent as a `prompt` (see `PiSession::steer`).
//!
//! Left out of the listing:
//! * the approval gate's internal command ([`crate::gate::FORK_COMMAND`], sent by the adapter
//!   itself to fork at a turn); its `/reload` is offered like any extension command;
//! * [`TUI_ONLY_COMMANDS`]: commands of extensions pi bundles that only work in pi's
//!   interactive mode, by name and source, per pi version.

use std::collections::HashSet;

use aas_harness::{Command, TurnInput, TurnInputPart};
use aas_protocol::{CommandAction, CommandSource};

use crate::gate;
use crate::wire::PiCommand;

/// Name of the built-in compaction command.
pub const COMPACT: &str = "compact";

/// Commands of extensions bundled with pi that do nothing over RPC, as `(name, sourceInfo.path)`
/// (both must match, so a user's own extension of the same name stays). Checked against pi
/// 0.85.1:
/// * `llama` from the bundled llama.cpp extension (`<inline:llama.cpp>`): its handler returns
///   right away with the notice "/llama is available in interactive mode" when `ctx.mode` is
///   not `tui` (the extension's `registerCommand("llama")`), so offering it on the phone only
///   produces that notice. pi marks the command in no other way (no hidden flag; `inline` only
///   says the extension was loaded from a factory), hence the list.
pub const TUI_ONLY_COMMANDS: &[(&str, &str)] = &[("llama", "<inline:llama.cpp>")];

/// Whether the command is one pi lists but the phone never gets (see the module docs).
fn hidden(c: &PiCommand) -> bool {
    (c.name == gate::FORK_COMMAND && c.source.as_deref() == Some("extension"))
        || TUI_ONLY_COMMANDS
            .iter()
            .any(|(name, path)| c.name == *name && c.source_path() == Some(path))
}

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

/// The commands shown for a session: pi's own (without the hidden ones), then the built-in ones
/// pi does not shadow.
pub fn commands(pi_commands: Vec<PiCommand>) -> Vec<Command> {
    let add_compact = builtin_compact(&pi_commands);
    let mut out: Vec<Command> = pi_commands
        .into_iter()
        .filter(|c| !hidden(c))
        .map(to_command)
        .collect();
    if add_compact {
        out.push(compact_command());
    }
    out
}

/// Invocation names of the extension commands in a `get_commands` listing (the names pi
/// matches a prompt against).
pub fn extension_command_names(pi_commands: &[PiCommand]) -> HashSet<String> {
    pi_commands
        .iter()
        .filter(|c| c.source.as_deref() == Some("extension"))
        .map(|c| c.name.clone())
        .collect()
}

/// Whether pi runs `message` (the prompt text as sent) as one of these extension commands. The
/// same test as pi 0.85.1's `_tryExecuteExtensionCommand`: the text starts with `/` and the
/// name, up to the first space, is a registered command's invocation name.
pub fn is_extension_command(message: &str, extension_commands: &HashSet<String>) -> bool {
    let Some(rest) = message.strip_prefix('/') else {
        return false;
    };
    let name = rest.split_once(' ').map_or(rest, |(name, _)| name);
    extension_commands.contains(name)
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
            source_info: None,
        }
    }

    fn from(name: &str, path: &str) -> PiCommand {
        PiCommand {
            source_info: Some(crate::wire::PiSourceInfo {
                path: Some(path.into()),
                source: Some("inline".into()),
            }),
            ..pi(name)
        }
    }

    #[test]
    fn tui_only_and_internal_commands_are_not_offered() {
        let names = |c: Vec<Command>| c.into_iter().map(|c| c.name).collect::<Vec<_>>();
        let listed = vec![
            // pi 0.85.1's bundled llama.cpp extension: interactive mode only.
            from("llama", "<inline:llama.cpp>"),
            // A user's own extension of that name stays.
            from("llama", "C:/Users/me/.pi/agent/extensions/llama.ts"),
            // The gate's internal fork command is the adapter's; its `/reload` is offered.
            from("aas-gate-fork", "C:/state/aas-gate-v3.ts"),
            from("reload", "C:/state/aas-gate-v3.ts"),
            pi("skill:x"),
        ];
        assert_eq!(
            names(commands(listed)),
            ["llama", "reload", "skill:x", "compact"]
        );
        // A skill or template of the internal command's name is not the gate's.
        let mut template = pi("aas-gate-fork");
        template.source = Some("prompt".into());
        assert_eq!(
            names(commands(vec![template])),
            ["aas-gate-fork", "compact"]
        );
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

    #[test]
    fn extension_commands_are_matched_like_pi_does() {
        let mut listed = vec![pi("aas-later"), pi("review:2")];
        listed.push(PiCommand {
            name: "skill:x".into(),
            description: None,
            source: Some("skill".into()),
            source_info: None,
        });
        let names = extension_command_names(&listed);
        assert_eq!(names.len(), 2, "skills and templates are expanded, not run");
        assert!(is_extension_command("/aas-later", &names));
        assert!(is_extension_command("/aas-later 1500 some text", &names));
        assert!(is_extension_command("/review:2 x", &names));
        assert!(!is_extension_command("/skill:x", &names));
        assert!(
            !is_extension_command(" /aas-later", &names),
            "pi needs the leading slash"
        );
        assert!(
            !is_extension_command("/aas-later\tx", &names),
            "pi splits at a space only"
        );
        assert!(!is_extension_command("aas-later", &names));
        assert!(!is_extension_command("/", &names));
    }
}
