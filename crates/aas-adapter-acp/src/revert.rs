//! Cognition's revert extension, as Devin CLI implements it (shapes recorded from Devin CLI
//! 3000.11.3; docs/adapters/acp.md §17.2): the steps of a session and forks at a step, which
//! give a thread's fork at a turn (design.md §9.6). Everything here is used only when the agent
//! confirmed `cognition.ai/revert` (`crate::cognition::Extensions::revert`).
//!
//! * `_cognition.ai/revert/stepsUpdated {sessionId, steps}` (a notification) lists every step
//!   of the session when a prompt starts and when it ends. `_cognition.ai/revert/listSteps
//!   {sessionId}` answers the same list, but only for a session this process has loaded.
//! * A step of kind `prompt` is one prompt. Its `stepId` is the prompt's `userMessageId` (also
//!   `session/prompt`'s `_meta["cognition.ai/userMessageId"]`, `turn_stats.turnClientMessageId`
//!   and a replayed user chunk's `_meta["cognition.ai/clientMessageId"]`). A prompt that runs
//!   only one of Devin's own commands without the model (`/ask` alone, `/session-stats`) is no
//!   step.
//! * `forkTargetNodeId` is the node at which a fork holds the step, `revertTargetNodeId` the
//!   node right before it (the previous step's `forkTargetNodeId`).
//! * The node ids of a prompt's step move while the prompt runs (recorded: step 1's
//!   `forkTargetNodeId` was 1 when it started, 21 when it ended, 23 in `listSteps` right after
//!   `session/prompt` answered, and 23 from then on). A turn's anchor therefore takes node ids
//!   only from listings received after its prompt answered: the adapter asks `listSteps` then,
//!   and a later listing that says otherwise replaces the anchor (`TurnAnchorReplaced`).
//! * `_cognition.ai/revert/forkFromStep {sessionId, targetNodeId}` answers `{forkedSessionId}`:
//!   a new, unloaded session holding the source up to that node, with the source's step ids and
//!   node ids (so the anchors of copied turns stay valid in the fork). It works without loading
//!   the source, also while another process holds it.
//!
//! Anchors are built only from these explicit ids: a turn's steps are the steps first listed
//! while it ran, never counted.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use aas_harness::{AdapterError, AdapterEvent, ForkPoint};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `_cognition.ai/revert/stepsUpdated`.
pub const STEPS_UPDATED: &str = "_cognition.ai/revert/stepsUpdated";
/// `_cognition.ai/revert/listSteps`.
pub const LIST_STEPS: &str = "_cognition.ai/revert/listSteps";
/// `_cognition.ai/revert/forkFromStep`.
pub const FORK_FROM_STEP: &str = "_cognition.ai/revert/forkFromStep";
/// The kind of a prompt's step.
const PROMPT_KIND: &str = "prompt";
/// `_meta` key of a replayed user chunk: the prompt's step id.
pub const CLIENT_MESSAGE_ID: &str = "cognition.ai/clientMessageId";

/// One step as Devin lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    #[serde(default)]
    pub step_id: String,
    #[serde(default)]
    pub step_number: u64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub revert_target_node_id: Option<i64>,
    #[serde(default)]
    pub fork_target_node_id: Option<i64>,
}

/// Decodes a list of steps; entries that are not steps (no `stepId`) are left out, so one odd
/// entry does not hide the others. The result is in Devin's order (`stepNumber`).
pub fn parse_steps(steps: &[Value]) -> Vec<Step> {
    let mut out: Vec<Step> = steps
        .iter()
        .filter_map(|v| serde_json::from_value::<Step>(v.clone()).ok())
        .filter(|s| !s.step_id.is_empty())
        .collect();
    out.sort_by_key(|s| s.step_number);
    out
}

/// Params of `stepsUpdated` and the answer of `listSteps`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepList {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub steps: Vec<Value>,
}

/// The answer of `forkFromStep`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkFromStepResponse {
    #[serde(default)]
    pub forked_session_id: String,
}

/// A turn's anchor (`AdapterEvent::TurnAnchor`, stored by the engine and handed back in a
/// `ForkPoint`): the turn's steps and, once Devin listed them after the turn, the nodes a fork
/// branches at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepAnchor {
    /// The turn's steps in Devin's order (the prompt's own step first).
    pub step_ids: Vec<String>,
    /// `revertTargetNodeId` of the first step: a fork there holds everything before the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert_target_node_id: Option<i64>,
    /// `forkTargetNodeId` of the last step: a fork there holds the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_target_node_id: Option<i64>,
}

impl StepAnchor {
    /// An anchor naming only the steps (the node ids are not settled yet).
    fn provisional(step_ids: Vec<String>) -> Self {
        Self {
            step_ids,
            revert_target_node_id: None,
            fork_target_node_id: None,
        }
    }

    /// The anchor of `step_ids` with node ids from `known`; `None` when a step or a node id is
    /// missing there.
    fn settled(step_ids: &[String], known: &HashMap<String, Step>) -> Option<Self> {
        let first = known.get(step_ids.first()?)?;
        let last = known.get(step_ids.last()?)?;
        if step_ids.iter().any(|id| !known.contains_key(id)) {
            return None;
        }
        Some(Self {
            step_ids: step_ids.to_vec(),
            revert_target_node_id: Some(first.revert_target_node_id?),
            fork_target_node_id: Some(last.fork_target_node_id?),
        })
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("an anchor serializes")
    }

    /// An anchor this adapter reported (`None` for anything else).
    pub fn parse(value: &Value) -> Option<Self> {
        serde_json::from_value::<Self>(value.clone())
            .ok()
            .filter(|a| !a.step_ids.is_empty())
    }
}

/// The anchors of a session's turns as a history import gives them (`message_ids`: each turn's
/// `_meta["cognition.ai/clientMessageId"]` of its first user chunk), from the steps
/// `listSteps` gave for the loaded session (at rest, so settled). A turn's steps are its prompt
/// step and the steps of other kinds that follow it up to the next prompt step. A turn without
/// a message id or without a listed step has no anchor.
pub fn history_anchors(message_ids: &[Option<String>], steps: &[Step]) -> Vec<Option<Value>> {
    let mut groups: HashMap<&str, Vec<String>> = HashMap::new();
    let mut current: Option<&str> = None;
    for step in steps {
        if step.kind == PROMPT_KIND {
            current = Some(step.step_id.as_str());
        }
        if let Some(prompt) = current {
            groups.entry(prompt).or_default().push(step.step_id.clone());
        }
    }
    let known: HashMap<String, Step> = steps
        .iter()
        .map(|s| (s.step_id.clone(), s.clone()))
        .collect();
    message_ids
        .iter()
        .map(|id| {
            let ids = groups.get(id.as_deref()?)?;
            StepAnchor::settled(ids, &known).map(|a| a.to_value())
        })
        .collect()
}

/// What a fork branches the source at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkTarget {
    /// `forkFromStep` at this node.
    Node(i64),
    /// The whole source holds no step: the fork is a new session.
    Empty,
}

/// What a fork at `point` (`None`: of the whole session) branches at, from what is known
/// without reading the source: the steps this daemon's processes listed for it after their
/// turns (`indexed`), else the anchor's own node ids, else (before a turn) the previous turn's
/// anchor. `Ok(None)`: the source's steps must be listed first. An anchor this adapter did not
/// report is an error.
pub fn known_target(
    point: Option<&ForkPoint>,
    indexed: Option<&IndexedSession>,
) -> Result<Option<ForkTarget>, AdapterError> {
    let Some(point) = point else {
        let Some(session) = indexed.filter(|s| s.complete) else {
            return Ok(None);
        };
        return Ok(
            match session.order.last().and_then(|id| session.steps.get(id)) {
                Some(step) => step.fork_target_node_id.map(ForkTarget::Node),
                None => Some(ForkTarget::Empty),
            },
        );
    };
    let anchor = anchor_of(point)?;
    let listed = |id: &str| indexed.and_then(|s| s.steps.get(id));
    let node = if point.before {
        listed(&anchor.step_ids[0])
            .and_then(|s| s.revert_target_node_id)
            .or(anchor.revert_target_node_id)
            .or_else(|| {
                point
                    .previous
                    .as_ref()
                    .and_then(StepAnchor::parse)
                    .and_then(|p| {
                        p.step_ids
                            .last()
                            .and_then(|id| listed(id))
                            .and_then(|s| s.fork_target_node_id)
                            .or(p.fork_target_node_id)
                    })
            })
    } else {
        listed(anchor.step_ids.last().expect("an anchor names a step"))
            .and_then(|s| s.fork_target_node_id)
            .or(anchor.fork_target_node_id)
    };
    Ok(node.map(ForkTarget::Node))
}

/// What a fork at `point` (`None`: of the whole session) branches at, from the full list of
/// the source's steps (`listSteps` of the loaded source, which is at rest).
pub fn listed_target(
    point: Option<&ForkPoint>,
    steps: &[Step],
) -> Result<ForkTarget, AdapterError> {
    let node = |node: Option<i64>, what: &str| {
        node.map(ForkTarget::Node).ok_or_else(|| {
            AdapterError::Protocol(format!("Devin listed the step without its {what}"))
        })
    };
    let Some(point) = point else {
        return match steps.last() {
            Some(step) => node(step.fork_target_node_id, "forkTargetNodeId"),
            None => Ok(ForkTarget::Empty),
        };
    };
    let anchor = anchor_of(point)?;
    let find = |id: &str| {
        steps.iter().find(|s| s.step_id == id).ok_or_else(|| {
            AdapterError::Harness(format!(
                "the session no longer has the step `{id}` of this turn"
            ))
        })
    };
    if point.before {
        node(
            find(&anchor.step_ids[0])?.revert_target_node_id,
            "revertTargetNodeId",
        )
    } else {
        node(
            find(anchor.step_ids.last().expect("an anchor names a step"))?.fork_target_node_id,
            "forkTargetNodeId",
        )
    }
}

/// Whether `point` names an anchor this adapter reported (see
/// `aas_harness::HarnessAdapter::check_fork_point`).
pub fn check_point(point: &ForkPoint) -> Result<(), AdapterError> {
    anchor_of(point).map(|_| ())
}

fn anchor_of(point: &ForkPoint) -> Result<StepAnchor, AdapterError> {
    StepAnchor::parse(&point.anchor).ok_or_else(|| {
        AdapterError::Other(format!(
            "the turn's anchor `{}` is not a Devin step",
            point.anchor
        ))
    })
}

/// What this daemon's processes know of a session's steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexedSession {
    /// Steps of completed turns, as listed after those turns.
    pub steps: HashMap<String, Step>,
    /// Their ids in Devin's order.
    pub order: Vec<String>,
    /// `steps` is the whole session: no turn runs and the last one was listed after it ended.
    pub complete: bool,
}

/// The steps the sessions of this adapter listed, shared by its processes (by session id): a
/// fork of a session another of the daemon's processes holds (so that it cannot be loaded and
/// listed) branches at what that process listed.
#[derive(Debug, Clone, Default)]
pub struct StepIndex {
    sessions: Arc<Mutex<HashMap<String, IndexedSession>>>,
}

impl StepIndex {
    pub fn get(&self, session_id: &str) -> Option<IndexedSession> {
        self.sessions
            .lock()
            .expect("step index lock")
            .get(session_id)
            .cloned()
    }

    fn put(&self, session_id: &str, session: IndexedSession) {
        self.sessions
            .lock()
            .expect("step index lock")
            .insert(session_id.to_owned(), session);
    }
}

/// A turn this process anchored.
#[derive(Debug)]
struct Anchored {
    step_ids: Vec<String>,
    /// The anchor last reported for it (what the engine stores).
    reported: Value,
    /// A listing received after the turn named all its steps: `reported` holds their node ids.
    settled: bool,
}

/// The steps of the session a router runs, and the anchors of its turns.
#[derive(Debug)]
pub struct SessionSteps {
    session_id: String,
    index: StepIndex,
    /// Latest listed state of each step (merged from every listing).
    known: HashMap<String, Step>,
    /// Steps of completed turns and steps listed while no turn ran; `None` while unknown
    /// (the session's steps could not be listed when it was set up).
    base: Option<HashSet<String>>,
    running: bool,
    anchored: Vec<Anchored>,
}

impl SessionSteps {
    /// `listed`: the session's steps when it was set up (a new session has none); `None` when
    /// they could not be listed.
    pub fn new(session_id: String, index: StepIndex, listed: Option<Vec<Step>>) -> Self {
        let mut this = Self {
            session_id,
            index,
            known: HashMap::new(),
            base: None,
            running: false,
            anchored: Vec::new(),
        };
        if let Some(steps) = listed {
            this.base = Some(steps.iter().map(|s| s.step_id.clone()).collect());
            this.merge(steps);
        }
        this.publish();
        this
    }

    fn merge(&mut self, steps: Vec<Step>) {
        for step in steps {
            self.known.insert(step.step_id.clone(), step);
        }
    }

    /// A prompt was sent: steps first listed from now on belong to it.
    pub fn turn_started(&mut self) {
        self.running = true;
        self.publish();
    }

    /// A listing of the session's steps (`stepsUpdated`, or the answer of `listSteps`).
    /// Returns the replacements of the anchors of earlier turns (every turn anchored so far
    /// ended before this listing) whose node ids it gives otherwise than reported.
    pub fn on_listing(&mut self, steps: Vec<Step>) -> Vec<AdapterEvent> {
        let ids: HashSet<String> = steps.iter().map(|s| s.step_id.clone()).collect();
        self.merge(steps);
        if !self.running {
            // Listed while no prompt runs: part of the session before the next turn.
            self.base
                .get_or_insert_with(HashSet::new)
                .extend(ids.iter().cloned());
        }
        let mut out = Vec::new();
        for turn in &mut self.anchored {
            if !turn.step_ids.iter().all(|id| ids.contains(id)) {
                continue;
            }
            let Some(anchor) = StepAnchor::settled(&turn.step_ids, &self.known) else {
                continue;
            };
            turn.settled = true;
            let value = anchor.to_value();
            if value != turn.reported {
                out.push(AdapterEvent::TurnAnchorReplaced {
                    previous: turn.reported.clone(),
                    anchor: value.clone(),
                });
                turn.reported = value;
            }
        }
        self.publish();
        out
    }

    /// The prompt answered: the turn's anchor (its steps, the node ids to follow), when it has
    /// a step. `user_message_id` is the answer's `_meta["cognition.ai/userMessageId"]`, the
    /// turn's step when no listing named one.
    pub fn turn_ended(&mut self, user_message_id: Option<&str>) -> Option<Value> {
        self.running = false;
        let mut new: Vec<&Step> = match &self.base {
            Some(base) => self
                .known
                .values()
                .filter(|s| !base.contains(&s.step_id))
                .collect(),
            None => Vec::new(),
        };
        new.sort_by_key(|s| s.step_number);
        let mut step_ids: Vec<String> = new.into_iter().map(|s| s.step_id.clone()).collect();
        if step_ids.is_empty()
            && let Some(id) = user_message_id.filter(|id| !id.is_empty())
            && !self.base.as_ref().is_some_and(|b| b.contains(id))
        {
            // No listing named the prompt's step: the answer names it.
            step_ids.push(id.to_owned());
        }
        if let Some(base) = &mut self.base {
            base.extend(step_ids.iter().cloned());
        }
        let reported = (!step_ids.is_empty()).then(|| {
            let reported = StepAnchor::provisional(step_ids.clone()).to_value();
            self.anchored.push(Anchored {
                step_ids,
                reported: reported.clone(),
                settled: false,
            });
            reported
        });
        // A new step stays out of the index until it is listed after the prompt answered.
        self.publish();
        reported
    }

    /// Updates the shared index with the steps of completed turns, as listed after them. It is
    /// the whole session while no turn runs and every anchored turn was listed after it.
    fn publish(&self) {
        let Some(base) = &self.base else {
            self.index.put(&self.session_id, IndexedSession::default());
            return;
        };
        let pending: HashSet<&String> = self
            .anchored
            .iter()
            .filter(|t| !t.settled)
            .flat_map(|t| t.step_ids.iter())
            .collect();
        let mut steps: Vec<&Step> = self
            .known
            .values()
            .filter(|s| base.contains(&s.step_id) && !pending.contains(&s.step_id))
            .collect();
        steps.sort_by_key(|s| s.step_number);
        self.index.put(
            &self.session_id,
            IndexedSession {
                order: steps.iter().map(|s| s.step_id.clone()).collect(),
                steps: steps
                    .into_iter()
                    .map(|s| (s.step_id.clone(), s.clone()))
                    .collect(),
                complete: !self.running && pending.is_empty(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn step(n: u64, id: &str, revert: i64, fork: i64) -> Value {
        json!({"stepId": id, "stepNumber": n, "kind": "prompt", "userMessageId": id,
               "revertTargetNodeId": revert, "forkTargetNodeId": fork, "summary": "s"})
    }

    fn steps(v: &[Value]) -> Vec<Step> {
        parse_steps(v)
    }

    fn anchor(ids: &[&str], revert: Option<i64>, fork: Option<i64>) -> Value {
        StepAnchor {
            step_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            revert_target_node_id: revert,
            fork_target_node_id: fork,
        }
        .to_value()
    }

    /// The recorded sequence (revert.jsonl): step ids come with the prompt, node ids settle
    /// after it, and a later listing that agrees changes nothing.
    #[test]
    fn a_turns_anchor_settles_after_its_prompt() {
        let index = StepIndex::default();
        let mut s = SessionSteps::new("s1".into(), index.clone(), Some(Vec::new()));
        assert!(index.get("s1").unwrap().complete);
        s.turn_started();
        assert!(!index.get("s1").unwrap().complete);
        assert!(s.on_listing(steps(&[step(1, "a", 0, 1)])).is_empty());
        assert!(s.on_listing(steps(&[step(1, "a", 19, 21)])).is_empty());
        let reported = s.turn_ended(Some("a")).unwrap();
        assert_eq!(reported, anchor(&["a"], None, None));
        let indexed = index.get("s1").unwrap();
        assert!(!indexed.complete, "listed after the turn");
        assert!(
            indexed.order.is_empty(),
            "its provisional nodes are not shared"
        );
        // `listSteps` right after the prompt answered.
        let replaced = s.on_listing(steps(&[step(1, "a", 19, 23)]));
        assert_eq!(
            replaced,
            vec![AdapterEvent::TurnAnchorReplaced {
                previous: anchor(&["a"], None, None),
                anchor: anchor(&["a"], Some(19), Some(23)),
            }]
        );
        let indexed = index.get("s1").unwrap();
        assert!(indexed.complete);
        assert_eq!(indexed.order, vec!["a"]);
        // The next prompt: its own step is new, the first one is unchanged.
        s.turn_started();
        assert!(
            s.on_listing(steps(&[step(1, "a", 19, 23), step(2, "b", 23, 24)]))
                .is_empty()
        );
        assert_eq!(index.get("s1").unwrap().order, vec!["a"], "b runs");
        assert_eq!(s.turn_ended(Some("b")).unwrap(), anchor(&["b"], None, None));
        // A later listing that moves a settled node replaces its anchor again.
        let replaced = s.on_listing(steps(&[step(1, "a", 19, 25), step(2, "b", 25, 29)]));
        assert_eq!(replaced.len(), 2);
        assert_eq!(
            replaced[1],
            AdapterEvent::TurnAnchorReplaced {
                previous: anchor(&["b"], None, None),
                anchor: anchor(&["b"], Some(25), Some(29)),
            }
        );
        assert_eq!(index.get("s1").unwrap().order, vec!["a", "b"]);
    }

    #[test]
    fn turns_without_a_step_have_no_anchor_and_the_answer_names_an_unlisted_step() {
        let index = StepIndex::default();
        let mut s = SessionSteps::new("s1".into(), index.clone(), Some(Vec::new()));
        // `/ask` alone: no step, no anchor, nothing to wait for.
        s.turn_started();
        assert_eq!(s.turn_ended(None), None);
        assert!(index.get("s1").unwrap().complete);
        // No listing reached us, but the answer names the step.
        s.turn_started();
        assert_eq!(s.turn_ended(Some("x")), Some(anchor(&["x"], None, None)));
        // A step listed outside a turn is not the next turn's.
        assert!(
            s.on_listing(steps(&[step(1, "x", 0, 5), step(2, "y", 5, 6)]))
                .len()
                == 1
        );
        s.turn_started();
        assert_eq!(s.turn_ended(None), None);
    }

    #[test]
    fn a_resumed_session_knows_its_earlier_steps() {
        let index = StepIndex::default();
        let mut s = SessionSteps::new(
            "s1".into(),
            index.clone(),
            Some(steps(&[step(1, "a", 19, 23)])),
        );
        let indexed = index.get("s1").unwrap();
        assert!(indexed.complete);
        assert_eq!(indexed.order, vec!["a"]);
        s.turn_started();
        s.on_listing(steps(&[step(1, "a", 19, 23), step(2, "b", 23, 24)]));
        assert_eq!(s.turn_ended(None), Some(anchor(&["b"], None, None)));
        // Unknown earlier steps: only the answer's id is trusted.
        let mut u = SessionSteps::new("s2".into(), StepIndex::default(), None);
        u.turn_started();
        u.on_listing(steps(&[step(1, "a", 19, 23), step(2, "b", 23, 24)]));
        assert_eq!(u.turn_ended(Some("b")), Some(anchor(&["b"], None, None)));
        // The listing after the turn makes the earlier steps known.
        u.on_listing(steps(&[step(1, "a", 19, 23), step(2, "b", 23, 29)]));
        u.turn_started();
        u.on_listing(steps(&[
            step(1, "a", 19, 23),
            step(2, "b", 23, 29),
            step(3, "c", 29, 30),
        ]));
        assert_eq!(u.turn_ended(None), Some(anchor(&["c"], None, None)));
    }

    #[test]
    fn history_anchors_follow_the_client_message_ids() {
        let listed = steps(&[
            step(1, "a", 19, 23),
            json!({"stepId": "q", "stepNumber": 2, "kind": "questionAnswer",
                   "revertTargetNodeId": 23, "forkTargetNodeId": 26}),
            step(3, "b", 26, 29),
        ]);
        let anchors = history_anchors(
            &[Some("a".into()), None, Some("b".into()), Some("zz".into())],
            &listed,
        );
        assert_eq!(
            anchors,
            vec![
                Some(anchor(&["a", "q"], Some(19), Some(26))),
                None,
                Some(anchor(&["b"], Some(26), Some(29))),
                None
            ]
        );
    }

    fn point(anchor: Value, before: bool, previous: Option<Value>) -> ForkPoint {
        ForkPoint {
            anchor,
            before,
            previous,
        }
    }

    #[test]
    fn fork_targets_come_from_the_index_the_anchor_or_a_listing() {
        let listed = steps(&[step(1, "a", 19, 23), step(2, "b", 23, 29)]);
        let indexed = IndexedSession {
            steps: listed
                .iter()
                .map(|s| (s.step_id.clone(), s.clone()))
                .collect(),
            order: vec!["a".into(), "b".into()],
            complete: true,
        };
        let settled_b = anchor(&["b"], Some(23), Some(29));
        let provisional_b = anchor(&["b"], None, None);
        // The anchor's own node ids.
        assert_eq!(
            known_target(Some(&point(settled_b.clone(), false, None)), None),
            Ok(Some(ForkTarget::Node(29)))
        );
        assert_eq!(
            known_target(Some(&point(settled_b, true, None)), None),
            Ok(Some(ForkTarget::Node(23)))
        );
        // A provisional anchor: the index, else unknown.
        assert_eq!(
            known_target(
                Some(&point(provisional_b.clone(), false, None)),
                Some(&indexed)
            ),
            Ok(Some(ForkTarget::Node(29)))
        );
        assert_eq!(
            known_target(Some(&point(provisional_b.clone(), false, None)), None),
            Ok(None)
        );
        // Before a turn: the previous turn's anchor.
        assert_eq!(
            known_target(
                Some(&point(
                    provisional_b.clone(),
                    true,
                    Some(anchor(&["a"], Some(19), Some(23)))
                )),
                None
            ),
            Ok(Some(ForkTarget::Node(23)))
        );
        // The whole session: the index's last step when it is complete.
        assert_eq!(
            known_target(None, Some(&indexed)),
            Ok(Some(ForkTarget::Node(29)))
        );
        let running = IndexedSession {
            complete: false,
            ..indexed.clone()
        };
        assert_eq!(known_target(None, Some(&running)), Ok(None));
        assert_eq!(
            known_target(
                None,
                Some(&IndexedSession {
                    complete: true,
                    ..IndexedSession::default()
                })
            ),
            Ok(Some(ForkTarget::Empty))
        );
        // From a listing of the loaded source.
        assert_eq!(
            listed_target(Some(&point(provisional_b.clone(), false, None)), &listed),
            Ok(ForkTarget::Node(29))
        );
        assert_eq!(
            listed_target(Some(&point(provisional_b, true, None)), &listed),
            Ok(ForkTarget::Node(23))
        );
        assert_eq!(listed_target(None, &listed), Ok(ForkTarget::Node(29)));
        assert_eq!(listed_target(None, &[]), Ok(ForkTarget::Empty));
        assert!(matches!(
            listed_target(
                Some(&point(anchor(&["gone"], None, None), false, None)),
                &listed
            ),
            Err(AdapterError::Harness(_))
        ));
        // Another adapter's anchor.
        assert!(matches!(
            known_target(Some(&point(json!({"turn": 1}), false, None)), None),
            Err(AdapterError::Other(_))
        ));
    }

    #[test]
    fn steps_parse_tolerantly_in_devins_order() {
        let parsed = parse_steps(&[
            step(2, "b", 23, 29),
            json!("not a step"),
            json!({"stepNumber": 3}),
            step(1, "a", 19, 23),
        ]);
        let ids: Vec<&str> = parsed.iter().map(|s| s.step_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }
}
