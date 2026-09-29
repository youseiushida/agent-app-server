//! Harness commands that switch the native session (design.md §9.5): never offered in
//! `command/list`, and refused when typed as the first word of an input.

use aas_harness::HarnessAdapter;
use aas_protocol::{ErrorKind, InputPart, RpcError};

use crate::error::CoreError;

/// Harness command names that are never offered, whatever the harness.
///
/// `resume`: every CLI that has a command of that name uses it to open another of its sessions
/// in the running process (Claude Code's and pi's session pickers, Codex's `/resume`). A thread
/// is one native session (design.md §9.5), and the app offers its own `/resume` that opens the
/// session import (`native/list`, `native/import`), which a harness command of the same name
/// would hide (docs/ux/codex-desktop.md §8.5).
pub(crate) const SESSION_SWITCHING_COMMAND_NAMES: &[&str] = &["resume"];

/// Every name that switches the native session in `adapter`'s harness:
/// [`SESSION_SWITCHING_COMMAND_NAMES`] and the adapter's names with their aliases
/// ([`HarnessAdapter::session_switching_names`]).
pub(crate) fn session_switching_names(adapter: &dyn HarnessAdapter) -> Vec<String> {
    let mut names: Vec<String> = SESSION_SWITCHING_COMMAND_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    for name in adapter.session_switching_names() {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The command an input starts with, when its first word is a slash command: the first part
/// is text whose first word (after leading blank space) is `/<name>`. Mentions and images
/// first are not commands.
pub(crate) fn leading_command(input: &[InputPart]) -> Option<&str> {
    let InputPart::Text { text } = input.first()? else {
        return None;
    };
    let word = text.split_whitespace().next()?;
    word.strip_prefix('/').filter(|name| !name.is_empty())
}

/// Refuses `input` when its first word is one of `names` (`sessionSwitchingCommand`, a
/// definitive error with `data.command` and `data.harnessId`): sent to the harness, it would
/// switch the thread's agent to another native session. The names are the harness's own list
/// (never guessed from what the command says about itself).
pub(crate) fn refuse_session_switch(
    input: &[InputPart],
    names: &[String],
    harness_id: &str,
) -> Result<(), CoreError> {
    let Some(command) = leading_command(input) else {
        return Ok(());
    };
    if !names.iter().any(|n| n == command) {
        return Ok(());
    }
    Err(CoreError::Rpc(
        RpcError::new(
            ErrorKind::SessionSwitchingCommand,
            format!(
                "/{command} would switch the agent to another session; a thread keeps its one session (use the app's commands instead)"
            ),
        )
        .with("command", command)
        .with("harnessId", harness_id),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> Vec<InputPart> {
        vec![InputPart::Text { text: t.into() }]
    }

    #[test]
    fn only_a_leading_slash_word_is_a_command() {
        assert_eq!(leading_command(&text("/clear")), Some("clear"));
        assert_eq!(leading_command(&text("  /new please")), Some("new"));
        assert_eq!(leading_command(&text("please /clear")), None);
        assert_eq!(leading_command(&text("/ clear")), None);
        assert_eq!(leading_command(&text("")), None);
        assert_eq!(
            leading_command(&[
                InputPart::Mention {
                    path: "a.rs".into()
                },
                InputPart::Text {
                    text: "/clear".into()
                }
            ]),
            None
        );
    }

    #[test]
    fn switching_commands_are_refused_with_their_name() {
        let names = vec!["resume".to_owned(), "clear".to_owned(), "reset".to_owned()];
        assert!(refuse_session_switch(&text("/compact"), &names, "claude").is_ok());
        assert!(refuse_session_switch(&text("clear the cache"), &names, "claude").is_ok());
        let CoreError::Rpc(e) =
            refuse_session_switch(&text("/reset now"), &names, "claude").unwrap_err()
        else {
            panic!("an rpc error")
        };
        assert_eq!(e.kind(), Some(ErrorKind::SessionSwitchingCommand));
        let data = e.data.unwrap();
        assert_eq!(data["command"], "reset");
        assert_eq!(data["harnessId"], "claude");
    }
}
