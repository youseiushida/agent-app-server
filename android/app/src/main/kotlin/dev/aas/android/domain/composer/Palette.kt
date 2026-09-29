package dev.aas.android.domain.composer

import dev.aas.android.R
import dev.aas.android.protocol.Command
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.CommandSource
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.PickerKind
import dev.aas.android.ui.common.UiText

/**
 * Commands the app implements without the daemon (docs/ux/codex-desktop.md §8.5 "アプリ側").
 * They are added to the `/` palette next to `command/list`, and run instead of being sent when
 * typed as the first word of a message ([TypedCommands]).
 *
 * [aliases] are other names typed for the same command. [precedes]: the app's command wins over a
 * harness command of the same name (the palette leaves the harness's out, a typed one runs the
 * app's): the thread's own management (new, status, rename, pin) and the harness features the
 * app drives itself (plan mode, side questions). The prompt templates yield to a harness command
 * of their name (Claude's `/init`, Codex's `/review`).
 */
enum class LocalCommand(val commandName: String, val aliases: List<String> = emptyList(), val precedes: Boolean = false) {
    /**
     * A new thread in the same project with the same harness. `/clear` and `/reset` are the same
     * command: in a CLI they start a new session, which in the app is a new thread (one thread is
     * one native session, design.md §9.5).
     */
    New("new", aliases = listOf("clear", "reset"), precedes = true),

    /** Thread id, native session id, context usage, process state and the harness's own status in a sheet. */
    Status("status", precedes = true),

    /** `/rename <title>` renames at once; without a title the rename dialog → `thread/update { title }`. */
    Rename("rename", precedes = true),

    /** Pin / unpin → `thread/update { pinned }`. */
    Pin("pin", precedes = true),

    /**
     * `/plan [request]`: plan mode (`thread/update { modes: { plan: true } }`), then the request as
     * a message. Only where the harness offers the app's plan mode (`features.planMode`); a
     * harness's own `/plan` elsewhere (Devin, a pi extension) stays the harness's.
     */
    Plan("plan", precedes = true),

    /**
     * `/btw <question>`: a question beside the conversation (`thread/sideQuestion`), answered in a
     * sheet and never added to the history. Only with the feature `sideQuestion`.
     */
    Btw("btw", precedes = true),

    /** Inserts a review request (for harnesses without a `review` command). */
    Review("review"),

    /** Inserts the AGENTS.md prompt (for harnesses without an `init` command). */
    Init("init"),

    /**
     * 「PC のセッションを取り込む」 for this project, with this harness preselected (only when some
     * harness can list its sessions, [PaletteContext.canImport]). It replaces the harnesses' own
     * `/resume`, which picks a session in the CLI's terminal UI that a daemon session cannot show:
     * that one is never offered or sent ([Palette.entries], [Palette.isResume]).
     */
    Resume("resume", precedes = true),
    ;

    /** The name and the aliases. */
    val names: List<String> get() = listOf(commandName) + aliases
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
    val argumentHint: UiText?,
    val action: PaletteAction,
    /** Other names typed for the same entry (filtering matches them too). */
    val aliases: List<String> = emptyList(),
) {
    val runnable: Boolean get() = action !is PaletteAction.Unsupported
}

/** Where the palette is shown. */
data class PaletteContext(
    /** An existing thread (`/new`, `/status`, `/rename`, `/pin` apply); false in the new-thread composer. */
    val inThread: Boolean,
    /** The thread is pinned (the `/pin` entry says whether it pins or unpins). */
    val pinned: Boolean = false,
    /** Some harness can list its own sessions (`/resume` applies, see NativeSessionHarnesses.canImport). */
    val canImport: Boolean = false,
    /** The harness offers the app's plan mode (`features.planMode`): `/plan` applies. */
    val planMode: Boolean = false,
    /** The harness answers side questions (`features.sideQuestion`): `/btw` applies in a thread. */
    val sideQuestion: Boolean = false,
)

object Palette {
    /**
     * The palette entries: the daemon's app commands (`source: "app"`), the app's own commands,
     * then the harness commands (`source: "harness"`).
     *
     * * A local command that [LocalCommand.precedes] leaves the harness commands of its names
     *   out (Claude's and Devin's `rename`, Devin's `status`); the prompt templates are left out
     *   when the daemon lists a command of their name (Claude's `/init`, Codex's `review`).
     * * A harness command named `resume`, `clear` or `reset` is never offered: the app's own
     *   `/resume` and `/new` replace them (the daemon stops listing them; one that still does is
     *   ignored here).
     * * Each (source, name) appears once, the first listed: a harness may list two commands of one
     *   name (Codex adds `compact` and `review` and then one command per skill under the skill's
     *   own name, and the daemon passes harness lists through), and a name identifies the entry.
     */
    fun entries(commands: List<Command>, context: PaletteContext): List<PaletteEntry> {
        val names = commands.map { it.name }.toSet()
        val local = LocalCommand.entries
            .filter { applies(it, context) }
            .filter { it.precedes || it.commandName !in names }
        val shadowed = local.filter { it.precedes }.flatMap { it.names }.toSet() + ALWAYS_THE_APPS
        val offered = commands.filterNot { it.source == CommandSource.Harness && it.name in shadowed }
        val app = offered.filter { it.source != CommandSource.Harness }.map(::fromServer)
        val harness = offered.filter { it.source == CommandSource.Harness }.map(::fromServer)
        val localEntries = local.map { command ->
            PaletteEntry(command.commandName, PaletteSource.Local, localDescription(command, context), localHint(command), PaletteAction.Local(command), command.aliases)
        }
        return (app + localEntries + harness).distinctBy { it.source to it.name }
    }

    /**
     * The daemon's app commands as protocol.md §4 `command/list` defines them, for deciding what a
     * typed first word is while the daemon's list is not loaded (offline, or never opened): the
     * pickers when the harness lists their values, and in a thread the `method` commands (`fork`
     * when the harness can fork). They are the same names and actions the daemon lists; the
     * daemon never lists a harness command of these names.
     */
    fun protocolAppCommands(harness: Harness?, inThread: Boolean): List<Command> = buildList {
        fun picker(name: String, kind: PickerKind) = Command(name, null, CommandSource.App, action = CommandAction.Picker(kind))
        fun method(name: String, method: String) = Command(name, null, CommandSource.App, action = CommandAction.Method(method))
        if (harness != null && harness.models.isNotEmpty()) add(picker("model", PickerKind.Model))
        if (harness != null && harness.effortLevels.isNotEmpty()) add(picker("effort", PickerKind.Effort))
        if (harness != null && harness.permissionModes.isNotEmpty()) add(picker("permissions", PickerKind.PermissionMode))
        if (inThread) {
            if (harness?.capabilities?.fork == true) add(method("fork", Methods.ThreadFork.name))
            add(method("diff", Methods.ThreadDiff.name))
            add(method("stop", Methods.ThreadStop.name))
            add(method("resume-queue", Methods.QueueResume.name))
            add(method("archive", Methods.ThreadArchive.name))
        }
    }

    /**
     * Whether a draft about to be sent is `/resume` typed out instead of chosen from the palette
     * (its first word; anything after it is not sent either). It runs the app's `/resume` and is
     * never sent: a harness's `/resume` must not reach the harness ([LocalCommand.Resume]).
     */
    fun isResume(text: String): Boolean = TypedCommands.split(text)?.first == LocalCommand.Resume.commandName

    /** Where each of the app's commands is offered. */
    private fun applies(command: LocalCommand, context: PaletteContext): Boolean = when (command) {
        LocalCommand.Review, LocalCommand.Init -> true
        LocalCommand.Resume -> context.canImport
        LocalCommand.Plan -> context.planMode
        LocalCommand.Btw -> context.inThread && context.sideQuestion
        LocalCommand.New, LocalCommand.Status, LocalCommand.Rename, LocalCommand.Pin -> context.inThread
    }

    /**
     * Entries whose name (or an alias) starts with [query] first, then those containing it
     * (case-insensitive).
     */
    fun filter(entries: List<PaletteEntry>, query: String): List<PaletteEntry> {
        if (query.isEmpty()) return entries
        val q = query.lowercase()
        fun PaletteEntry.names() = listOf(name) + aliases
        val prefix = entries.filter { e -> e.names().any { it.lowercase().startsWith(q) } }
        val contains = entries.filter { e -> e !in prefix && e.names().any { it.lowercase().contains(q) } }
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
        return PaletteEntry(
            command.name,
            source,
            description ?: command.description?.let { UiText.Plain(it) },
            command.argumentHint?.let { UiText.Plain(it) },
            action,
        )
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
        LocalCommand.Plan -> UiText.of(R.string.command_plan)
        LocalCommand.Btw -> UiText.of(R.string.command_btw)
        LocalCommand.Review -> UiText.of(R.string.command_review)
        LocalCommand.Init -> UiText.of(R.string.command_init)
        LocalCommand.Resume -> UiText.of(R.string.command_resume)
    }

    /** What may follow the app's own commands. */
    private fun localHint(command: LocalCommand): UiText? = when (command) {
        LocalCommand.New -> UiText.of(R.string.command_hint_new)
        LocalCommand.Rename -> UiText.of(R.string.command_hint_rename)
        LocalCommand.Plan -> UiText.of(R.string.command_hint_plan)
        LocalCommand.Btw -> UiText.of(R.string.command_hint_btw)
        LocalCommand.Status, LocalCommand.Pin, LocalCommand.Review, LocalCommand.Init, LocalCommand.Resume -> null
    }

    /**
     * Harness command names the app always replaces: the CLIs' `resume` (a session picker in their
     * terminal UI) and `clear` / `reset` (a new session: the app's `/new`). All of them would move
     * the thread's agent to another session (design.md §9.5).
     */
    private val ALWAYS_THE_APPS = setOf(LocalCommand.Resume.commandName) + LocalCommand.New.aliases
}

/** A command typed as the first word of a draft: the app runs it instead of sending the draft. */
data class TypedCommand(
    /** The palette entry of the app's command (its own, or the daemon's `source: "app"` one). */
    val entry: PaletteEntry,
    /** The name as typed (the entry's name or an alias). */
    val typedName: String,
    /** What followed the first word, trimmed. */
    val args: String,
)

/**
 * The first word of a draft about to be sent (docs/android.md 25章 「打ったコマンド」): when it names
 * one of the app's commands in this composer's palette — the app's own commands (by name or
 * alias) and the daemon's app commands — the app runs that command instead of sending the text,
 * with what follows as its argument. Harness commands and plain text are sent as they are.
 */
object TypedCommands {
    /** `/name` as the first word of [text] (leading whitespace ignored) and what follows it (trimmed). */
    fun split(text: String): Pair<String, String>? {
        val t = text.trimStart()
        if (!t.startsWith('/')) return null
        val end = t.indexOfFirst { it.isWhitespace() }.let { if (it < 0) t.length else it }
        val name = t.substring(1, end)
        if (name.isEmpty()) return null
        return name to t.substring(end).trim()
    }

    /** The app's command [text] starts with, among the palette [entries] of the composer, or `null`. */
    fun resolve(text: String, entries: List<PaletteEntry>): TypedCommand? {
        val (name, args) = split(text) ?: return null
        val entry = entries.firstOrNull { it.source != PaletteSource.Harness && (it.name == name || name in it.aliases) } ?: return null
        return TypedCommand(entry, name, args)
    }

    /**
     * The app's command [text] starts with, in a composer whose palette is built from [loaded]
     * (the daemon's `command/list`, `null` while it is not loaded: offline, loading or failed)
     * or else [fallback] ([Palette.protocolAppCommands]).
     *
     * The prompt templates (`/review`, `/init`, which do not [LocalCommand.precedes]) yield to a
     * harness command of their name, and only the loaded list says whether there is one. Without
     * it they are not resolved: the text goes as typed, so the daemon runs the harness's own
     * command where it has one (Codex's `/review <text>` and `/init`, Claude's). What a typed
     * first word does never depends on whether the list happened to load.
     */
    fun resolve(text: String, loaded: List<Command>?, fallback: List<Command>, context: PaletteContext): TypedCommand? {
        val typed = resolve(text, Palette.entries(loaded ?: fallback, context)) ?: return null
        val local = (typed.entry.action as? PaletteAction.Local)?.command
        return if (loaded == null && local != null && !local.precedes) null else typed
    }
}
