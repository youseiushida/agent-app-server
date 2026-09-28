package dev.aas.android.ui

import androidx.lifecycle.viewModelScope
import dev.aas.android.R
import dev.aas.android.data.ProjectRepository
import dev.aas.android.data.ThreadRepository
import dev.aas.android.data.WorkspaceRepository
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.ThreadArchiveParams
import dev.aas.android.sync.Samples
import dev.aas.android.sync.eventually
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.MainDispatcherRule
import dev.aas.android.testing.TestEngine
import dev.aas.android.testing.blockingTest
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.projects.ProjectThreadsViewModel
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Rule
import org.junit.Test
import kotlin.test.assertEquals

/** A project's thread list: archiving with the snackbar's 元に戻す. */
class ProjectThreadsViewModelTest {
    @get:Rule
    val main = MainDispatcherRule()

    private val env = TestEngine()
    private val messages = UserMessages()
    private val jobs = mutableListOf<Job>()

    @After
    fun tearDown() {
        jobs.forEach { it.cancel() }
        env.close()
    }

    /**
     * The snackbar is shown and its action run by the app shell, which outlives the screen:
     * 元に戻す tapped after the user left the thread list (its view model cleared) still
     * un-archives the thread. It used to launch in the cleared view model's scope, which did
     * nothing at all.
     */
    @Test
    fun undoingAnArchiveWorksAfterTheThreadListIsGone() = blockingTest {
        val read = Fixtures.threadRead
        val projectId = read.thread.projectId
        env.seed(read, projects = listOf(Samples.project(projectId)))
        env.startOffline()
        val vm = withContext(Dispatchers.Main) {
            ProjectThreadsViewModel(projectId, WorkspaceRepository(env.engine), ThreadRepository(env.engine, env.reads), ProjectRepository(env.engine, env.reads), messages, 0L)
        }
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.state.collect {} }
        val row = eventually(what = "the thread row") { vm.state.value.rows.singleOrNull() }

        withContext(Dispatchers.Main) { vm.archive(row) }
        val message = withTimeout(MESSAGE_TIMEOUT_MS) { messages.messages.first() }
        assertEquals(UiText.of(R.string.thread_archived, row.title), message.text)
        assertEquals(UiText.of(R.string.undo), message.action)
        eventually(what = "thread/archive in the outbox") { env.engine.outbox.value.takeIf { it.size == 1 } }

        // Back to the project list: the view model is cleared before 元に戻す is tapped.
        withContext(Dispatchers.Main) { vm.viewModelScope.cancel() }
        val undo = message.onAction ?: throw AssertionError("the archive snackbar has no action")
        undo()

        val archived = eventually(what = "the undo in the outbox") {
            env.engine.outbox.value.filter { it.method == Methods.ThreadArchive.name }.takeIf { it.size == 2 }
        }.map { AasJson.decodeFromJsonElement(ThreadArchiveParams.serializer(), it.params) }
        assertEquals(listOf(true, false), archived.map { it.archived })
        assertEquals(listOf(row.id, row.id), archived.map { it.threadId })
    }

    private companion object {
        /** Upper bound for a view model's message to be posted. */
        const val MESSAGE_TIMEOUT_MS = 10_000L
    }
}
