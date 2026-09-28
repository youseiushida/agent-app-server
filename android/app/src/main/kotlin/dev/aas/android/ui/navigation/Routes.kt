package dev.aas.android.ui.navigation

import kotlinx.serialization.Serializable

/*
 * Typed routes of the navigation graph (Navigation Compose type-safe destinations). Arguments
 * are the route's properties; screens read them with `entry.toRoute<T>()`. Each feature
 * registers its routes with a `NavGraphBuilder.<feature>Destinations(navigator)` function called
 * from AasNavHost.
 */

/** Tab プロジェクト: the project list (start destination once paired). */
@Serializable
data object ProjectsRoute

/** The threads of one project. */
@Serializable
data class ProjectThreadsRoute(val projectId: String)

/**
 * A thread. [interactionId] focuses a pending interaction (a notification's approval or
 * question: the card is scrolled to, a question opens its answer sheet).
 * Deep link: `aas://thread/<threadId>[?interactionId=<id>]` ([DeepLinks.thread]).
 */
@Serializable
data class ThreadRoute(val threadId: String, val interactionId: String? = null)

/** New project: an existing folder, or a new one (empty, git init, git clone). */
@Serializable
data object NewProjectRoute

/**
 * A new thread of [projectId]: harness, model, effort, permission and workspace on one sheet,
 * with the first message ([harnessId]: preselected, e.g. by `/new`).
 */
@Serializable
data class NewThreadRoute(val projectId: String, val harnessId: String? = null)

/** The archived threads of a project (`thread/list` with `includeArchived`). */
@Serializable
data class ArchivedThreadsRoute(val projectId: String)

/** Import a harness's own session into the project (`native/list`, `native/import`). */
@Serializable
data class ImportSessionRoute(val projectId: String)

/** The diff of a turn ([turnId]) or of the whole thread (`thread/diff`). */
@Serializable
data class DiffRoute(val threadId: String, val turnId: String? = null)

/** The full output of a command or tool call (inline or its blob). */
@Serializable
data class ItemOutputRoute(val threadId: String, val itemId: String)

/** An image attachment in full. */
@Serializable
data class ImageRoute(val blobId: String)

/** Tab 要対応: pending approvals and questions, errors, unread threads. */
@Serializable
data object InboxRoute

/** Tab 設定. */
@Serializable
data object SettingsRoute

/** 設定 → デバイス: paired devices (`device/list`, `device/revoke`). */
@Serializable
data object DevicesRoute

/** 設定 → 診断: connection log, outbox, epoch, cursors. */
@Serializable
data object DiagnosticsRoute

/** Why and how to exempt the app from battery optimisation. */
@Serializable
data object BatteryRoute

/**
 * Pairing (QR or manual entry). [repair]: the device was paired before (revoked, token
 * rejected, unreadable); on success the screen pops back instead of starting the setup.
 * [link]: an `aas://pair` link opened from another app, to confirm.
 */
@Serializable
data class PairingRoute(val repair: Boolean = false, val link: String? = null)

/** After the first pairing: notification permission and battery optimisation, then the app. */
@Serializable
data object SetupRoute
