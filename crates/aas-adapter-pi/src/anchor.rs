//! Turn anchors of pi sessions (feature `forkAtTurn`, see `docs/adapters/pi.md` §13).
//!
//! A pi session is a tree of entries with ids that stay the same in every copy of the session
//! (`fork`, `--fork`). The anchor of a turn names two entries, both read from pi's own
//! `get_entries` answer for the turn (never found by counting turns or messages):
//!
//! * `leafId`: pi's leaf when the turn was over — the last entry of the turn;
//! * `userEntryId`: the first user message among the entries the turn added, when the adapter
//!   knows where the turn began (absent otherwise, e.g. for a turn that opened with an
//!   extension's custom message, or the first turn after pi replaced its session).
//!
//! A fork "with the turn" keeps the path through `leafId` (`ctx.fork(leafId, { position:
//! "at" })`); a fork "before the turn" keeps the path before `userEntryId` (`ctx.fork(userEntryId,
//! { position: "before" })`, the RPC `fork`'s meaning), or, without one, the path through the
//! previous turn's `leafId`.

use aas_harness::{AdapterError, ForkPoint};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The anchor of one turn (serialized as the turn's `native_anchor`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Anchor {
    pub leaf_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_entry_id: Option<String>,
}

impl Anchor {
    pub fn to_value(&self) -> Value {
        let mut value = serde_json::json!({ "leafId": self.leaf_id });
        if let Some(user) = &self.user_entry_id {
            value["userEntryId"] = Value::String(user.clone());
        }
        value
    }

    pub fn parse(value: &Value) -> Result<Anchor, AdapterError> {
        serde_json::from_value(value.clone())
            .map_err(|e| AdapterError::Other(format!("not an anchor of a pi turn ({e}): {value}")))
    }
}

/// Where `ctx.fork` branches: the entry and pi's `position`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkTarget {
    pub entry_id: String,
    /// `"at"` (the path through the entry) or `"before"` (the path before the user message).
    pub position: &'static str,
}

/// The fork of `point`: with the turn, at its leaf; before it, before its first user message
/// or, when pi reported none, at the previous turn's leaf.
pub fn fork_target(point: &ForkPoint) -> Result<ForkTarget, AdapterError> {
    let anchor = Anchor::parse(&point.anchor)?;
    if !point.before {
        return Ok(ForkTarget {
            entry_id: anchor.leaf_id,
            position: "at",
        });
    }
    if let Some(user) = anchor.user_entry_id {
        return Ok(ForkTarget {
            entry_id: user,
            position: "before",
        });
    }
    match point.previous.as_ref().map(Anchor::parse).transpose()? {
        Some(previous) => Ok(ForkTarget {
            entry_id: previous.leaf_id,
            position: "at",
        }),
        None => Err(AdapterError::Other(
            "pi recorded neither where this turn began nor the end of the turn before it, so the \
             session cannot be branched before this turn"
                .into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn point(anchor: Value, before: bool, previous: Option<Value>) -> ForkPoint {
        ForkPoint {
            anchor,
            before,
            previous,
        }
    }

    #[test]
    fn anchors_round_trip_without_empty_fields() {
        let anchor = Anchor {
            leaf_id: "eb03ba61".into(),
            user_entry_id: None,
        };
        assert_eq!(anchor.to_value(), json!({"leafId": "eb03ba61"}));
        assert_eq!(Anchor::parse(&anchor.to_value()).unwrap(), anchor);
        assert!(Anchor::parse(&json!({"turn": 1})).is_err());
    }

    #[test]
    fn forks_branch_at_the_leaf_or_before_the_user_message() {
        let this = json!({"leafId": "l2", "userEntryId": "u2"});
        let earlier = json!({"leafId": "l1", "userEntryId": "u1"});
        assert_eq!(
            fork_target(&point(this.clone(), false, Some(earlier.clone()))).unwrap(),
            ForkTarget {
                entry_id: "l2".into(),
                position: "at"
            }
        );
        assert_eq!(
            fork_target(&point(this, true, Some(earlier.clone()))).unwrap(),
            ForkTarget {
                entry_id: "u2".into(),
                position: "before"
            }
        );
        // No user message recorded for the turn: right after the previous turn.
        let without_user = json!({"leafId": "l2"});
        assert_eq!(
            fork_target(&point(without_user.clone(), true, Some(earlier))).unwrap(),
            ForkTarget {
                entry_id: "l1".into(),
                position: "at"
            }
        );
        assert!(fork_target(&point(without_user, true, None)).is_err());
        assert!(fork_target(&point(json!({"pending": 1}), false, None)).is_err());
    }
}
