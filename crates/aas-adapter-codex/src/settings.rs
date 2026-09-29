//! Thread settings ↔ Codex parameters.
//!
//! Permission modes are presets mirroring the Codex desktop app ("Ask for approval",
//! "Approve for me", "Full access") plus a read-only mode. Each preset is a fixed
//! (approvalPolicy, approvalsReviewer, sandbox) triple.

use std::collections::BTreeMap;

use aas_harness::protocol::{EffortLevel, Model, PermissionMode, ThreadSettings};
use serde_json::{Map, Value, json};

use crate::wire::WireModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// `AskForApproval` value.
    pub approval_policy: &'static str,
    /// `ApprovalsReviewer` value.
    pub reviewer: &'static str,
    /// `SandboxMode` value (thread open parameters).
    pub sandbox_mode: &'static str,
}

pub const DEFAULT_PRESET: &str = "ask";

pub const PRESETS: [Preset; 4] = [
    Preset {
        id: "ask",
        label: "Ask for approval",
        description: "Works inside the workspace sandbox and asks before going beyond it",
        approval_policy: "on-request",
        reviewer: "user",
        sandbox_mode: "workspace-write",
    },
    Preset {
        id: "auto",
        label: "Approve for me",
        description: "Workspace sandbox; approval requests are reviewed automatically by Codex",
        approval_policy: "on-request",
        reviewer: "auto_review",
        sandbox_mode: "workspace-write",
    },
    Preset {
        id: "readOnly",
        label: "Read only",
        description: "Can read files; asks before any change or command outside the read-only sandbox",
        approval_policy: "on-request",
        reviewer: "user",
        sandbox_mode: "read-only",
    },
    Preset {
        id: "fullAccess",
        label: "Full access",
        description: "No sandbox and no approval prompts",
        approval_policy: "never",
        reviewer: "user",
        sandbox_mode: "danger-full-access",
    },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

pub fn permission_modes() -> Vec<PermissionMode> {
    PRESETS
        .iter()
        .map(|p| PermissionMode {
            id: p.id.into(),
            label: p.label.into(),
            description: Some(p.description.into()),
            is_default: p.id == DEFAULT_PRESET,
        })
        .collect()
}

/// `SandboxPolicy` object for `turn/start` (defaults of each mode).
fn sandbox_policy(mode: &str) -> Value {
    match mode {
        "read-only" => json!({"type": "readOnly", "networkAccess": false}),
        "danger-full-access" => json!({"type": "dangerFullAccess"}),
        _ => json!({
            "type": "workspaceWrite", "writableRoots": [], "networkAccess": false,
            "excludeTmpdirEnvVar": false, "excludeSlashTmp": false
        }),
    }
}

/// Overrides for `thread/start`, `thread/resume` and `thread/fork`. Absent settings are not
/// sent, so Codex keeps its own configuration (or the thread's persisted settings).
/// `service_tier` is the fast mode's tier when the session starts in fast mode.
pub fn open_overrides(
    settings: &ThreadSettings,
    service_tier: Option<&str>,
) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    if let Some(model) = &settings.model {
        map.insert("model".into(), json!(model));
    }
    if let Some(tier) = service_tier {
        map.insert("serviceTier".into(), json!(tier));
    }
    if let Some(mode) = &settings.permission_mode {
        let p = preset(mode).ok_or_else(|| format!("unknown permission mode {mode}"))?;
        map.insert("approvalPolicy".into(), json!(p.approval_policy));
        map.insert("approvalsReviewer".into(), json!(p.reviewer));
        map.insert("sandbox".into(), json!(p.sandbox_mode));
    }
    Ok(map)
}

/// Overrides for `turn/start`. Model and effort are sent whenever set (Codex applies turn
/// overrides to the turn and all later turns, so re-sending is idempotent). Permission
/// settings are sent only when they differ from what is already in effect (`applied_mode`),
/// because the per-turn `sandboxPolicy` object would otherwise replace sandbox details the
/// user configured in `config.toml`.
pub fn turn_overrides(
    settings: &ThreadSettings,
    applied_mode: Option<&str>,
) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    if let Some(model) = &settings.model {
        map.insert("model".into(), json!(model));
    }
    if let Some(effort) = &settings.effort {
        map.insert("effort".into(), json!(effort));
    }
    if let Some(mode) = &settings.permission_mode
        && applied_mode != Some(mode.as_str())
    {
        let p = preset(mode).ok_or_else(|| format!("unknown permission mode {mode}"))?;
        map.insert("approvalPolicy".into(), json!(p.approval_policy));
        map.insert("approvalsReviewer".into(), json!(p.reviewer));
        map.insert("sandboxPolicy".into(), sandbox_policy(p.sandbox_mode));
    }
    Ok(map)
}

/// The preset whose triple exactly equals what Codex reports as in effect, if any.
pub fn preset_from_response(
    approval: Option<&Value>,
    reviewer: Option<&str>,
    sandbox: Option<&Value>,
) -> Option<&'static str> {
    let approval = approval?.as_str()?;
    let reviewer = reviewer.unwrap_or("user");
    let sandbox_mode = match sandbox?.get("type")?.as_str()? {
        "workspaceWrite" => "workspace-write",
        "readOnly" => "read-only",
        "dangerFullAccess" => "danger-full-access",
        _ => return None,
    };
    PRESETS
        .iter()
        .find(|p| {
            p.approval_policy == approval
                && p.reviewer == reviewer
                && p.sandbox_mode == sandbox_mode
        })
        .map(|p| p.id)
}

/// Fixed labels of known effort ids; unknown ids are shown as-is.
pub fn effort_label(id: &str) -> String {
    match id {
        "none" => "None".into(),
        "minimal" => "Minimal".into(),
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "Extra high".into(),
        "max" => "Max".into(),
        other => other.into(),
    }
}

pub struct ModelCatalog {
    pub models: Vec<Model>,
    pub default_model: Option<String>,
    pub effort_levels: Vec<EffortLevel>,
    /// The fast mode of each model that has one (see [`FastTier`]).
    pub fast_tiers: FastTiers,
}

/// The service tier that is a model's fast mode: the one tier its `model/list` entry lists in
/// `serviceTiers` (codex-cli 0.148.0: `{id: "priority", name: "Fast"}` on the GPT models; the
/// TUI calls the toggle "Fast mode"). A model that lists no tier has no fast mode. A model that
/// lists several has none either: which of them is the fast one is not stated anywhere, and
/// choosing by the tier's name would be reading text written for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastTier {
    /// Sent as `serviceTier`.
    pub id: String,
    /// Codex's name of the tier (`Fast`), shown as the fast mode state.
    pub name: String,
}

/// Fast tiers by model id.
pub type FastTiers = BTreeMap<String, FastTier>;

/// What the thread's service tier is called in the fast mode state (`Thread.fastModeState`):
/// the tier's own name when it is a fast mode's tier, else Codex's id of it as reported
/// (`default` once a tier was cleared; a tier set in `config.toml`).
pub fn tier_word(tier: &str, fast_tiers: &FastTiers) -> String {
    fast_tiers
        .values()
        .find(|t| t.id == tier && !t.name.is_empty())
        .map(|t| t.name.clone())
        .unwrap_or_else(|| tier.to_owned())
}

/// `collaborationMode` for `turn/start`: plan or default mode, with the model and effort the
/// thread uses. `developer_instructions: null` selects Codex's built-in text of the mode. The
/// effort goes inside the mode because Codex ignores a top-level `effort` sent together with a
/// mode, and a mode's own preset effort (`medium` for plan) would replace the thread's
/// (recorded, codex-cli 0.148.0).
pub fn collaboration_mode(plan: bool, model: &str, effort: Option<&str>) -> Value {
    json!({
        "mode": if plan { "plan" } else { "default" },
        "settings": {
            "model": model,
            "reasoning_effort": effort,
            "developer_instructions": null,
        },
    })
}

/// Visible models from `model/list`; effort levels are the union of every model's supported
/// levels, in order of first appearance.
pub fn model_catalog(models: &[WireModel]) -> ModelCatalog {
    let mut effort_ids: Vec<String> = Vec::new();
    let mut out = Vec::new();
    let mut default_model = None;
    let mut fast_tiers = FastTiers::new();
    for m in models.iter().filter(|m| !m.hidden) {
        if let [tier] = m.service_tiers.as_slice() {
            fast_tiers.insert(
                m.id.clone(),
                FastTier {
                    id: tier.id.clone(),
                    name: tier.name.clone(),
                },
            );
        }
        let efforts: Vec<String> = m
            .supported_reasoning_efforts
            .iter()
            .map(|e| e.reasoning_effort.clone())
            .collect();
        for e in &efforts {
            if !effort_ids.contains(e) {
                effort_ids.push(e.clone());
            }
        }
        if m.is_default && default_model.is_none() {
            default_model = Some(m.id.clone());
        }
        out.push(Model {
            id: m.id.clone(),
            display_name: if m.display_name.is_empty() {
                m.id.clone()
            } else {
                m.display_name.clone()
            },
            description: (!m.description.is_empty()).then(|| m.description.clone()),
            is_default: m.is_default,
            effort_levels: (!efforts.is_empty()).then_some(efforts),
        });
    }
    ModelCatalog {
        models: out,
        default_model,
        effort_levels: effort_ids
            .iter()
            .map(|id| EffortLevel {
                id: id.clone(),
                label: effort_label(id),
            })
            .collect(),
        fast_tiers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(model: Option<&str>, effort: Option<&str>, mode: Option<&str>) -> ThreadSettings {
        ThreadSettings {
            model: model.map(Into::into),
            effort: effort.map(Into::into),
            permission_mode: mode.map(Into::into),
        }
    }

    #[test]
    fn open_overrides_map_presets() {
        let map =
            open_overrides(&settings(Some("m"), Some("high"), Some("fullAccess")), None).unwrap();
        assert_eq!(
            Value::Object(map),
            json!({"model":"m","approvalPolicy":"never","approvalsReviewer":"user","sandbox":"danger-full-access"})
        );
        assert!(open_overrides(&settings(None, None, Some("bogus")), None).is_err());
        assert!(
            open_overrides(&ThreadSettings::default(), None)
                .unwrap()
                .is_empty()
        );
        let fast = open_overrides(&settings(Some("m"), None, None), Some("priority")).unwrap();
        assert_eq!(
            Value::Object(fast),
            json!({"model":"m","serviceTier":"priority"})
        );
    }

    #[test]
    fn collaboration_modes_carry_the_thread_effort() {
        assert_eq!(
            collaboration_mode(true, "m", Some("low")),
            json!({"mode":"plan","settings":{"model":"m","reasoning_effort":"low","developer_instructions":null}})
        );
        assert_eq!(
            collaboration_mode(false, "m", None),
            json!({"mode":"default","settings":{"model":"m","reasoning_effort":null,"developer_instructions":null}})
        );
    }

    #[test]
    fn turn_overrides_send_permissions_only_when_changed() {
        let s = settings(Some("m"), Some("low"), Some("ask"));
        let unchanged = turn_overrides(&s, Some("ask")).unwrap();
        assert_eq!(
            Value::Object(unchanged),
            json!({"model":"m","effort":"low"})
        );
        let changed = turn_overrides(&s, Some("readOnly")).unwrap();
        assert_eq!(changed["approvalPolicy"], "on-request");
        assert_eq!(changed["sandboxPolicy"]["type"], "workspaceWrite");
    }

    #[test]
    fn presets_are_recognised_from_responses() {
        let p = preset_from_response(
            Some(&json!("on-request")),
            Some("auto_review"),
            Some(&json!({"type":"workspaceWrite"})),
        );
        assert_eq!(p, Some("auto"));
        let p = preset_from_response(
            Some(&json!("untrusted")),
            Some("user"),
            Some(&json!({"type":"readOnly"})),
        );
        assert_eq!(p, None);
    }

    #[test]
    fn catalog_from_recorded_model_list() {
        let models: Vec<WireModel> = serde_json::from_value(json!([
            {"id":"deepseek-flash","displayName":"DeepSeek-Flash","description":"Fast","hidden":false,"isDefault":true,
             "supportedReasoningEfforts":[{"reasoningEffort":"low","description":""},{"reasoningEffort":"high","description":""},{"reasoningEffort":"max","description":""}]},
            {"id":"hidden-one","displayName":"H","description":"","hidden":true,"isDefault":false,"supportedReasoningEfforts":[]},
            {"id":"pro","displayName":"","description":"","hidden":false,"isDefault":false,
             "supportedReasoningEfforts":[{"reasoningEffort":"xhigh","description":""}]}
        ]))
        .unwrap();
        let c = model_catalog(&models);
        assert_eq!(c.models.len(), 2);
        assert_eq!(c.default_model.as_deref(), Some("deepseek-flash"));
        assert_eq!(c.models[1].display_name, "pro");
        let ids: Vec<&str> = c.effort_levels.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["low", "high", "max", "xhigh"]);
        assert_eq!(c.effort_levels[3].label, "Extra high");
        // The user's DeepSeek models list no service tier: no fast mode.
        assert!(c.fast_tiers.is_empty());
    }

    #[test]
    fn fast_tiers_from_the_recorded_bundled_catalog() {
        // codex-cli 0.148.0's bundled catalog (recording `tiers`), abridged.
        let models: Vec<WireModel> = serde_json::from_value(json!([
            {"id":"gpt-5.6-sol","displayName":"GPT-5.6 Sol","isDefault":true,"hidden":false,
             "serviceTiers":[{"id":"priority","name":"Fast","description":"1.5x speed, increased usage"}],
             "defaultServiceTier":null,"additionalSpeedTiers":["fast"]},
            {"id":"gpt-5.2","displayName":"GPT-5.2","isDefault":false,"hidden":false,
             "serviceTiers":[],"defaultServiceTier":null,"additionalSpeedTiers":[]},
            {"id":"two-tiers","displayName":"T","isDefault":false,"hidden":false,
             "serviceTiers":[{"id":"priority","name":"Fast"},{"id":"flex","name":"Flex"}]}
        ]))
        .unwrap();
        let c = model_catalog(&models);
        assert_eq!(
            c.fast_tiers,
            FastTiers::from([(
                "gpt-5.6-sol".to_owned(),
                FastTier {
                    id: "priority".into(),
                    name: "Fast".into()
                }
            )])
        );
        assert_eq!(tier_word("priority", &c.fast_tiers), "Fast");
        assert_eq!(tier_word("default", &c.fast_tiers), "default");
    }
}
