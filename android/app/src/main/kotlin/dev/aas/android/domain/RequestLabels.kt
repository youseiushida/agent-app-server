package dev.aas.android.domain

import android.content.res.Resources
import androidx.annotation.StringRes
import dev.aas.android.R
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.RpcError
import dev.aas.android.sync.OutboxEntry

/** What a request (by method name) does, in the user's words: "メッセージの送信" etc. */
object RequestLabels {
    @StringRes
    fun of(method: String): Int = when (method) {
        Methods.TurnStart.name -> R.string.request_turn_start
        Methods.TurnInterrupt.name -> R.string.request_turn_interrupt
        Methods.InteractionRespond.name -> R.string.request_interaction_respond
        Methods.ThreadCreate.name -> R.string.request_thread_create
        Methods.ThreadUpdate.name -> R.string.request_thread_update
        Methods.ThreadArchive.name -> R.string.request_thread_archive
        Methods.ThreadFork.name -> R.string.request_thread_fork
        Methods.ThreadStop.name -> R.string.request_thread_stop
        Methods.BackgroundTaskStop.name -> R.string.request_background_stop
        Methods.QueueRemove.name -> R.string.request_queue_remove
        Methods.QueueResume.name -> R.string.request_queue_resume
        Methods.QueueUpdate.name -> R.string.request_queue_update
        Methods.QueueSteer.name -> R.string.request_queue_steer
        Methods.ProjectCreate.name -> R.string.request_project_create
        Methods.ProjectOpen.name -> R.string.request_project_open
        Methods.ProjectUpdate.name -> R.string.request_project_update
        Methods.ProjectArchive.name -> R.string.request_project_archive
        Methods.ProjectRemove.name -> R.string.request_project_remove
        Methods.FsMkdir.name -> R.string.request_fs_mkdir
        Methods.NativeImport.name -> R.string.request_native_import
        Methods.OperationCancel.name -> R.string.request_operation_cancel
        Methods.DeviceRevoke.name -> R.string.request_device_revoke
        else -> R.string.request_other
    }

    /** "メッセージの送信に失敗しました: <server message>". */
    fun failure(res: Resources, entry: OutboxEntry, error: RpcError): String =
        res.getString(R.string.request_failed, res.getString(of(entry.method)), error.message)
}
