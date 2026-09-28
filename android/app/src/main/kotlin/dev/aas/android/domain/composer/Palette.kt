package dev.aas.android.domain.composer

import dev.aas.android.R
import dev.aas.android.protocol.Command
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.CommandSource
import dev.aas.android.protocol.PickerKind
import dev.aas.android.ui.common.UiText

/**
 * Commands the app implements without the daemon (docs/ux/codex-desktop.md §8.5 "アプリ側").
 * They are added to the `/` palette next to `command/list`.
 */
enum class LocalCommand(val commandName: String) {
    /** A new thread in the same project with the same harness. */
    New("new"),

    /** Thread id, native session id, context usage and process state in a sheet. */
    Status("status"),

    /** Rename dialog → `thread/update { title }`. */
    Rename("rename"),

    /** Pin / unpin → `thread/update { pinned }`. */
    Pin("pin"),

    /** Inserts a review request (for harnesses without a `review` command). */
    Review("review"),

    /** Inserts the AGENTS.md prompt (for harnesses without an `init` command). */
    Init("init"),
}

/** What choosing a palette entry does. */
sealed interface PaletteAction {
    /** `insertText`: the text goes into the composer (the harness runs it when sent). */
    data class Insert(val text: String) : PaletteAction

    /** `method`: a protocol call; the app adds `clientRequestId` and `threadId`. */
    data class Method(val action: CommandAction.Method) : PaletteAction

    /** `picker`: opens the model / effort / permission picker. */
    data class Picker(val kind: PickerKind) : PaletteAction

    data class Local(val command: LocalCommand) : PaletteAction

    /** An action type this app does not know (a newer daemon): listed, not runnable. */
    data class Unsupported(val type: String) : PaletteAction
}

enum class PaletteSource { App, Local, Harness }

data class PaletteEntry(
    val name: String,
    val source: PaletteSource,
    val description: UiText?,
    val argumentHint: String?,
    val action: PaletteAction,
) {
    val runnable: Boolean get() = action !is PaletteAction.Unsupported
}

/** Where the palette is shown. */
data class PaletteContext(
    /** An existing thread (`/new`, `/status`, `/rename`, `/pin` apply); false in the new-thread composer. */
    val inThread: Boolean,
    /** The thread is pinned (the `/pin` entry says whether it pins or unpins). */
    val pinned: Boolean = false,
)

object Palette {
    /**
     * The palette entries: the daemon's app commands (`source: "app"`), the app's own commands,
     * then the harness commands (`source: "harness"`). A local command is left out when the
     * daemon already lists a command of that name (Claude's `/init`, Codex's `review`).
     *
     * Each (source, name) appears once, the first listed: a harness may list two commands of one
     * name (Codex adds `compact` and `review` and then one command per skill under the skill's
     * own name, and the daemon passes harness lists through), and a name identifies the entry.
     */
    fun entries(commands: List<Command>, context: PaletteContext): List<PaletteEntry> {
        val names = commands.map { it.name }.toSet()
        val app = commands.filter { it.source != CommandSource.Harness }.map(::fromServer)
        val harness = commands.filter { it.source == CommandSource.Harness }.map(::fromServer)
        val local = LocalCommand.entries
            .filter { it.commandName !in names }
            .filter { context.inThread || (it == LocalCommand.Review || it == LocalCommand.Init) }
            .map { command -> PaletteEntry(command.commandName, PaletteSource.Local, localDescription(command, context), null, PaletteAction.Local(command)) }
        return (app + local + harness).distinctBy { it.source to it.name }
    }

    /** Entries whose name starts with [query] first, then those containing it (case-insensitive). */
    fun filter(entries: List<PaletteEntry>, query: String): List<PaletteEntry> {
        if (query.isEmpty()) return entries
        val q = query.lowercase()
        val prefix = entries.filter { it.name.lowercase().startsWith(q) }
        val contains = entries.filter { it !in prefix && it.name.lowercase().contains(q) }
        return prefix + contains
    }

    private fun fromServer(command: Command): PaletteEntry {
        val action = when (val a = command.action) {
            is CommandAction.InsertText -> PaletteAction.Insert(a.text)
            is CommandAction.Method -> PaletteAction.Method(a)
            is CommandAction.Picker -> PaletteAction.Picker(a.picker)
            is CommandAction.Unknown -> PaletteAction.Unsupported(a.type)
        }
        val source = if (command.source == CommandSource.Harness) PaletteSource.Harness else PaletteSource.App
        val description = if (source == PaletteSource.App) appDescription(command.name) else null
        return PaletteEntry(command.name, source, description ?: command.description?.let { UiText.Plain(it) }, command.argumentHint, action)
    }

    /** Japanese descriptions of the daemon's app commands (its own descriptions are English). */
    private fun appDescription(name: String): UiText? = when (name) {
        "model" -> UiText.of(R.string.command_model)
        "effort" -> UiText.of(R.string.command_effort)
        "permissions" -> UiText.of(R.string.command_permissions)
        "fork" -> UiText.of(R.string.command_fork)
        "diff" -> UiText.of(R.string.command_diff)
        "stop" -> UiText.of(R.string.command_stop)
        "resume-queue" -> UiText.of(R.string.command_resume_queue)
        "archive" -> UiText.of(R.string.command_archive)
        else -> null
    }

    private fun localDescription(command: LocalCommand, context: PaletteContext): UiText = when (command) {
        LocalCommand.New -> UiText.of(R.string.command_new)
        LocalCommand.Status -> UiText.of(R.string.command_status)
        LocalCommand.Rename -> UiText.of(R.string.command_rename)
        LocalCommand.Pin -> UiText.of(if (context.pinned) R.string.command_unpin else R.string.command_pin)
        LocalCommand.Review -> UiText.of(R.string.command_review)
        LocalCommand.Init -> UiText.of(R.string.command_init)
    }
}
