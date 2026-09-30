package dev.aas.android.protocol

import kotlinx.serialization.KSerializer

/**
 * The protocol types a client keeps in its local store between runs, and the version of their
 * shape (docs/android.md 15.2).
 *
 * The app's store (Room) holds each of [TYPES] as JSON written with these classes, so a build
 * stores only what its classes know: fields the server sent that the build did not know are
 * dropped (an older build stored harnesses without `features`), and an enum value it did not
 * know is stored as `unknown`. A newer build cannot recover them from the stored copy, and a
 * resumed sync only replays what changed after the stored cursors. The store therefore records
 * the [VERSION] it was written with, and the sync engine reads everything again (a full resync,
 * as after an epoch change) when the recorded version is not this build's.
 *
 * [VERSION] changes with the shape of [TYPES]: a field, a union variant or an enum value added,
 * removed or renamed in any type they contain. `StoredModelsTest` records the shape of every
 * version (`android/protocol/stored-models/<version>.txt`) and fails when the classes no longer
 * have the shape recorded for [VERSION], so a change cannot ship without a new version.
 */
object StoredModels {
    /** The shape of [TYPES] in this build (see the class documentation). */
    const val VERSION: Int = 1

    /** Every protocol type the app's store keeps (`RoomSyncStore`), with what they contain. */
    val TYPES: List<KSerializer<*>> = listOf(
        Harness.serializer(),
        Project.serializer(),
        Thread.serializer(),
        Operation.serializer(),
        Turn.serializer(),
        Item.Serializer,
        Interaction.serializer(),
        BackgroundTask.serializer(),
        QueuedInput.serializer(),
    )
}
