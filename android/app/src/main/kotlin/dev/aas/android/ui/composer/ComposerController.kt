package dev.aas.android.ui.composer

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import dev.aas.android.AppPolicy
import dev.aas.android.R
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.data.Draft
import dev.aas.android.data.DraftImage
import dev.aas.android.data.ImageUploadException
import dev.aas.android.data.ImageUploader
import dev.aas.android.data.UploadedImage
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.composer.ComposerText
import dev.aas.android.domain.composer.ComposerTextState
import dev.aas.android.domain.composer.ComposerTrigger
import dev.aas.android.domain.composer.LocalCommand
import dev.aas.android.domain.composer.Palette
import dev.aas.android.domain.composer.PaletteAction
import dev.aas.android.domain.composer.PaletteContext
import dev.aas.android.domain.composer.PaletteEntry
import dev.aas.android.domain.composer.TypedCommand
import dev.aas.android.domain.composer.TypedCommands
import dev.aas.android.protocol.Command
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.PickerKind
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.SearchResult
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.requestFailed
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import java.util.UUID

/** The daemon's command list for the palette. */
sealed interface CommandsState {
    data object Idle : CommandsState

    data object Loading : CommandsState

    data class Loaded(val commands: List<Command>) : CommandsState

    /** Not connected: only the app's own commands are listed. */
    data object Offline : CommandsState

    data class Failed(val message: UiText) : CommandsState
}

/** Results of the `@` mention search. */
sealed interface MentionSearch {
    /** `@` alone: type to search. */
    data object Prompt : MentionSearch

    data object Loading : MentionSearch

    data class Results(val query: String, val results: List<SearchResult>) : MentionSearch

    data object Offline : MentionSearch

    data class Failed(val message: UiText) : MentionSearch
}

/**
 * An attached image. [localUri] is the picked content (its thumbnail); `null` for an image only
 * the daemon has (an attachment of a sent message put back, [DraftImage]).
 */
data class Attachment(val id: String, val localUri: String?, val state: State) {
    sealed interface State {
        data object Uploading : State

        data class Uploaded(val image: UploadedImage) : State

        data class Failed(val message: UiText) : State
    }
}

/** Templates the app inserts for its own `/review` and `/init` (resolved from resources). */
data class PromptTemplates(val review: String, val init: String)

/**
 * A draft as it was sent: its text, chosen mentions and uploaded images. Kept by the sender
 * until the request is safely made, to put everything back ([ComposerController.restore]) when
 * it is not.
 */
data class SentDraft(val text: String, val mentions: Set<String>, val images: List<DraftImage>) {
    /** As [ComposerDrafts] keeps drafts (to give it back later, [ComposerDrafts.giveBack]). */
    fun toDraft(): Draft = Draft(text, mentions, images)

    companion object {
        fun of(draft: Draft): SentDraft = SentDraft(draft.text, draft.mentions, draft.images)
    }
}

/** What the owner of the composer has to do after a palette choice. */
sealed interface PaletteChoice {
    /** Handled inside the composer (text inserted). */
    data object Inserted : PaletteChoice

    data class Method(val action: CommandAction.Method) : PaletteChoice

    data class Picker(val kind: PickerKind) : PaletteChoice

    data class Local(val command: LocalCommand) : PaletteChoice

    /** An action type of a newer daemon. */
    data class Unsupported(val type: String) : PaletteChoice
}

data class ComposerUiState(
    val value: TextFieldValue,
    val mentions: Set<String>,
    val attachments: List<Attachment>,
    val trigger: ComposerTrigger?,
    /** The filtered palette while `/` is typed. */
    val palette: List<PaletteEntry>,
    val commands: CommandsState,
    val mentionSearch: MentionSearch,
) {
    val uploading: Boolean get() = attachments.any { it.state is Attachment.State.Uploading }
    val uploadFailed: Boolean get() = attachments.any { it.state is Attachment.State.Failed }
    val uploaded: List<UploadedImage> get() = attachments.mapNotNull { (it.state as? Attachment.State.Uploaded)?.image }

    /** Something to send: text, or at least one uploaded image. */
    val hasContent: Boolean get() = value.text.isNotBlank() || uploaded.isNotEmpty()

    /** Images are attached (uploaded or not): the harness must take images. */
    val hasImages: Boolean get() = attachments.isNotEmpty()
}

/**
 * The composer's state and behaviour, shared by the thread screen and the new-thread screen
 * (docs/ux/codex-desktop.md §2, §8.1): the text with its cursor, the `/` palette from
 * `command/list` plus the app's own commands, `@` mentions through `fs/search`, and image
 * attachments uploaded as they are picked (`POST /v1/blobs`). The draft is kept in
 * [ComposerDrafts] under [draftKey] as it changes, and drafts given back there (a message the
 * daemon refused after it left the composer) are put back into this composer.
 */
class ComposerController(
    private val scope: CoroutineScope,
    private val uploader: ImageUploader,
    private val search: suspend (query: String) -> List<SearchResult>,
    private val policy: AppPolicy,
    private val drafts: ComposerDrafts,
    private val draftKey: String,
    private val templates: PromptTemplates,
    paletteContext: PaletteContext,
) {
    private val initial: Draft = drafts.get(draftKey)
    private val value = MutableStateFlow(TextFieldValue(initial.text, TextRange(initial.text.length)))

    /**
     * The text field's value as Compose state: the input shows it directly, so typing never
     * waits for the combined [state] (which drives the palette, mentions and send button).
     */
    var textValue: TextFieldValue by mutableStateOf(value.value)
        private set

    /** The mentions chosen for the current draft. */
    val currentMentions: Set<String> get() = mentions.value
    private val mentions = MutableStateFlow(initial.mentions)
    private val attachments = MutableStateFlow(initial.images.map { Attachment(UUID.randomUUID().toString(), it.localUri, Attachment.State.Uploaded(it.image)) })
    private val commands = MutableStateFlow<CommandsState>(CommandsState.Idle)
    private val context = MutableStateFlow(paletteContext)
    private val mentionSearch = MutableStateFlow<MentionSearch>(MentionSearch.Prompt)
    private val uploads = HashMap<String, Job>()
    private var searchJob: Job? = null
    private var searchedQuery: String? = null

    /** The text this composer last stored in [drafts]: a different draft text came from elsewhere. */
    private var savedText: String = initial.text

    val state: StateFlow<ComposerUiState> = combine(
        combine(value, mentions, attachments) { v, m, a -> Triple(v, m, a) },
        commands,
        context,
        mentionSearch,
    ) { (v, m, a), c, ctx, ms ->
        val trigger = triggerOf(v)
        val palette = if (trigger is ComposerTrigger.Slash) {
            Palette.filter(Palette.entries((c as? CommandsState.Loaded)?.commands.orEmpty(), ctx), trigger.query)
        } else {
            emptyList()
        }
        ComposerUiState(v, m, a, trigger, palette, c, ms)
    }.stateIn(scope, SharingStarted.Eagerly, ComposerUiState(value.value, mentions.value, attachments.value, triggerOf(value.value), emptyList(), CommandsState.Idle, MentionSearch.Prompt))

    init {
        // Text appended from elsewhere (the diff viewer's line comments) shows up here.
        scope.launch {
            drafts.changes(draftKey).collect { draft ->
                if (draft.text != savedText && draft.text != value.value.text) {
                    savedText = draft.text
                    update(TextFieldValue(draft.text, TextRange(draft.text.length)))
                    mentions.value = draft.mentions
                }
            }
        }
        // Messages refused after they left the composer (now, or while its screen was closed).
        scope.launch {
            drafts.returnedWaiting(draftKey).collect { waiting ->
                if (waiting) drafts.takeReturned(draftKey).forEach { restore(SentDraft.of(it)) }
            }
        }
    }

    fun setCommands(state: CommandsState) {
        commands.value = state
    }

    fun setPaletteContext(ctx: PaletteContext) {
        context.value = ctx
    }

    fun onValueChange(newValue: TextFieldValue) {
        update(newValue)
        onTextChanged()
    }

    /** Replaces the whole text (editing a queued message, a template). */
    fun setText(text: String) {
        update(TextFieldValue(text, TextRange(text.length)))
        onTextChanged()
    }

    /** Runs a palette entry: inserts text itself, or says what the owner has to do. */
    fun choose(entry: PaletteEntry): PaletteChoice {
        val trigger = triggerOf(value.value) as? ComposerTrigger.Slash
        val current = ComposerTextState(value.value.text, value.value.selection.end.coerceIn(0, value.value.text.length))
        fun insert(text: String) {
            val next = if (trigger != null) ComposerText.insertCommand(current, trigger, text) else ComposerText.replace(current, 0, 0, text)
            update(TextFieldValue(next.text, TextRange(next.cursor)))
        }
        fun dropToken() {
            if (trigger == null) return
            // The "/query" token and the space after it go; arguments typed after it stay.
            val end = if (current.text.getOrNull(trigger.end) == ' ') trigger.end + 1 else trigger.end
            val next = ComposerText.replace(current, 0, end, "")
            update(TextFieldValue(next.text, TextRange(next.cursor)))
        }
        val choice = when (val action = entry.action) {
            is PaletteAction.Insert -> {
                insert(action.text)
                PaletteChoice.Inserted
            }
            is PaletteAction.Local -> when (action.command) {
                // The argument is typed after it; sending runs the command (TypedCommands).
                LocalCommand.Plan, LocalCommand.Btw -> {
                    insert("/${action.command.commandName} ")
                    PaletteChoice.Inserted
                }
                LocalCommand.Review -> {
                    insert(templates.review)
                    PaletteChoice.Inserted
                }
                LocalCommand.Init -> {
                    insert(templates.init)
                    PaletteChoice.Inserted
                }
                else -> {
                    dropToken()
                    PaletteChoice.Local(action.command)
                }
            }
            is PaletteAction.Method -> {
                dropToken()
                PaletteChoice.Method(action.action)
            }
            is PaletteAction.Picker -> {
                dropToken()
                PaletteChoice.Picker(action.kind)
            }
            is PaletteAction.Unsupported -> PaletteChoice.Unsupported(action.type)
        }
        onTextChanged()
        return choice
    }

    /** Replaces the `@query` token with the chosen path and remembers the mention. */
    fun chooseMention(path: String) {
        val trigger = triggerOf(value.value) as? ComposerTrigger.Mention ?: return
        val current = ComposerTextState(value.value.text, value.value.selection.end.coerceIn(0, value.value.text.length))
        val next = ComposerText.insertMention(current, trigger, path)
        update(TextFieldValue(next.text, TextRange(next.cursor)))
        mentions.update { it + path }
        onTextChanged()
    }

    /**
     * Attaches picked images and uploads them at once (the blob id is what the message
     * references). Beyond [AppPolicy.maxImagesPerMessage] images the rest is left out.
     * Returns how many were left out.
     */
    fun addImages(uris: List<String>): Int {
        val room = (policy.maxImagesPerMessage - attachments.value.size).coerceAtLeast(0)
        val taken = uris.take(room)
        for (uri in taken) {
            val attachment = Attachment(UUID.randomUUID().toString(), uri, Attachment.State.Uploading)
            attachments.update { it + attachment }
            upload(attachment)
        }
        return uris.size - taken.size
    }

    fun retryAttachment(id: String) {
        val attachment = attachments.value.firstOrNull { it.id == id } ?: return
        if (attachment.state !is Attachment.State.Failed) return
        val retry = attachment.copy(state = Attachment.State.Uploading)
        attachments.update { list -> list.map { if (it.id == id) retry else it } }
        upload(retry)
    }

    fun removeAttachment(id: String) {
        uploads.remove(id)?.cancel()
        attachments.update { list -> list.filterNot { it.id == id } }
        saveDraft()
    }

    /** The `turn/start` input of the current draft. */
    fun input(): List<InputPart> = ComposerText.input(
        value.value.text,
        mentions.value,
        attachments.value.mapNotNull { (it.state as? Attachment.State.Uploaded)?.image?.blobId },
    )

    /** The draft about to be sent, for [restore] if the request cannot be made. */
    fun sentDraft(): SentDraft = SentDraft(value.value.text, mentions.value, uploadedImages())

    /**
     * The app's command the draft starts with ([TypedCommands.resolve]), resolved against this
     * composer's palette: the daemon's command list when it is loaded, else [fallback] (the
     * protocol's app commands, [Palette.protocolAppCommands]); the prompt templates only with the
     * loaded list.
     */
    fun typedCommand(fallback: List<Command>): TypedCommand? {
        val loaded = (commands.value as? CommandsState.Loaded)?.commands
        return TypedCommands.resolve(value.value.text, loaded, fallback, context.value)
    }

    /** The draft with [text] instead of its text (a typed command's argument, a template): its mentions and images stay. */
    fun draftWith(text: String): SentDraft = SentDraft(text, mentions.value, uploadedImages())

    /**
     * Puts text the harness asked for into the composer (`composer/insert`): in place of the text
     * when [replace] or when there is none, else as the next paragraph after it.
     */
    fun insertFromHarness(text: String, replace: Boolean) {
        val current = value.value.text
        val next = if (replace || current.isBlank()) text else ComposerText.appendParagraph(current, text)
        update(TextFieldValue(next, TextRange(next.length)))
        onTextChanged()
    }

    /** After sending: an empty composer and no draft. */
    fun clear() {
        uploads.values.forEach { it.cancel() }
        uploads.clear()
        update(TextFieldValue(""))
        mentions.value = emptySet()
        attachments.value = emptyList()
        mentionSearch.value = MentionSearch.Prompt
        searchedQuery = null
        savedText = ""
        drafts.clear(draftKey)
    }

    /**
     * Puts a sent draft back (the request could not be queued, or the daemon refused it): its
     * text, mentions and images, before anything typed or attached since (a refusal can come
     * long after the message left: what was written meanwhile stays, as the next paragraph).
     * The images are uploaded already (a blob stays on the daemon for days without a message
     * referencing it), so the draft can be sent again as it was.
     */
    fun restore(draft: SentDraft) {
        val typed = value.value.text
        val text = when {
            typed.isBlank() -> draft.text
            draft.text.isBlank() -> typed
            else -> ComposerText.appendParagraph(draft.text.trimEnd(), typed)
        }
        update(TextFieldValue(text, TextRange(text.length)))
        mentions.update { draft.mentions + it }
        val restored = draft.images.map { Attachment(UUID.randomUUID().toString(), it.localUri, Attachment.State.Uploaded(it.image)) }
        attachments.update { current -> restored + current }
        onTextChanged()
    }

    private fun update(newValue: TextFieldValue) {
        value.value = newValue
        textValue = newValue
    }

    private fun upload(attachment: Attachment) {
        // Only picked images upload: an image of a sent message put back is on the daemon already.
        val uri = attachment.localUri ?: return
        uploads[attachment.id] = scope.launch {
            val next = try {
                Attachment.State.Uploaded(uploader.upload(uri))
            } catch (e: CancellationException) {
                throw e
            } catch (e: ImageUploadException) {
                Attachment.State.Failed(describe(e))
            } catch (e: Exception) {
                Attachment.State.Failed(UiText.of(R.string.upload_failed, e.message ?: e.javaClass.simpleName))
            }
            attachments.update { list -> list.map { if (it.id == attachment.id) it.copy(state = next) else it } }
            uploads.remove(attachment.id)
            saveDraft()
        }
    }

    private fun onTextChanged() {
        saveDraft()
        val trigger = triggerOf(value.value)
        if (trigger !is ComposerTrigger.Mention) {
            searchJob?.cancel()
            searchedQuery = null
            mentionSearch.value = MentionSearch.Prompt
            return
        }
        val query = trigger.query
        if (query == searchedQuery) return
        searchedQuery = query
        searchJob?.cancel()
        if (query.isEmpty()) {
            mentionSearch.value = MentionSearch.Prompt
            return
        }
        mentionSearch.value = MentionSearch.Loading
        searchJob = scope.launch {
            delay(policy.mentionSearchDebounceMs)
            mentionSearch.value = try {
                MentionSearch.Results(query, search(query))
            } catch (e: CancellationException) {
                throw e
            } catch (e: NotConnectedException) {
                MentionSearch.Offline
            } catch (e: RpcException) {
                MentionSearch.Failed(ErrorTexts.server(e.error))
            } catch (e: Exception) {
                MentionSearch.Failed(requestFailed(e))
            }
        }
    }

    private fun uploadedImages(): List<DraftImage> =
        attachments.value.mapNotNull { a -> (a.state as? Attachment.State.Uploaded)?.let { DraftImage(a.localUri, it.image) } }

    private fun saveDraft() {
        savedText = value.value.text
        drafts.set(draftKey, Draft(value.value.text, mentions.value, uploadedImages()))
    }

    private fun triggerOf(v: TextFieldValue): ComposerTrigger? {
        // Only with a collapsed cursor (not while text is selected).
        if (!v.selection.collapsed) return null
        return ComposerText.trigger(ComposerTextState(v.text, v.selection.end.coerceIn(0, v.text.length)))
    }

    companion object {
        /** The message for a failed image upload. */
        fun describe(e: ImageUploadException): UiText = when (e) {
            is ImageUploadException.NotPaired -> UiText.of(R.string.upload_not_paired)
            is ImageUploadException.Unreadable -> UiText.of(R.string.upload_unreadable)
            is ImageUploadException.SourceTooLarge -> UiText.of(R.string.upload_source_too_large, e.limit / BYTES_PER_MIB)
            is ImageUploadException.TooLarge -> UiText.of(R.string.upload_too_large, e.limit / BYTES_PER_MIB)
            is ImageUploadException.Rejected -> UiText.of(R.string.upload_rejected, e.error.detail.ifEmpty { e.error.kind })
            is ImageUploadException.Network -> UiText.of(R.string.upload_network)
        }

        private const val BYTES_PER_MIB = 1024L * 1024
    }
}
