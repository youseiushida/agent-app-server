package dev.aas.android

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.core.os.BundleCompat
import dev.aas.android.ui.AasApp
import dev.aas.android.ui.navigation.PendingIntents
import dev.aas.android.ui.theme.AasTheme

/**
 * The single activity. It hosts the Compose navigation graph; the intent that started it and
 * those that arrive while it runs (notification taps, `aas://pair` links) wait in [intents]
 * until the graph handled them, also across the activity being recreated.
 */
class MainActivity : ComponentActivity() {
    private lateinit var intents: PendingIntents<Intent>

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        // A first start handles the intent that started it. A recreated activity handled that
        // one already, but not those that were still waiting when it was destroyed.
        val waiting = if (savedInstanceState == null) {
            listOf(intent)
        } else {
            BundleCompat.getParcelableArrayList(savedInstanceState, KEY_PENDING_INTENTS, Intent::class.java).orEmpty()
        }
        intents = PendingIntents(waiting)
        val container = appContainer
        setContent {
            AasTheme {
                AasApp(container = container, intents = intents)
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        intents.add(intent)
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        outState.putParcelableArrayList(KEY_PENDING_INTENTS, ArrayList(intents.items.value))
    }

    private companion object {
        /** Saved state: the intents not handled yet (see [PendingIntents]). */
        const val KEY_PENDING_INTENTS = "dev.aas.android.pendingIntents"
    }
}
