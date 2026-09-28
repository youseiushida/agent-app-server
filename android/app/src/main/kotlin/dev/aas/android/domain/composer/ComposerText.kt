package dev.aas.android.domain.composer

import dev.aas.android.protocol.InputPart

/** What the text at the cursor asks the composer to show (docs/ux/codex-desktop.md §2.2, §2.3). */
sealed interface ComposerTrigger {
    /** `/name` at the very start of the message: the command palette, filtered by [query]. */
    data class Slash(val query: String, val end: Int) : ComposerTrigger

    /** `@query` at the cursor: file mention search; the token spans [start] until [end]. */
    data class Mention(val query: String, val start: Int, val end: Int) : ComposerTrigger
}

/** The composer's text with the cursor, independent of the UI toolkit. */
data class ComposerTextState(val text: String, val cursor: Int) {
    init {
        require(cursor in 0..text.length) { "cursor $cursor outside 0..${text.length}" }
    }
}

/**
 * Pure text operations of the composer: which popup the cursor asks for, inserting a chosen
 * command or mention, and turning the draft into `turn/start` input.
 */
object ComposerText {
    /**
     * The trigger at the cursor:
     *
     * * `/` as the first character with the cursor still in that first word: the palette
     *   (commands are only meaningful at the start of a message).
     * * `@` at the start of a word, with the cursor in the rest of that word: a mention search.
     */
    fun trigger(state: ComposerTextState): ComposerTrigger? {
        val text = state.text
        val cursor = state.cursor
        if (text.startsWith('/')) {
            val firstBreak = text.indexOfFirst { it.isWhitespace() }.let { if (it < 0) text.length else it }
            if (cursor in 1..firstBreak) return ComposerTrigger.Slash(text.substring(1, cursor), firstBreak)
        }
        // The word the cursor is in (or right after).
        var start = cursor
        while (start > 0 && !text[start - 1].isWhitespace()) start--
        if (start < text.length && text[start] == '@' && cursor > start) {
            var end = cursor
            while (end < text.length && !text[end].isWhitespace()) end++
            return ComposerTrigger.Mention(text.substring(start + 1, cursor), start, end)
        }
        return null
    }

    /** Replaces [start] until [end] with [replacement]; the cursor goes after it. */
    fun replace(state: ComposerTextState, start: Int, end: Int, replacement: String): ComposerTextState {
        val text = state.text.substring(0, start) + replacement + state.text.substring(end)
        return ComposerTextState(text, start + replacement.length)
    }

    /**
     * Inserts a command's text for a palette choice: replaces the typed `/query` token (the
     * command text normally ends with a space so arguments can follow).
     */
    fun insertCommand(state: ComposerTextState, trigger: ComposerTrigger.Slash, commandText: String): ComposerTextState {
        // Whitespace after the token is kept only when the command text does not already end with it.
        val end = if (commandText.endsWith(' ') && state.text.getOrNull(trigger.end) == ' ') trigger.end + 1 else trigger.end
        return replace(state, 0, end, commandText)
    }

    /** Replaces the `@query` token with `@path ` (a mention of [path]). */
    fun insertMention(state: ComposerTextState, trigger: ComposerTrigger.Mention, path: String): ComposerTextState {
        val end = if (state.text.getOrNull(trigger.end) == ' ') trigger.end + 1 else trigger.end
        return replace(state, trigger.start, end, "${mentionToken(path)} ")
    }

    /** How a mention appears in the text. */
    fun mentionToken(path: String): String = "@$path"

    /** Appends [block] as its own paragraph (inline diff comments, templates). */
    fun appendParagraph(text: String, block: String): String = when {
        text.isBlank() -> block
        text.endsWith("\n\n") -> text + block
        text.endsWith('\n') -> text + "\n" + block
        else -> text + "\n\n" + block
    }

    /**
     * The `turn/start` input for a draft: the text (trimmed at its ends) with each chosen
     * mention's `@path` token, where it stands as a whole word, replaced by a `mention` part in
     * its place, then the images in the order they were attached. The daemon writes a mention
     * part into the message as `@path` (protocol.md §3 `InputPart`), so the token must not stay
     * in the text as well: "see @a.rs please" becomes text "see ", mention "a.rs", text
     * " please". A message of only whitespace has no text part; mentions no longer in the text
     * are left out; at one position the longest chosen path wins.
     */
    fun input(text: String, mentions: Collection<String>, images: List<String>): List<InputPart> {
        val trimmed = text.trim()
        val chosen = mentions.distinct().sortedByDescending { it.length }
        val parts = ArrayList<InputPart>()
        var emitted = 0
        var i = 0
        while (i < trimmed.length) {
            val atWordStart = trimmed[i] == '@' && (i == 0 || trimmed[i - 1].isWhitespace())
            val path = if (atWordStart) chosen.firstOrNull { tokenAt(trimmed, i, mentionToken(it)) } else null
            if (path == null) {
                i++
                continue
            }
            if (i > emitted) parts += InputPart.Text(trimmed.substring(emitted, i))
            parts += InputPart.Mention(path)
            i += mentionToken(path).length
            emitted = i
        }
        if (emitted < trimmed.length) parts += InputPart.Text(trimmed.substring(emitted))
        images.forEach { parts += InputPart.Image(it) }
        return parts
    }

    /** [token] starts at [at] and ends a word there (not the prefix of a longer path). */
    private fun tokenAt(text: String, at: Int, token: String): Boolean {
        if (!text.startsWith(token, at)) return false
        val after = text.getOrNull(at + token.length)
        return after == null || after.isWhitespace() || after in TOKEN_END_PUNCTUATION
    }

    /** Punctuation that may directly follow a mention token in prose ("see @a.rs, then …"). */
    private const val TOKEN_END_PUNCTUATION = ",.;:!?)"

    /**
     * The message text of an input, as the daemon writes it (for editing a queued message and
     * showing a pending one): text parts as they are, each mention as its `@path` token in its
     * place (after a space when the text before it does not end with one), images left out.
     */
    fun textOf(input: List<InputPart>): String {
        val out = StringBuilder()
        for (part in input) {
            when (part) {
                is InputPart.Text -> out.append(part.text)
                is InputPart.Mention -> {
                    if (out.isNotEmpty() && !out.last().isWhitespace()) out.append(' ')
                    out.append(mentionToken(part.path))
                }
                is InputPart.Image, is InputPart.Unknown -> Unit
            }
        }
        return out.toString()
    }
}
