package dev.aas.android.domain.composer

import dev.aas.android.protocol.EffortLevel
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Model
import dev.aas.android.protocol.PermissionMode
import dev.aas.android.protocol.ProjectDefaults
import dev.aas.android.protocol.ThreadSettings

/**
 * The model / effort / permission choices a harness offers (protocol.md §3 `Harness`) and the
 * values in effect for a thread: the thread's setting, else the harness's default. Nothing is
 * inferred beyond what the harness lists.
 */
object HarnessSettings {
    /**
     * The model in effect: the chosen one, else the harness default. A chosen model the harness
     * does not list (any more) is `null`: callers show its id, never another model.
     */
    fun model(harness: Harness?, settings: ThreadSettings): Model? {
        if (harness == null) return null
        settings.model?.let { id -> return harness.models.firstOrNull { it.id == id } }
        return harness.models.firstOrNull { it.id == harness.defaultModel } ?: harness.models.firstOrNull { it.isDefault }
    }

    /**
     * Effort levels for [modelId]: the model's own list when it has one (by id, in the harness's
     * order), otherwise all of the harness's levels.
     */
    fun effortLevels(harness: Harness?, modelId: String?): List<EffortLevel> {
        if (harness == null) return emptyList()
        val allowed = harness.models.firstOrNull { it.id == modelId }?.effortLevels ?: return harness.effortLevels
        return harness.effortLevels.filter { it.id in allowed }
    }

    /**
     * Whether [modelId] can keep the thread's [effort]: it has none (the harness decides), the
     * model offers it, or the model offers no levels to choose from (as in the model sheet,
     * where only a model with levels asks for one).
     */
    fun offersEffort(harness: Harness?, modelId: String, effort: String?): Boolean {
        if (effort == null) return true
        val levels = effortLevels(harness, modelId)
        return levels.isEmpty() || levels.any { it.id == effort }
    }

    /** The effort level in effect (`null`: the harness decides, shown as 既定). */
    fun effort(harness: Harness?, settings: ThreadSettings): EffortLevel? =
        settings.effort?.let { id -> harness?.effortLevels?.firstOrNull { it.id == id } }

    /**
     * The permission mode in effect: the chosen one, else the harness default. A chosen mode the
     * harness does not list is shown by its id (never as another mode).
     */
    fun permission(harness: Harness?, settings: ThreadSettings): PermissionMode? {
        if (harness == null) return null
        settings.permissionMode?.let { id -> return harness.permissionModes.firstOrNull { it.id == id } ?: PermissionMode(id, id) }
        return harness.permissionModes.firstOrNull { it.id == harness.defaultPermissionMode } ?: harness.permissionModes.firstOrNull { it.isDefault }
    }

    /**
     * The permission modes [modelId] can run in (protocol.md §3.1 `Model.permissionModes`): the
     * model's own list when it has one (by id, in the harness's order), otherwise all of the
     * harness's modes. The app offers no other (the daemon refuses them with `invalidParams`).
     */
    fun permissionModes(harness: Harness?, modelId: String?): List<PermissionMode> {
        if (harness == null) return emptyList()
        val allowed = harness.models.firstOrNull { it.id == modelId }?.permissionModes ?: return harness.permissionModes
        return harness.permissionModes.filter { it.id in allowed }
    }

    /**
     * Whether [modelId] can run in the permission mode [settings] put in effect (the chosen one,
     * else the harness default). A model without its own list runs in every mode; a mode in
     * effect that is not known (no harness default) is left to the daemon.
     */
    fun offersPermission(harness: Harness?, modelId: String?, settings: ThreadSettings): Boolean {
        val allowed = harness?.models?.firstOrNull { it.id == modelId }?.permissionModes ?: return true
        val mode = permission(harness, settings)?.id ?: return true
        return mode in allowed
    }

    /** A permission mode other than the harness default: the app asks before switching to it. */
    fun needsConfirmation(harness: Harness?, mode: PermissionMode): Boolean {
        if (harness == null) return true
        val default = harness.permissionModes.firstOrNull { it.isDefault }?.id ?: harness.defaultPermissionMode
        return mode.id != default
    }

    /**
     * The settings a new thread starts with: the project's defaults when they were made with
     * the same harness ("前回値を引き継ぐ", UX §8.2), else the harness's defaults (left unset).
     * Values the harness no longer lists are dropped.
     */
    fun initial(harness: Harness, defaults: ProjectDefaults): ThreadSettings {
        if (defaults.harnessId != harness.id) return ThreadSettings()
        val model = defaults.model?.takeIf { id -> harness.models.any { it.id == id } }
        val modelInEffect = model ?: harness.defaultModel
        val effort = defaults.effort?.takeIf { id -> effortLevels(harness, modelInEffect).any { it.id == id } }
        // A mode the model does not run in (the model's list changed since) is dropped too.
        val permission = defaults.permissionMode?.takeIf { id -> permissionModes(harness, modelInEffect).any { it.id == id } }
        return ThreadSettings(model = model, effort = effort, permissionMode = permission)
    }

    /** The harness a new thread uses by default: the project's last one if available, else the first available. */
    fun initialHarness(harnesses: List<Harness>, defaults: ProjectDefaults): Harness? =
        harnesses.firstOrNull { it.id == defaults.harnessId && it.available } ?: harnesses.firstOrNull { it.available }

    /** The chip text: "Claude · Sonnet · 高". */
    fun label(harness: Harness?, settings: ThreadSettings): String = listOfNotNull(
        harness?.displayName,
        model(harness, settings)?.displayName ?: settings.model,
        effort(harness, settings)?.label,
    ).joinToString(" · ")
}
