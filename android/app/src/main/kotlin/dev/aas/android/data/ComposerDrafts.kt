package dev.aas.android.data

import dev.aas.android.protocol.ProjectId
import dev.aas.android.protocol.ThreadId
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.getAndUpdate
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.update

/** An uploaded image of a draft, with the picked content URI for its thumbnail. */
data class DraftImage(val localUri: String, val image: UploadedImage)

/** A composer's unsent content. */
data class Draft(val text: String = "", val mentions: Set<String> = emptySet(), val images: List<DraftImage> = emptyList()) {
    val isEmpty: Boolean get() = text.isEmpty() && images.isEmpty()
}

/**
 * Unsent composer content per thread (and per project for the new-thread composer), kept for
 * the life of the process: leaving a thread and coming back keeps what was typed, and the diff
 * viewer's line comments are appended to the thread's draft (UX §8.2 インラインコメント). The
 * thread screen also saves its text in its saved state, which survives the process being killed
 * in the background.
 *
 * Drafts can also be given back ([giveBack]): a message that left the composer and was refused
 * by the daemon afterwards (see [SentDrafts]). They wait here until the composer of their key
 * takes them ([takeReturned]): at once while its screen is open, else when it opens next.
 */
class ComposerDrafts {
    private val drafts = MutableStateFlow<Map<String, Draft>>(emptyMap())
    private val returned = MutableStateFlow<Map<String, List<Draft>>>(emptyMap())

    fun get(key: String): Draft = drafts.value[key] ?: Draft()

    /** The draft of [key] as it changes (also through [appendParagraph]). */
    fun changes(key: String): Flow<Draft> = drafts.map { it[key] ?: Draft() }.distinctUntilChanged()

    fun set(key: String, draft: Draft) {
        drafts.update { if (draft.isEmpty) it - key else it + (key to draft) }
    }

    fun clear(key: String) {
        drafts.update { it - key }
    }

    /** Adds [block] as a paragraph after the draft's text. */
    fun appendParagraph(key: String, block: String) {
        drafts.update { all ->
            val draft = all[key] ?: Draft()
            val text = when {
                draft.text.isBlank() -> block
                draft.text.endsWith("\n\n") -> draft.text + block
                draft.text.endsWith('\n') -> draft.text + "\n" + block
                else -> draft.text + "\n\n" + block
            }
            all + (key to draft.copy(text = text))
        }
    }

    /** Gives [draft] back to the composer of [key] (after the ones given back before it). */
    fun giveBack(key: String, draft: Draft) {
        returned.update { it + (key to (it[key].orEmpty() + draft)) }
    }

    /** Whether drafts given back to [key] wait to be taken, as it changes. */
    fun returnedWaiting(key: String): Flow<Boolean> = returned.map { !it[key].isNullOrEmpty() }.distinctUntilChanged()

    /** Takes the drafts given back to [key], oldest first: they are the caller's now (each is taken once). */
    fun takeReturned(key: String): List<Draft> = returned.getAndUpdate { it - key }[key].orEmpty()

    companion object {
        fun threadKey(threadId: ThreadId): String = "thread:$threadId"

        fun newThreadKey(projectId: ProjectId): String = "new:$projectId"
    }
}
