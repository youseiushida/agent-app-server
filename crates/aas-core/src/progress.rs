//! Display lines of a tool's output, for operation progress.
//!
//! Progress-reporting tools (git with `--progress`) rewrite their current line with `\r` and
//! end finished lines with `\n`. Both are treated as line terminators, which is how a terminal
//! displays them. The text of a line is relayed as it is: it is never parsed or interpreted
//! (reading percentages or phases out of it would be a heuristic, see CLAUDE.md).

/// Splits streamed output into display lines and remembers nothing but the partial line.
#[derive(Debug)]
pub struct ProgressLines {
    partial: Vec<u8>,
    max_bytes: usize,
}

impl ProgressLines {
    /// `max_bytes` bounds one line (`policy.max_progress_line_bytes`); longer lines are cut.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            partial: Vec::new(),
            max_bytes: max_bytes.max(1),
        }
    }

    /// Feeds a chunk; returns the last complete, non-blank line it finished (earlier lines of
    /// the same chunk are already superseded).
    pub fn push(&mut self, bytes: &[u8]) -> Option<String> {
        let mut latest = None;
        for &b in bytes {
            if b == b'\r' || b == b'\n' {
                if let Some(line) = self.take_line() {
                    latest = Some(line);
                }
            } else if self.partial.len() < self.max_bytes {
                self.partial.push(b);
            }
        }
        latest
    }

    fn take_line(&mut self) -> Option<String> {
        let raw = std::mem::take(&mut self.partial);
        let text = String::from_utf8_lossy(&raw);
        if text.trim().is_empty() {
            return None;
        }
        Some(cut_at_char_boundary(&text, self.max_bytes).to_owned())
    }
}

fn cut_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The text a terminal would show for `bytes`: each `\n`-terminated line displays only what
/// was written after its last `\r`. Blank lines are dropped. Used to report a failed tool's
/// stderr without the superseded progress updates.
pub fn terminal_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.split('\n')
        .filter_map(|line| line.split('\r').rev().find(|seg| !seg.trim().is_empty()))
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last `max_bytes` of `text` (cut at a character boundary, whole text when it fits).
pub fn tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriage_returns_and_newlines_both_end_a_line() {
        let mut lines = ProgressLines::new(1024);
        assert_eq!(
            lines.push(b"Cloning into 'x'...\n"),
            Some("Cloning into 'x'...".into())
        );
        assert_eq!(
            lines.push(b"Receiving objects:   1% (1/100)\rReceiving objects:  2"),
            Some("Receiving objects:   1% (1/100)".into())
        );
        // The rest of a line arrives in the next chunk.
        assert_eq!(
            lines.push(b"% (2/100)\r"),
            Some("Receiving objects:  2% (2/100)".into())
        );
        assert_eq!(
            lines.push(b"\r\n\n  \r"),
            None,
            "blank lines are not progress"
        );
        assert_eq!(
            lines.push(b"unterminated"),
            None,
            "an unfinished line is not shown yet"
        );
    }

    #[test]
    fn a_chunk_reports_only_its_last_line_and_keeps_split_utf8() {
        let mut lines = ProgressLines::new(1024);
        assert_eq!(lines.push(b"a\rb\rc\r"), Some("c".into()));
        let text = "remote: \u{00e9}t\u{00e9}\n".as_bytes();
        assert_eq!(lines.push(&text[..9]), None);
        assert_eq!(
            lines.push(&text[9..]),
            Some("remote: \u{00e9}t\u{00e9}".into())
        );
    }

    #[test]
    fn long_lines_are_cut() {
        let mut lines = ProgressLines::new(8);
        assert_eq!(lines.push(b"0123456789abcdef\n"), Some("01234567".into()));
        assert_eq!(
            lines.push("\u{00e9}\u{00e9}\u{00e9}\u{00e9}\u{00e9}\n".as_bytes()),
            Some("\u{00e9}\u{00e9}\u{00e9}\u{00e9}".into())
        );
    }

    #[test]
    fn terminal_text_keeps_what_a_terminal_shows() {
        let stderr = b"Cloning into 'x'...\nremote: Counting: 50%\rremote: Counting: 100%, done.\n\nfatal: Authentication failed\n";
        assert_eq!(
            terminal_text(stderr),
            "Cloning into 'x'...\nremote: Counting: 100%, done.\nfatal: Authentication failed"
        );
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("\u{00e9}\u{00e9}", 3), "\u{00e9}");
    }
}
