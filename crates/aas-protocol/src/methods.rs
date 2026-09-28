//! Client → server methods: params and result types.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::*;
use crate::rpc::{ErrorKind, RpcError};
use crate::types::*;

/// Static description of a method, used by typed clients.
pub trait MethodSpec {
    const NAME: &'static str;
    /// Whether the method changes state (and therefore requires `clientRequestId`).
    const MUTATING: bool;
    type Params: Serialize + DeserializeOwned;
    type Result: Serialize + DeserializeOwned;
}

/// Access to the idempotency key of mutating params.
pub trait ClientRequestIdOf {
    fn client_request_id(&self) -> Option<&str>;
}

macro_rules! with_request_id {
    ($($t:ty),* $(,)?) => {
        $(impl ClientRequestIdOf for $t {
            fn client_request_id(&self) -> Option<&str> { Some(&self.client_request_id) }
        })*
    };
}

macro_rules! without_request_id {
    ($($t:ty),* $(,)?) => {
        $(impl ClientRequestIdOf for $t {
            fn client_request_id(&self) -> Option<&str> { None }
        })*
    };
}

macro_rules! client_requests {
    ($( $variant:ident => $name:literal, $params:ty, $result:ty, $mutating:literal; )*) => {
        /// A parsed client request.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "method", content = "params")]
        pub enum ClientRequest {
            $( #[serde(rename = $name)] $variant($params), )*
        }

        /// Every method name of protocol v1.
        pub const METHOD_NAMES: &[&str] = &[ $($name),* ];

        impl ClientRequest {
            pub fn method(&self) -> &'static str {
                match self { $( ClientRequest::$variant(_) => $name, )* }
            }

            pub fn is_mutating(&self) -> bool {
                match self { $( ClientRequest::$variant(_) => $mutating, )* }
            }

            pub fn client_request_id(&self) -> Option<&str> {
                match self { $( ClientRequest::$variant(p) => ClientRequestIdOf::client_request_id(p), )* }
            }

            /// Parses `params` for `method`. Missing or `null` params are treated as `{}`.
            pub fn parse(method: &str, params: Option<Value>) -> Result<Self, RpcError> {
                let params = match params {
                    None | Some(Value::Null) => Value::Object(Default::default()),
                    Some(v) => v,
                };
                match method {
                    $( $name => serde_json::from_value::<$params>(params)
                        .map(ClientRequest::$variant)
                        .map_err(|e| RpcError::invalid_params(format!("{}: {e}", $name))), )*
                    _ => Err(RpcError::new(ErrorKind::MethodNotFound, format!("unknown method {method}"))
                        .with("method", method)),
                }
            }

            /// The params as JSON (canonical field order, used for idempotency hashing).
            pub fn params_json(&self) -> Value {
                match self {
                    $( ClientRequest::$variant(p) => serde_json::to_value(p).expect("params always serialize"), )*
                }
            }
        }

        /// Parses a result of `method` into its typed form and serializes it back.
        /// Used by fixture tests to prove results round-trip through the types.
        pub fn result_roundtrip(method: &str, value: Value) -> Result<Value, String> {
            match method {
                $( $name => serde_json::from_value::<$result>(value)
                    .map(|r| serde_json::to_value(r).expect("result serializes"))
                    .map_err(|e| format!("{}: {e}", $name)), )*
                _ => Err(format!("unknown method {method}")),
            }
        }

        /// Marker types implementing [`MethodSpec`], one per method.
        pub mod spec {
            use super::*;
            $(
                pub struct $variant;
                impl MethodSpec for $variant {
                    const NAME: &'static str = $name;
                    const MUTATING: bool = $mutating;
                    type Params = $params;
                    type Result = $result;
                }
            )*
        }
    };
}

client_requests! {
    Initialize => "initialize", InitializeParams, InitializeResult, false;
    Subscribe => "subscribe", SubscribeParams, SubscribeResult, false;
    Unsubscribe => "unsubscribe", UnsubscribeParams, Empty, false;
    WorkspaceSnapshot => "workspace/snapshot", Empty, WorkspaceSnapshotResult, false;
    ServerStatus => "server/status", Empty, ServerStatusResult, false;
    DeviceList => "device/list", Empty, DeviceListResult, false;
    DeviceRevoke => "device/revoke", DeviceRevokeParams, Empty, true;
    HarnessList => "harness/list", Empty, HarnessListResult, false;
    HarnessRefresh => "harness/refresh", HarnessRefreshParams, HarnessListResult, false;
    ProjectList => "project/list", ProjectListParams, ProjectListResult, false;
    ProjectGet => "project/get", ProjectGetParams, ProjectResult, false;
    ProjectCreate => "project/create", ProjectCreateParams, ProjectCreateResult, true;
    ProjectOpen => "project/open", ProjectOpenParams, ProjectResult, true;
    ProjectUpdate => "project/update", ProjectUpdateParams, ProjectResult, true;
    ProjectArchive => "project/archive", ProjectArchiveParams, ProjectResult, true;
    ProjectRemove => "project/remove", ProjectRemoveParams, Empty, true;
    FsRoots => "fs/roots", Empty, FsRootsResult, false;
    FsList => "fs/list", FsListParams, FsListResult, false;
    FsMkdir => "fs/mkdir", FsMkdirParams, FsMkdirResult, true;
    FsSearch => "fs/search", FsSearchParams, FsSearchResult, false;
    ThreadList => "thread/list", ThreadListParams, ThreadListResult, false;
    ThreadGet => "thread/get", ThreadGetParams, ThreadResult, false;
    ThreadCreate => "thread/create", ThreadCreateParams, ThreadCreateResult, true;
    ThreadRead => "thread/read", ThreadReadParams, ThreadReadResult, false;
    ThreadUpdate => "thread/update", ThreadUpdateParams, ThreadUpdateResult, true;
    ThreadArchive => "thread/archive", ThreadArchiveParams, ThreadResult, true;
    ThreadFork => "thread/fork", ThreadForkParams, ThreadResult, true;
    ThreadStop => "thread/stop", ThreadStopParams, ThreadResult, true;
    ThreadDiff => "thread/diff", ThreadDiffParams, ThreadDiffResult, false;
    TurnStart => "turn/start", TurnStartParams, TurnStartResult, true;
    TurnInterrupt => "turn/interrupt", TurnInterruptParams, TurnInterruptResult, true;
    QueueRemove => "queue/remove", QueueRemoveParams, QueueRemoveResult, true;
    QueueResume => "queue/resume", QueueResumeParams, QueueResumeResult, true;
    QueueUpdate => "queue/update", QueueUpdateParams, QueueUpdateResult, true;
    QueueSteer => "queue/steer", QueueSteerParams, QueueSteerResult, true;
    InteractionRespond => "interaction/respond", InteractionRespondParams, InteractionRespondResult, true;
    InteractionList => "interaction/list", InteractionListParams, InteractionListResult, false;
    CommandList => "command/list", CommandListParams, CommandListResult, false;
    NativeList => "native/list", NativeListParams, NativeListResult, false;
    NativeImport => "native/import", NativeImportParams, ThreadResult, true;
    OperationList => "operation/list", Empty, OperationListResult, false;
    OperationCancel => "operation/cancel", OperationCancelParams, OperationResult, true;
    BackgroundTaskStop => "backgroundTask/stop", BackgroundTaskStopParams, BackgroundTaskResult, true;
}

with_request_id!(
    DeviceRevokeParams,
    ProjectCreateParams,
    ProjectOpenParams,
    ProjectUpdateParams,
    ProjectArchiveParams,
    ProjectRemoveParams,
    FsMkdirParams,
    ThreadCreateParams,
    ThreadUpdateParams,
    ThreadArchiveParams,
    ThreadForkParams,
    ThreadStopParams,
    TurnStartParams,
    TurnInterruptParams,
    QueueRemoveParams,
    QueueResumeParams,
    QueueUpdateParams,
    QueueSteerParams,
    InteractionRespondParams,
    NativeImportParams,
    OperationCancelParams,
    BackgroundTaskStopParams,
);

without_request_id!(
    Empty,
    InitializeParams,
    SubscribeParams,
    UnsubscribeParams,
    HarnessRefreshParams,
    ProjectListParams,
    ProjectGetParams,
    FsListParams,
    FsSearchParams,
    ThreadListParams,
    ThreadGetParams,
    ThreadReadParams,
    ThreadDiffParams,
    InteractionListParams,
    CommandListParams,
    NativeListParams,
);

/// `{}` params or result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Empty {}

// ----- connection & server -----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client: ClientInfo,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_known_epoch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
    pub platform: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub server: ServerInfo,
    pub device: DeviceInfo,
    pub epoch_changed: bool,
    pub policy: ClientPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub hostname: String,
    pub epoch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeParams {
    pub subscriptions: Vec<Subscription>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub stream: String,
    /// Last sequence number the client has applied (0 = from the beginning).
    pub after: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeResult {
    pub subscriptions: Vec<SubscriptionStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionStatus {
    pub stream: String,
    pub head: u64,
    pub status: SubscriptionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SubscriptionState {
    Ok,
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeParams {
    pub streams: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSnapshotResult {
    pub harnesses: Vec<Harness>,
    pub projects: Vec<Project>,
    pub threads: Vec<Thread>,
    pub pending_interactions: Vec<Interaction>,
    pub operations: Vec<Operation>,
    pub head: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatusResult {
    pub uptime_ms: u64,
    pub running_processes: u32,
    pub running_turns: u32,
    pub draining: bool,
    /// `policy.prevent_sleep_while_running` of the daemon: the PC is kept awake while a turn
    /// runs, and while background work keeps an agent busy.
    #[serde(default)]
    pub prevent_sleep_while_running: bool,
    /// Background tasks that keep an agent's process alive (in the harness's live set and not
    /// ambient), over all threads. A drain waits for them too.
    #[serde(default)]
    pub running_background_tasks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceListResult {
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRevokeParams {
    pub client_request_id: String,
    pub device_id: DeviceId,
}

// ----- harnesses -----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HarnessListResult {
    pub harnesses: Vec<Harness>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRefreshParams {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub harness_id: Option<String>,
}

// ----- projects & filesystem -----

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectListParams {
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectListResult {
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGetParams {
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectResult {
    pub project: Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCreateParams {
    pub client_request_id: String,
    pub parent_path: String,
    pub name: String,
    pub init: ProjectInit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCreateResult {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project: Option<Project>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub operation: Option<Operation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectOpenParams {
    pub client_request_id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectUpdateParams {
    pub client_request_id: String,
    pub project_id: ProjectId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub defaults: Option<ProjectDefaults>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectArchiveParams {
    pub client_request_id: String,
    pub project_id: ProjectId,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRemoveParams {
    pub client_request_id: String,
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsRootsResult {
    pub roots: Vec<FsRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsListParams {
    pub path: String,
    #[serde(default)]
    pub include_files: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsListResult {
    pub path: String,
    pub entries: Vec<FsEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsMkdirParams {
    pub client_request_id: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsMkdirResult {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsSearchParams {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_id: Option<ProjectId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thread_id: Option<ThreadId>,
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsSearchResult {
    pub results: Vec<SearchResult>,
    /// Always `"heuristic:H1"`: the order is a fuzzy-match ranking (see design.md).
    pub ranking: String,
}

// ----- threads & turns -----

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListParams {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub before: Option<ThreadCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCursor {
    pub last_activity_at: Millis,
    pub id: ThreadId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResult {
    pub threads: Vec<Thread>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadGetParams {
    pub thread_id: ThreadId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResult {
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCreateParams {
    pub client_request_id: String,
    pub project_id: ProjectId,
    pub harness_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub settings: Option<ThreadSettings>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub workspace: Option<WorkspaceSpec>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub input: Option<Vec<InputPart>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCreateResult {
    pub thread: Thread,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub turn_id: Option<TurnId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub disposition: Option<Disposition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadParams {
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub before_turn_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub limit_turns: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadResult {
    pub thread: Thread,
    pub turns: Vec<Turn>,
    pub items: Vec<Item>,
    pub interactions: Vec<Interaction>,
    pub queued: Vec<QueuedInput>,
    /// The background tasks first reported during the returned turns, and every task that is
    /// still running (oldest first).
    #[serde(default)]
    pub background_tasks: Vec<BackgroundTask>,
    pub head: u64,
    pub has_more_before: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUpdateParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub settings: Option<ThreadSettings>,
    /// Pins (`true`) or unpins (`false`) the thread.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pinned: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUpdateResult {
    pub thread: Thread,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub settings_outcome: Option<SettingsOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SettingsOutcome {
    AppliedLive,
    AppliesNextTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadArchiveParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
    pub archived: bool,
    #[serde(default)]
    pub remove_worktree: bool,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadForkParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub at_turn_id: Option<TurnId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStopParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDiffParams {
    pub thread_id: ThreadId,
    pub scope: DiffScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDiffResult {
    pub summary: DiffSummary,
    pub files: Vec<DiffFile>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub patch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub patch_blob_id: Option<BlobId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
    pub input: Vec<InputPart>,
    #[serde(default)]
    pub delivery: Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResult {
    pub disposition: Disposition,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub turn_id: Option<TurnId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub queued_id: Option<QueuedInputId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptResult {
    pub interrupted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueRemoveParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
    pub queued_id: QueuedInputId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueRemoveResult {
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueResumeParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueResumeResult {
    /// Set when the next queued input started a turn right away.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub turn_id: Option<TurnId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueUpdateParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
    pub queued_id: QueuedInputId,
    /// Replaces the queued input (validated like `turn/start` input).
    pub input: Vec<InputPart>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueUpdateResult {
    /// `false` when the entry had already left the queue (started or removed).
    pub updated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueSteerParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
    pub queued_id: QueuedInputId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueSteerResult {
    /// `steered` (sent into the running turn) or `started` (no turn was running: the entry
    /// started one). Absent when the entry had already left the queue.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub disposition: Option<Disposition>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub turn_id: Option<TurnId>,
}

// ----- interactions, commands, native sessions, operations -----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRespondParams {
    pub client_request_id: String,
    pub interaction_id: InteractionId,
    pub resolution: InteractionResolution,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRespondResult {
    pub interaction: Interaction,
    pub already_resolved: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionListParams {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<InteractionStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractionListResult {
    pub interactions: Vec<Interaction>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandListParams {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thread_id: Option<ThreadId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_id: Option<ProjectId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub harness_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandListResult {
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NativeListParams {
    pub project_id: ProjectId,
    pub harness_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NativeListResult {
    pub sessions: Vec<NativeSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NativeImportParams {
    pub client_request_id: String,
    pub project_id: ProjectId,
    pub harness_id: String,
    pub native_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperationListResult {
    pub operations: Vec<Operation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperationCancelParams {
    pub client_request_id: String,
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperationResult {
    pub operation: Operation,
}

// ----- background tasks -----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskStopParams {
    pub client_request_id: String,
    pub thread_id: ThreadId,
    pub task_id: BackgroundTaskId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskResult {
    pub task: BackgroundTask,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for name in METHOD_NAMES {
            assert!(seen.insert(*name), "duplicate method {name}");
        }
    }

    #[test]
    fn parse_known_and_unknown_methods() {
        let req = ClientRequest::parse(
            "turn/start",
            Some(serde_json::json!({
                "clientRequestId": "c1",
                "threadId": "thr_1",
                "input": [{"type": "text", "text": "hi"}]
            })),
        )
        .unwrap();
        assert_eq!(req.method(), "turn/start");
        assert!(req.is_mutating());
        assert_eq!(req.client_request_id(), Some("c1"));
        match &req {
            ClientRequest::TurnStart(p) => assert_eq!(p.delivery, Delivery::Auto),
            other => panic!("unexpected {other:?}"),
        }

        let err = ClientRequest::parse("nope", None).unwrap_err();
        assert_eq!(err.kind(), Some(ErrorKind::MethodNotFound));

        let err = ClientRequest::parse("turn/start", Some(serde_json::json!({}))).unwrap_err();
        assert_eq!(err.kind(), Some(ErrorKind::InvalidParams));
    }

    #[test]
    fn empty_params_accept_missing_and_null() {
        assert!(matches!(
            ClientRequest::parse("harness/list", None).unwrap(),
            ClientRequest::HarnessList(_)
        ));
        assert!(matches!(
            ClientRequest::parse("harness/list", Some(Value::Null)).unwrap(),
            ClientRequest::HarnessList(_)
        ));
    }

    #[test]
    fn every_mutating_method_has_a_request_id() {
        // The macro tables must agree with MethodSpec::MUTATING.
        use crate::examples::all_requests;
        for req in all_requests() {
            assert_eq!(
                req.is_mutating(),
                req.client_request_id().is_some(),
                "{} mutating flag and clientRequestId disagree",
                req.method()
            );
        }
    }
}
