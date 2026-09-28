package dev.aas.android.data

import dev.aas.android.sync.SyncLogger

/**
 * Lists read from the daemon, made unique by the id the screens key their rows by.
 *
 * Compose's lazy lists need every key once and throw on a key given twice, which ends the app.
 * The daemon passes some lists through from the harnesses: Codex's `thread/list` returns a
 * thread once per rollout when it was resumed elsewhere (e.g. in Codex desktop), so `native/list`
 * listed one session id two or three times and opening 「PC のセッションを取り込む」 crashed the
 * app on a phone. Pages of `thread/list` can also overlap when a thread's activity moves it
 * between them. So every list a screen shows keyed by a server id passes [unique] here, where
 * it enters the app. The screens keep the real ids as keys: an index key would tie a row's
 * state (the progress of an import, an expanded row) to its position instead of its item.
 *
 * The lists the sync engine stores (projects, threads, interactions, operations, turns, items)
 * are kept by id in the store already, and the engine makes a thread's queue unique itself
 * (`EventApplier`).
 */
class ServerLists(private val logger: SyncLogger) {
    /**
     * [list] with each [id] once, in the order of first appearance. Of the entries sharing an id
     * the greatest by [prefer] is kept (the first of equal ones), or the first entry without
     * [prefer]. Duplicates are logged as a warning naming [what] and the ids.
     */
    fun <T, K> unique(list: List<T>, what: String, id: (T) -> K, prefer: Comparator<in T>? = null): List<T> {
        if (list.size < 2) return list
        val groups = LinkedHashMap<K, MutableList<T>>()
        for (entry in list) groups.getOrPut(id(entry)) { ArrayList(1) }.add(entry)
        if (groups.size == list.size) return list
        val repeated = groups.filterValues { it.size > 1 }.keys
        val shown = repeated.take(LOGGED_IDS).joinToString()
        val more = if (repeated.size > LOGGED_IDS) " and ${repeated.size - LOGGED_IDS} more" else ""
        val kept = if (prefer == null) "the first" else "the preferred"
        logger.log(
            SyncLogger.Level.Warn,
            "$what listed ${repeated.size} id(s) more than once ($shown$more): kept $kept entry of each, ${list.size - groups.size} dropped",
            null,
        )
        return groups.values.map { entries -> if (prefer == null) entries.first() else entries.maxWith(prefer) }
    }

    private companion object {
        /** Ids named in one warning: enough to find the entries, short enough for the diagnostics screen. */
        const val LOGGED_IDS = 5
    }
}
