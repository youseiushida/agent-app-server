//! Texts that Codex's own clients send as user input, verbatim, so that the app does the same
//! thing as Codex (docs/adapters/codex.md §6, §14).
//!
//! They are not part of the app-server protocol: the TUI embeds them in its binary. Each one was
//! read from the codex-cli binary of [`CODEX_TEXTS_VERSION`] (`codex.exe` of the npm package
//! `@openai/codex-win32-x64`), where it appears word for word. The Windows build embeds the
//! files with CRLF line ends (its checkout); the texts here use LF, as the upstream source files
//! do. When Codex changes one of them, update it here together with the version, and run the
//! live test `live_codex_texts_match_the_installed_binary`.

/// The codex-cli version the texts were taken from.
pub const CODEX_TEXTS_VERSION: &str = "0.148.0";

/// The prompt the TUI's `/init` sends (`codex-rs/tui/prompt_for_init_command.md`).
pub const INIT_PROMPT: &str = "Generate a file named AGENTS.md that serves as a contributor guide for this repository.
Before writing, check whether AGENTS.md already exists in the current working directory. If it does, do not overwrite or modify it.
Your goal is to produce a clear, concise, and well-structured document with descriptive headings and actionable explanations for each section.
Follow the outline below, but adapt as needed \u{2014} add sections if relevant, and omit those that do not apply to this project.

Document Requirements

- Title the document \"Repository Guidelines\".
- Use Markdown headings (#, ##, etc.) for structure.
- Keep the document concise. 200-400 words is optimal.
- Keep explanations short, direct, and specific to this repository.
- Provide examples where helpful (commands, directory paths, naming patterns).
- Maintain a professional, instructional tone.

Recommended Sections

Project Structure & Module Organization

- Outline the project structure, including where the source code, tests, and assets are located.

Build, Test, and Development Commands

- List key commands for building, testing, and running locally (e.g., npm test, make build).
- Briefly explain what each command does.

Coding Style & Naming Conventions

- Specify indentation rules, language-specific style preferences, and naming patterns.
- Include any formatting or linting tools used.

Testing Guidelines

- Identify testing frameworks and coverage requirements.
- State test naming conventions and how to run tests.

Commit & Pull Request Guidelines

- Summarize commit message conventions found in the project\u{2019}s Git history.
- Outline pull request requirements (descriptions, linked issues, screenshots, etc.).

(Optional) Add other sections if relevant, such as Security & Configuration Tips, Architecture Overview, or Agent-Specific Instructions.
";

/// What the TUI sends, in the default collaboration mode, when the user chooses to implement a
/// proposed plan in the same thread ("Yes, implement this plan").
pub const IMPLEMENT_PLAN_PROMPT: &str = "Implement the plan.";

/// What the TUI puts before the plan's text (with a blank line between them) when the user
/// chooses to implement a proposed plan in a fresh thread ("Yes, clear context and implement").
pub const NEW_THREAD_PREAMBLE: &str = "A previous agent produced the plan below to accomplish the user's task. Implement the plan in a fresh context. Treat the plan as the source of user intent, re-read files as needed, and carry the work through implementation and verification.";

/// Every text with its name, for checks against a Codex binary.
pub const ALL: [(&str, &str); 3] = [
    ("init prompt", INIT_PROMPT),
    ("implement prompt", IMPLEMENT_PLAN_PROMPT),
    ("new thread preamble", NEW_THREAD_PREAMBLE),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_plain_lf_text() {
        for (name, text) in ALL {
            assert!(!text.contains('\r'), "{name} has CR");
            assert!(!text.trim().is_empty(), "{name} is empty");
        }
        assert!(INIT_PROMPT.starts_with("Generate a file named AGENTS.md"));
        assert!(INIT_PROMPT.ends_with("Agent-Specific Instructions.\n"));
        assert!(INIT_PROMPT.contains("adapt as needed \u{2014} add sections"));
        assert!(INIT_PROMPT.contains("project\u{2019}s Git history"));
    }
}
