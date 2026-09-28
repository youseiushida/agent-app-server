package dev.aas.android.ui.navigation

import android.net.Uri

/**
 * URIs that open a screen of the app.
 *
 * * `aas://thread/<threadId>[?interactionId=<id>]` opens [ThreadRoute]. Notifications put it in
 *   an explicit intent to MainActivity; [IntentTarget] parses it and [AppNavigator] navigates
 *   (not a Navigation deep link: see [IntentTarget.of]).
 * * `aas://pair?u=…&c=…&n=…` is the pairing QR payload; opened from another app (a camera app
 *   that scanned the QR) it starts pairing with the values filled in, after confirmation.
 */
object DeepLinks {
    const val SCHEME = "aas"
    const val HOST_THREAD = "thread"
    const val HOST_PAIR = "pair"

    /** Query parameter of [ThreadRoute.interactionId]. */
    const val PARAM_INTERACTION = "interactionId"

    fun thread(threadId: String, interactionId: String? = null): Uri = Uri.Builder()
        .scheme(SCHEME)
        .authority(HOST_THREAD)
        .appendPath(threadId)
        .apply { if (interactionId != null) appendQueryParameter(PARAM_INTERACTION, interactionId) }
        .build()

    fun isPairingLink(uri: Uri?): Boolean = uri != null && uri.scheme == SCHEME && uri.host == HOST_PAIR
}
