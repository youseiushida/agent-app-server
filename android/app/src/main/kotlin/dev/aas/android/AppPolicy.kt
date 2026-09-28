package dev.aas.android

/**
 * Policy values of the app (CLAUDE.md: timeouts, intervals and limits are named policy values
 * with the reason for their default, never literals scattered in the code). The sync engine's
 * own values are in `dev.aas.android.sync.SyncConfig`.
 */
data class AppPolicy(
    /**
     * TCP connect + TLS for the plain HTTP calls (pairing, blobs). Same bound as the engine's
     * `connectTimeoutMs`: a cold Tailscale DERP relay on a slow mobile network still connects.
     */
    val httpConnectTimeoutMs: Long = 20_000,
    /**
     * Longest silence while reading an HTTP response. The daemon answers pairing and blob
     * requests from its database or disk in milliseconds; a minute covers a stalled relay
     * without leaving the user staring at a spinner forever.
     */
    val httpReadTimeoutMs: Long = 60_000,
    /**
     * Longest stall while writing a request body. An image upload of up to 25 MiB keeps writing
     * continuously on a working link; a minute without progress means the path is dead.
     */
    val httpWriteTimeoutMs: Long = 60_000,
    /**
     * Upper bound of the whole `POST /v1/pair` call: a small JSON exchange; the pairing code
     * expires after five minutes on the server anyway.
     */
    val pairCallTimeoutMs: Long = 60_000,
    /**
     * How long a notification action (許可 / 拒否) may take to commit its request to the outbox.
     * A broadcast receiver has about ten seconds before the system considers it hung; the commit
     * is a single short transaction, so eight seconds only trips when the database is stuck.
     */
    val notificationActionTimeoutMs: Long = 8_000,
    /**
     * How long unpairing waits for `device/revoke` of this device before forgetting it locally.
     * Long enough for a round trip over a relay; if it does not complete, the user is told to
     * revoke the device on the PC.
     */
    val unpairRevokeTimeoutMs: Long = 10_000,
    /**
     * How long a read-only call whose connection ended before the answer waits for the engine to
     * be online again before it is sent a second time (data/Reads.kt). The engine reconnects by
     * itself (backoff from 500 ms, doubling): 15 s cover about five attempts, enough for a watchdog
     * reconnect or a network switch; a longer outage is shown as offline with 再試行 instead of a
     * spinner that waits for it.
     */
    val readReconnectWaitMs: Long = 15_000,
    /**
     * Entries kept in the in-memory connection log (diagnostics). Hundreds of lines cover
     * several days of normal reconnects and fit comfortably in memory.
     */
    val connectionLogCapacity: Int = 500,
    /**
     * How long view-model flows keep their upstream after the last collector left
     * (`SharingStarted.WhileSubscribed`): longer than a configuration change (rotation), so the
     * thread stays subscribed, short enough to release it when the screen is really gone.
     */
    val uiStopTimeoutMs: Long = 5_000,
    /**
     * How long the approve / deny buttons of a newly shown approval stay disabled. A card that
     * appears (or moves) under the user's thumb must not take a tap aimed at what was there
     * before (UX §8.2); half a second is below the time anyone needs to read the request.
     */
    val interactionArmDelayMs: Long = 500,
    /** Image uploads (`POST /v1/blobs`); see [ImageUploadPolicy]. */
    val imageUpload: ImageUploadPolicy = ImageUploadPolicy(),
    /**
     * How long the composer waits after the last keystroke of an `@` mention before it asks the
     * daemon (`fs/search`). Every query is a round trip through Tailscale; 250 ms is below the
     * pause people notice in a suggestion list and skips the intermediate keystrokes of a word.
     */
    val mentionSearchDebounceMs: Long = 250,
    /**
     * Images attached to one message. The photo picker needs an upper bound; eight covers a set
     * of screenshots, while each image adds to the model's context (and to the upload time on a
     * mobile network).
     */
    val maxImagesPerMessage: Int = 8,
    /**
     * Downloaded blobs (images, long command output, large patches) kept on disk. Blobs are
     * immutable (content-addressed), so the cache is always right; it makes images and outputs
     * that were seen once readable offline. 128 MiB holds hundreds of screenshots and stays
     * small next to the app's other data; Android may clear it when storage runs low.
     */
    val blobDiskCacheBytes: Long = 128L * 1024 * 1024,
    /**
     * Decoded images kept in memory (thumbnails in the thread, the image viewer). 32 MiB holds
     * the images of a long thread at thumbnail size without pressing on a phone's heap.
     */
    val imageMemoryCacheBytes: Int = 32 * 1024 * 1024,
    /**
     * The largest command or tool output read into memory for the full-output view. Outputs
     * above the daemon's inline limit (64 KiB) are blobs; 16 MiB is far more than anyone reads
     * on a phone and still fits the heap as text (the PC has the rest).
     */
    val maxOutputDownloadBytes: Long = 16L * 1024 * 1024,
    /** Diff viewer limits; see [DiffViewPolicy]. */
    val diff: DiffViewPolicy = DiffViewPolicy(),
    /**
     * First wait before the device token is decrypted again after the keystore failed to run the
     * cipher (its service busy, or still starting right after a reboot). The wait doubles per
     * failure up to [keystoreRetryMaxMs]; a second is about how long a restarting keystore
     * service takes, so a transient failure costs the user nothing noticeable.
     */
    val keystoreRetryInitialMs: Long = 1_000,
    /** Upper bound of that wait: a keystore that recovers later is noticed within a minute. */
    val keystoreRetryMaxMs: Long = 60_000,
    /**
     * Notifications the app keeps posted before it removes the oldest informational ones
     * (finished turns, clones, refused requests). Android silently refuses every further
     * notification of a package that has 50 posted (NotificationManagerService's
     * MAX_PACKAGE_NOTIFICATIONS, which also counts the connection service's notification and the
     * group summaries the system adds). 40 leaves room for a burst of approvals and questions,
     * which are never removed to make room.
     */
    val notificationBudget: Int = 40,
    /** How often time displays refresh and how much of long content the screens show; see [DisplayPolicy]. */
    val display: DisplayPolicy = DisplayPolicy(),
) {
    init {
        require(httpConnectTimeoutMs > 0 && httpReadTimeoutMs > 0 && httpWriteTimeoutMs > 0) { "HTTP timeouts must be positive" }
        require(pairCallTimeoutMs > 0) { "pairCallTimeoutMs must be positive" }
        require(notificationActionTimeoutMs > 0) { "notificationActionTimeoutMs must be positive" }
        require(unpairRevokeTimeoutMs > 0) { "unpairRevokeTimeoutMs must be positive" }
        require(readReconnectWaitMs >= 0) { "readReconnectWaitMs must not be negative" }
        require(connectionLogCapacity > 0) { "connectionLogCapacity must be positive" }
        require(uiStopTimeoutMs >= 0) { "uiStopTimeoutMs must not be negative" }
        require(interactionArmDelayMs >= 0) { "interactionArmDelayMs must not be negative" }
        require(mentionSearchDebounceMs >= 0) { "mentionSearchDebounceMs must not be negative" }
        // The photo picker's multiple-selection contract needs at least two.
        require(maxImagesPerMessage >= 2) { "maxImagesPerMessage must be at least 2" }
        require(blobDiskCacheBytes > 0 && imageMemoryCacheBytes > 0 && maxOutputDownloadBytes > 0) { "cache and download limits must be positive" }
        require(keystoreRetryInitialMs > 0 && keystoreRetryMaxMs >= keystoreRetryInitialMs) { "0 < keystoreRetryInitialMs <= keystoreRetryMaxMs" }
        // Below the platform's 50 (with room for the service's own and the group summaries).
        require(notificationBudget in 2..PLATFORM_NOTIFICATION_LIMIT) { "notificationBudget must be within 2..$PLATFORM_NOTIFICATION_LIMIT" }
    }

    private companion object {
        /** NotificationManagerService.MAX_PACKAGE_NOTIFICATIONS: posted notifications per package. */
        const val PLATFORM_NOTIFICATION_LIMIT = 50
    }
}

/**
 * How often the screens refresh what depends on the clock, and how much of long content they
 * show before "全文を表示" / "PC で確認" (docs/android.md 21). Screens read it through
 * `LocalAppPolicy`; nothing here changes what is stored or sent.
 */
data class DisplayPolicy(
    /**
     * Refresh interval of relative times ("3 分前"): they show minutes, so twice a minute keeps
     * them right without recomposing lists every second.
     */
    val relativeTimeRefreshMs: Long = 30_000,
    /** Tick of the running turn's "作業中 {経過}" clock: it shows seconds. */
    val workingTickMs: Long = 1_000,
    /** Lines of a command shown while its card is folded. */
    val commandCollapsedLines: Int = 3,
    /** Output lines under a running command: live progress without opening the card. */
    val outputRunningLines: Int = 4,
    /** Output lines in an opened command card; the full output has its own screen. */
    val outputExpandedLines: Int = 60,
    /** Lines of a tool's input in an opened tool card; the full input has its own screen. */
    val toolInputLines: Int = 40,
    /** Diff lines of a file change shown in the conversation; the diff screen has the rest. */
    val inlineDiffLines: Int = 80,
    /** Files an approval card lists (a larger change names the count and opens the diff). */
    val approvalFiles: Int = 8,
    /** Lines of a tool's input shown in an approval card. */
    val approvalInputLines: Int = 12,
    /**
     * Files named in an approval's one-line preview (notification body, inbox row), after which
     * it says "ほか n 件": a notification line fits about three short paths.
     */
    val previewFiles: Int = 3,
    /**
     * Running background tasks a stop or archive confirmation names (docs/android.md 30), after
     * which it says "ほか n 件": the dialog stays readable without scrolling on a phone.
     */
    val dialogTaskTitles: Int = 5,
    /**
     * Output lines of a finished background task shown in its card (the tail); the full output
     * has its own screen. A result summary rather than a live log, so a few lines suffice.
     */
    val taskOutputLines: Int = 6,
) {
    init {
        require(dialogTaskTitles > 0 && taskOutputLines > 0) { "background display limits must be positive" }
        require(relativeTimeRefreshMs > 0 && workingTickMs > 0) { "refresh intervals must be positive" }
        require(commandCollapsedLines > 0 && outputRunningLines > 0 && outputExpandedLines > 0) { "command line limits must be positive" }
        require(toolInputLines > 0 && inlineDiffLines > 0 && approvalFiles > 0 && approvalInputLines > 0) { "display limits must be positive" }
        require(previewFiles > 0) { "previewFiles must be positive" }
    }
}

/** How much of a diff the phone downloads and draws (docs/ux/codex-desktop.md §6, §8.2). */
data class DiffViewPolicy(
    /**
     * The largest patch downloaded (`patchBlobId`) and parsed. A patch this size is tens of
     * thousands of lines, beyond reviewing on a phone; above it the viewer lists the files and
     * says to review on the PC ("差分が大きすぎて表示できません").
     */
    val maxPatchBytes: Long = 16L * 1024 * 1024,
    /**
     * Above this many lines in all files together, the viewer shows one file at a time with
     * previous / next ("差分が大きいため、ファイルを1件ずつ表示します"), so a huge refactoring does
     * not build one endless list.
     */
    val oneFileAtATimeLines: Int = 3_000,
    /**
     * A single file with more lines than this is not drawn ("PC で確認"): generated files and
     * lock files, which nobody reviews line by line on a phone.
     */
    val maxFileLines: Int = 20_000,
    /**
     * Characters of a diff line quoted into a review comment (the rest becomes "…"): enough to
     * recognise the line in the message without pasting a minified file into it.
     */
    val commentQuoteChars: Int = 160,
) {
    init {
        require(maxPatchBytes > 0 && oneFileAtATimeLines > 0 && maxFileLines > 0) { "diff limits must be positive" }
        require(commentQuoteChars > 0) { "commentQuoteChars must be positive" }
    }
}

/** How images are prepared before `POST /v1/blobs` (used by the composer's attachments). */
data class ImageUploadPolicy(
    /**
     * Longest edge of a re-encoded image. Coding agents downscale larger images before the
     * model sees them (around 1.5–2k pixels), so 2048 px keeps screenshots legible at a
     * fraction of the bytes of a full-resolution photo.
     */
    val maxEdgePx: Int = 2048,
    /** JPEG quality of re-encoded images: visually lossless for screenshots and photos. */
    val jpegQuality: Int = 85,
    /**
     * Factor applied to the edge when a re-encoded image is still above the server's limit;
     * each step roughly halves the bytes.
     */
    val downscaleStep: Float = 0.75f,
    /** Smallest edge tried before giving up: below this, text in a screenshot is unreadable. */
    val minEdgePx: Int = 512,
    /**
     * Largest source file read into memory. Above the daemon's blob limit (so a too-large but
     * re-encodable photo can still be shrunk) and within a phone's heap.
     */
    val maxSourceBytes: Long = 64L * 1024 * 1024,
    /**
     * The daemon's default `max_blob_bytes` (25 MiB, design.md §13), used until `initialize`
     * announced the actual limit (`policy.maxBlobBytes`).
     */
    val fallbackMaxBlobBytes: Long = 25L * 1024 * 1024,
) {
    init {
        require(maxEdgePx >= minEdgePx && minEdgePx > 0) { "0 < minEdgePx <= maxEdgePx" }
        require(jpegQuality in 1..100) { "jpegQuality must be within 1..100" }
        require(downscaleStep > 0f && downscaleStep < 1f) { "downscaleStep must be within (0, 1)" }
        require(maxSourceBytes > 0 && fallbackMaxBlobBytes > 0) { "byte limits must be positive" }
    }
}
