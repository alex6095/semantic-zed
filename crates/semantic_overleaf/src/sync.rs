use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::credentials::{CredentialError, CredentialStore};
use crate::http::{HttpError, Identity, OverleafHttpClient};
use crate::merge::{MergeResult, desired_change_is_present, merge_text};
use crate::ot::{diff_to_history_operations, diff_to_sharejs_operations};
use crate::ownership::{OwnershipError, WorkspaceOwnership};
use crate::project_model::{EntityKind, ProjectEntity, ProjectModel, ProjectModelError};
use crate::realtime::{DocumentSnapshot, OverleafRealtimeSession, RealtimeError, RealtimeEvent};
use crate::replica::{ConflictRecord, DocumentRecord, EntityRecord, ReplicaError, ReplicaStore};
use crate::scan::{
    ScanError, TreeSnapshot, atomic_write, digest_bytes, is_text_document_path, scan_tree,
};

const DEFAULT_SERVER: &str = "https://www.overleaf.com/";
const CONFIRM_DELAYS: [u64; 7] = [0, 30, 60, 120, 250, 500, 1_000];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeProjectConfig {
    #[serde(default = "default_config_version")]
    pub version: u32,
    #[serde(default = "default_server")]
    pub server: String,
    pub project_id: String,
    #[serde(default = "default_scan_interval")]
    pub scan_interval_ms: u64,
    #[serde(default)]
    pub adopt_local_on_first_sync: bool,
    #[serde(default = "default_allow_binary_writes")]
    pub allow_binary_writes: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub initialized_at: Option<String>,
}

impl NativeProjectConfig {
    pub fn new(project_id: impl Into<String>) -> Result<Self, NativeSyncError> {
        let project_id = project_id.into();
        if project_id.trim().is_empty() {
            return Err(NativeSyncError::InvalidConfig(
                "projectId must not be empty".into(),
            ));
        }
        Ok(Self {
            version: default_config_version(),
            server: default_server(),
            project_id,
            scan_interval_ms: default_scan_interval(),
            adopt_local_on_first_sync: false,
            allow_binary_writes: default_allow_binary_writes(),
            created_at: None,
            initialized_at: None,
        })
    }

    pub fn load(root: &Path) -> Result<Self, NativeSyncError> {
        let path = root.join(".semantic-zed/project.json");
        let bytes = fs::read(&path).map_err(|error| NativeSyncError::ConfigIo {
            path: path.clone(),
            source: error,
        })?;
        let config: Self = serde_json::from_slice(&bytes)?;
        if config.project_id.trim().is_empty() {
            return Err(NativeSyncError::InvalidConfig(
                "projectId must not be empty".into(),
            ));
        }
        Ok(config)
    }

    pub fn save(&self, root: &Path) -> Result<(), NativeSyncError> {
        if self.project_id.trim().is_empty() {
            return Err(NativeSyncError::InvalidConfig(
                "projectId must not be empty".into(),
            ));
        }
        let path = root.join(".semantic-zed/project.json");
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        atomic_write(&path, &bytes, 0o644)?;
        Ok(())
    }
}

fn default_config_version() -> u32 {
    1
}

fn default_server() -> String {
    DEFAULT_SERVER.into()
}

fn default_scan_interval() -> u64 {
    750
}

fn default_allow_binary_writes() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentCounters {
    pub total: usize,
    pub clean: usize,
    pub dirty: usize,
    pub conflicted: usize,
    pub pending: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativePresence {
    pub client_id: String,
    pub name: String,
    pub document_path: String,
    pub row: u32,
    pub column: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeStatus {
    pub state: String,
    pub connected: bool,
    pub server: String,
    pub project_id: String,
    pub project_name: Option<String>,
    pub root: PathBuf,
    pub documents: DocumentCounters,
    pub conflicts: Vec<ConflictRecord>,
    pub pending_operations: Vec<Value>,
    pub collaborators: Vec<NativePresence>,
    pub last_compile: Option<Value>,
    pub last_error: Option<String>,
}

impl NativeStatus {
    pub fn as_value(&self) -> Result<Value, NativeSyncError> {
        Ok(serde_json::to_value(self)?)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NativeSyncEvent {
    StatusChanged {
        status: NativeStatus,
    },
    DocumentChanged {
        path: String,
        origin: String,
        text: String,
    },
    PresenceChanged {
        collaborators: Vec<NativePresence>,
    },
    PdfChanged {
        path: PathBuf,
        compile: Value,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeDocumentSnapshot {
    pub id: String,
    pub path: String,
    pub version: u64,
    pub state: String,
    pub pending: bool,
    pub clean: bool,
    pub ot_type: String,
    pub read_only_reason: Option<String>,
    pub text: String,
}

#[derive(Debug, Error)]
pub enum NativeSyncError {
    #[error("could not read native Overleaf config {path}: {source}")]
    ConfigIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid native Overleaf config: {0}")]
    InvalidConfig(String),
    #[error("no saved Overleaf login exists for {0}")]
    MissingCredentials(String),
    #[error("native Overleaf sync actor stopped")]
    ActorStopped,
    #[error("unknown Overleaf document path: {0}")]
    UnknownDocument(String),
    #[error("Overleaf document is conflicted: {0}")]
    Conflicted(String),
    #[error("Overleaf document is read-only: {0}")]
    ReadOnly(String),
    #[error("Overleaf write was acknowledged but not proven in the authoritative snapshot: {0}")]
    WriteNotProven(String),
    #[error("Cloud compile is blocked by unresolved conflicts")]
    CompileBlocked,
    #[error("native Overleaf I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Credentials(#[from] CredentialError),
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Realtime(#[from] RealtimeError),
    #[error(transparent)]
    Project(#[from] ProjectModelError),
    #[error(transparent)]
    Ownership(#[from] OwnershipError),
    #[error(transparent)]
    Replica(#[from] ReplicaError),
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct NativeSyncHandle {
    commands: mpsc::UnboundedSender<NativeCommand>,
    events: broadcast::Sender<NativeSyncEvent>,
}

impl NativeSyncHandle {
    pub async fn start_from_root(root: impl Into<PathBuf>) -> Result<Self, NativeSyncError> {
        let root = root.into();
        let config = NativeProjectConfig::load(&root)?;
        let record = CredentialStore::default()
            .load(&config.server)?
            .ok_or_else(|| NativeSyncError::MissingCredentials(config.server.clone()))?;
        Self::start(root, config, record.identity).await
    }

    pub async fn start(
        root: impl Into<PathBuf>,
        config: NativeProjectConfig,
        identity: Identity,
    ) -> Result<Self, NativeSyncError> {
        let root = root.into();
        let (events, _) = broadcast::channel(256);
        let mut engine = NativeProjectSync::connect(root, config, identity, events.clone()).await?;
        let realtime_events = engine.realtime.subscribe();
        let (commands, command_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            engine.run(command_rx, realtime_events).await;
        });
        Ok(Self { commands, events })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NativeSyncEvent> {
        self.events.subscribe()
    }

    pub async fn status(&self) -> Result<NativeStatus, NativeSyncError> {
        let (sender, receiver) = oneshot::channel();
        self.commands
            .send(NativeCommand::Status(sender))
            .map_err(|_| NativeSyncError::ActorStopped)?;
        receiver.await.map_err(|_| NativeSyncError::ActorStopped)?
    }

    pub async fn scan(&self) -> Result<NativeStatus, NativeSyncError> {
        let (sender, receiver) = oneshot::channel();
        self.commands
            .send(NativeCommand::Scan(sender))
            .map_err(|_| NativeSyncError::ActorStopped)?;
        receiver.await.map_err(|_| NativeSyncError::ActorStopped)?
    }

    pub async fn compile(
        &self,
        root_resource_path: Option<String>,
    ) -> Result<NativeStatus, NativeSyncError> {
        let (sender, receiver) = oneshot::channel();
        self.commands
            .send(NativeCommand::Compile {
                root_resource_path,
                response: sender,
            })
            .map_err(|_| NativeSyncError::ActorStopped)?;
        receiver.await.map_err(|_| NativeSyncError::ActorStopped)?
    }

    pub async fn apply_snapshot(
        &self,
        path: impl Into<String>,
        text: impl Into<String>,
        origin: impl Into<String>,
    ) -> Result<NativeDocumentSnapshot, NativeSyncError> {
        let (sender, receiver) = oneshot::channel();
        self.commands
            .send(NativeCommand::ApplySnapshot {
                path: path.into(),
                text: text.into(),
                origin: origin.into(),
                response: sender,
            })
            .map_err(|_| NativeSyncError::ActorStopped)?;
        receiver.await.map_err(|_| NativeSyncError::ActorStopped)?
    }

    pub async fn update_position(
        &self,
        path: impl Into<String>,
        row: u32,
        column: u32,
    ) -> Result<(), NativeSyncError> {
        let (sender, receiver) = oneshot::channel();
        self.commands
            .send(NativeCommand::UpdatePosition {
                path: path.into(),
                row,
                column,
                response: sender,
            })
            .map_err(|_| NativeSyncError::ActorStopped)?;
        receiver.await.map_err(|_| NativeSyncError::ActorStopped)?
    }

    pub async fn stop(&self) -> Result<(), NativeSyncError> {
        let (sender, receiver) = oneshot::channel();
        self.commands
            .send(NativeCommand::Stop(sender))
            .map_err(|_| NativeSyncError::ActorStopped)?;
        receiver.await.map_err(|_| NativeSyncError::ActorStopped)?;
        Ok(())
    }
}

enum NativeCommand {
    Status(oneshot::Sender<Result<NativeStatus, NativeSyncError>>),
    Scan(oneshot::Sender<Result<NativeStatus, NativeSyncError>>),
    Compile {
        root_resource_path: Option<String>,
        response: oneshot::Sender<Result<NativeStatus, NativeSyncError>>,
    },
    ApplySnapshot {
        path: String,
        text: String,
        origin: String,
        response: oneshot::Sender<Result<NativeDocumentSnapshot, NativeSyncError>>,
    },
    UpdatePosition {
        path: String,
        row: u32,
        column: u32,
        response: oneshot::Sender<Result<(), NativeSyncError>>,
    },
    Stop(oneshot::Sender<()>),
}

struct NativeDocument {
    id: String,
    path: String,
    version: u64,
    ot_type: String,
    state: String,
    base_text: String,
    local_text: String,
    remote_text: String,
    raw_snapshot: Value,
    ranges: Value,
    read_only_reason: Option<String>,
    pending: bool,
}

impl NativeDocument {
    fn clean(&self) -> bool {
        self.state == "ready" && !self.pending && self.local_text == self.remote_text
    }

    fn snapshot(&self) -> NativeDocumentSnapshot {
        NativeDocumentSnapshot {
            id: self.id.clone(),
            path: self.path.clone(),
            version: self.version,
            state: self.state.clone(),
            pending: self.pending,
            clean: self.clean(),
            ot_type: self.ot_type.clone(),
            read_only_reason: self.read_only_reason.clone(),
            text: self.local_text.clone(),
        }
    }

    fn record(&self) -> DocumentRecord {
        DocumentRecord {
            id: self.id.clone(),
            path: self.path.clone(),
            version: self.version,
            last_version: self.version.checked_sub(1),
            ot_type: self.ot_type.clone(),
            state: self.state.clone(),
            collaborator_revision: 0,
            base_text: self.base_text.clone(),
            local_text: self.local_text.clone(),
            remote_text: self.remote_text.clone(),
            raw_snapshot: self.raw_snapshot.clone(),
            ranges: self.ranges.clone(),
            read_only_reason: self.read_only_reason.clone(),
            pending: self.pending.then(|| json!({ "native": true })),
            updated_at: String::new(),
            extra: Map::new(),
        }
    }
}

struct NativeProjectSync {
    root: PathBuf,
    config: NativeProjectConfig,
    identity: Identity,
    http: OverleafHttpClient,
    realtime: Arc<OverleafRealtimeSession>,
    model: ProjectModel,
    store: ReplicaStore,
    documents: HashMap<String, NativeDocument>,
    presence: HashMap<String, Value>,
    last_compile: Option<Value>,
    last_error: Option<String>,
    connection_state: String,
    events: broadcast::Sender<NativeSyncEvent>,
    _ownership: WorkspaceOwnership,
    missing_counts: HashMap<String, u8>,
}

impl NativeProjectSync {
    async fn connect(
        root: PathBuf,
        config: NativeProjectConfig,
        identity: Identity,
        events: broadcast::Sender<NativeSyncEvent>,
    ) -> Result<Self, NativeSyncError> {
        let ownership = WorkspaceOwnership::acquire(&root, &config.project_id)?;
        let realtime = Arc::new(
            OverleafRealtimeSession::connect_and_join(
                &config.server,
                &identity,
                &config.project_id,
            )
            .await?,
        );
        let model = ProjectModel::from_project(realtime.project().clone())?;
        let store = ReplicaStore::open(&root, &config.server, &config.project_id)?;
        let mut http = OverleafHttpClient::new(&config.server)?;
        http.set_identity(identity.clone());
        let last_compile = read_optional_json(&root.join(".semantic-zed/output/compile.json"));
        let mut sync = Self {
            root,
            config,
            identity,
            http,
            realtime,
            model,
            store,
            documents: HashMap::new(),
            presence: HashMap::new(),
            last_compile,
            last_error: None,
            connection_state: "connecting".into(),
            events,
            _ownership: ownership,
            missing_counts: HashMap::new(),
        };
        sync.recover_pending_operations().await;
        sync.bootstrap_directories()?;
        sync.bootstrap_documents().await?;
        sync.bootstrap_files().await?;
        sync.refresh_presence().await?;
        sync.connection_state = "live".into();
        sync.persist_status()?;
        Ok(sync)
    }

    async fn run(
        &mut self,
        mut commands: mpsc::UnboundedReceiver<NativeCommand>,
        mut realtime_events: broadcast::Receiver<RealtimeEvent>,
    ) {
        let mut scan_interval = tokio::time::interval(Duration::from_millis(
            self.config.scan_interval_ms.clamp(100, 60_000),
        ));
        scan_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let _ = scan_interval.tick().await;
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    if self.handle_command(command).await { break; }
                }
                event = realtime_events.recv() => {
                    match event {
                        Ok(event) if event.name == "disconnect" => {
                            self.connection_state = "reconnecting".into();
                            self.emit_status();
                            match self.reconnect().await {
                                Ok(receiver) => realtime_events = receiver,
                                Err(error) => self.record_error(error),
                            }
                        }
                        Ok(event) => {
                            if let Err(error) = self.handle_realtime_event(event).await {
                                self.record_error(error);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            if let Err(error) = self.refresh_all_documents("event-lag").await {
                                self.record_error(error);
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = scan_interval.tick() => {
                    if self.connection_state != "live" {
                        match self.reconnect().await {
                            Ok(receiver) => realtime_events = receiver,
                            Err(error) => self.record_error(error),
                        }
                    } else if let Err(error) = self.scan_documents("filesystem").await {
                        self.record_error(error);
                    }
                }
            }
        }
        self.realtime.disconnect();
        self.connection_state = "stopped".into();
        let _ = self.persist_status();
    }

    async fn handle_command(&mut self, command: NativeCommand) -> bool {
        match command {
            NativeCommand::Status(response) => {
                let _ = response.send(Ok(self.status()));
            }
            NativeCommand::Scan(response) => {
                let result = self.scan_documents("manual").await.map(|_| self.status());
                let _ = response.send(result);
            }
            NativeCommand::Compile {
                root_resource_path,
                response,
            } => {
                let result = self
                    .compile(root_resource_path.as_deref())
                    .await
                    .map(|_| self.status());
                let _ = response.send(result);
            }
            NativeCommand::ApplySnapshot {
                path,
                text,
                origin,
                response,
            } => {
                let result = self.apply_snapshot(&path, text, &origin).await;
                let _ = response.send(result);
            }
            NativeCommand::UpdatePosition {
                path,
                row,
                column,
                response,
            } => {
                let result = self.update_position(&path, row, column).await;
                let _ = response.send(result);
            }
            NativeCommand::Stop(response) => {
                let _ = response.send(());
                return true;
            }
        }
        false
    }

    async fn bootstrap_documents(&mut self) -> Result<(), NativeSyncError> {
        let documents = self.model.documents().cloned().collect::<Vec<_>>();
        for entity in documents {
            self.move_stored_document_if_needed(&entity)?;
            let snapshot = self.realtime.join_document(&entity.id, None, None).await?;
            let document =
                self.bootstrap_document(&entity, snapshot, self.config.adopt_local_on_first_sync)?;
            self.store.upsert_document(document.record())?;
            self.documents.insert(entity.id.clone(), document);
        }
        self.store.persist()?;
        let dirty = self
            .documents
            .values()
            .filter(|document| {
                document.state != "conflicted" && document.local_text != document.remote_text
            })
            .map(|document| document.id.clone())
            .collect::<Vec<_>>();
        for id in dirty {
            self.flush_document(&id, "bootstrap-rebase").await?;
        }
        Ok(())
    }

    async fn recover_pending_operations(&mut self) {
        let operations = self
            .store
            .state()
            .operations
            .values()
            .filter(|operation| operation_string(operation, "kind") == Some("binary-replace"))
            .cloned()
            .collect::<Vec<_>>();
        for operation in operations {
            if let Err(error) = self.recover_binary_replace(&operation).await {
                self.last_error = Some(format!(
                    "Could not recover interrupted binary replacement: {error}"
                ));
            }
        }
    }

    async fn recover_binary_replace(&mut self, operation: &Value) -> Result<(), NativeSyncError> {
        let operation_id = required_operation_string(operation, "id")?;
        let parent_id = required_operation_string(operation, "parentId")?;
        let old_id = required_operation_string(operation, "oldId")?;
        let original_name = required_operation_string(operation, "originalName")?;
        let original_path = operation_string(operation, "path")
            .or_else(|| operation_string(operation, "originalPath"))
            .ok_or_else(|| {
                NativeSyncError::InvalidConfig(format!(
                    "binary operation {operation_id} has no original path"
                ))
            })?;
        let stage_name = required_operation_string(operation, "stageName")?;
        let desired_digest = required_operation_string(operation, "desiredDigest")?;
        let parent = self
            .model
            .get_by_id(parent_id)
            .filter(|entity| entity.kind == EntityKind::Folder)
            .cloned()
            .ok_or_else(|| {
                NativeSyncError::InvalidConfig(format!(
                    "binary operation {operation_id} has no parent folder {parent_id}"
                ))
            })?;
        let stage_path = if parent.path.is_empty() {
            stage_name.to_owned()
        } else {
            format!("{}/{stage_name}", parent.path)
        };
        let visible = self.model.get_by_path(original_path)?.cloned();
        let stage = self
            .model
            .get_by_path(&stage_path)?
            .cloned()
            .or_else(|| self.model.get_by_id(old_id).cloned());

        if let Some(visible) = visible.filter(|entity| entity.kind == EntityKind::File) {
            let remote = self
                .http
                .download_file(&self.config.project_id, &visible.id)
                .await?;
            let remote_digest = digest_bytes(&remote);
            if remote_digest == desired_digest {
                if let Some(stage) = stage.filter(|stage| stage.id != visible.id) {
                    self.http
                        .delete_entity(&self.config.project_id, "file", &stage.id)
                        .await?;
                    self.model.remove(&stage.id);
                    self.store.remove_file(&stage.id);
                }
                atomic_write(&self.root.join(original_path), &remote, 0o644)?;
                self.store.upsert_file(entity_record(
                    &visible.id,
                    original_path,
                    Some(remote_digest),
                    "file",
                ))?;
                self.store.complete_operation(operation_id)?;
                return Ok(());
            }
            if visible.id == old_id && visible.path == original_path {
                self.store.complete_operation(operation_id)?;
                return Ok(());
            }
            let local = read_optional_bytes(&self.root.join(original_path))?;
            if !self.path_is_conflicted(original_path) {
                self.store.record_conflict(
                    old_id,
                    original_path,
                    "binary",
                    "binary-recovery-visible-mismatch",
                    None,
                    local.as_deref(),
                    Some(&remote),
                )?;
            }
            return Err(NativeSyncError::WriteNotProven(original_path.into()));
        }

        if let Some(stage) = stage.filter(|entity| entity.kind == EntityKind::File) {
            self.http
                .rename_entity(&self.config.project_id, "file", &stage.id, original_name)
                .await?;
            self.model.rename(&stage.id, original_name)?;
            self.store.complete_operation(operation_id)?;
            return Ok(());
        }

        let local = read_optional_bytes(&self.root.join(original_path))?;
        if !self.path_is_conflicted(original_path) {
            self.store.record_conflict(
                old_id,
                original_path,
                "binary",
                "binary-recovery-both-missing",
                None,
                local.as_deref(),
                Some(&[]),
            )?;
        }
        Err(NativeSyncError::WriteNotProven(original_path.into()))
    }

    fn bootstrap_directories(&mut self) -> Result<(), NativeSyncError> {
        let mut directories = self.model.folders().cloned().collect::<Vec<_>>();
        directories.sort_by_key(|entity| entity.path.matches('/').count());
        for entity in directories {
            let stored_path = self
                .store
                .state()
                .directories
                .get(&entity.id)
                .map(|stored| stored.path.clone());
            if let Some(stored_path) = stored_path
                && stored_path != entity.path
            {
                self.move_materialized_path(&stored_path, &entity.path)?;
            }
            if !entity.path.is_empty() {
                fs::create_dir_all(self.root.join(&entity.path))?;
            }
            self.store
                .upsert_directory(entity_record(&entity.id, &entity.path, None, "folder"))?;
        }
        self.store.persist()?;
        Ok(())
    }

    async fn bootstrap_files(&mut self) -> Result<(), NativeSyncError> {
        let files = self.model.files().cloned().collect::<Vec<_>>();
        for entity in files {
            if self.is_managed_stage_entity(&entity) || self.path_is_conflicted(&entity.path) {
                continue;
            }
            let stored_path = self
                .store
                .state()
                .files
                .get(&entity.id)
                .map(|stored| stored.path.clone());
            if let Some(stored_path) = stored_path
                && stored_path != entity.path
            {
                self.move_materialized_path(&stored_path, &entity.path)?;
            }
            let remote = self
                .http
                .download_file(&self.config.project_id, &entity.id)
                .await?;
            let remote_digest = digest_bytes(&remote);
            let local_path = self.root.join(&entity.path);
            let local = read_optional_bytes(&local_path)?;
            let stored = self.store.state().files.get(&entity.id).cloned();
            let local_digest = local.as_deref().map(digest_bytes);
            let base_digest = stored
                .as_ref()
                .and_then(|record| record.base_digest.clone())
                .or_else(|| stored.as_ref().and_then(|record| record.digest.clone()));
            let materialized_digest;
            match (local, local_digest.as_deref(), base_digest.as_deref()) {
                (None, _, _) => {
                    atomic_write(&local_path, &remote, 0o644)?;
                    materialized_digest = remote_digest.clone();
                }
                (Some(_local), Some(local_digest), _) if local_digest == remote_digest => {
                    materialized_digest = local_digest.into();
                }
                (Some(_), Some(local_digest), Some(base)) if local_digest == base => {
                    atomic_write(&local_path, &remote, 0o644)?;
                    materialized_digest = remote_digest.clone();
                }
                (Some(local), Some(_), Some(base)) if remote_digest == base => {
                    if !self.config.allow_binary_writes {
                        self.record_binary_conflict(
                            &entity,
                            "binary-writes-disabled",
                            None,
                            &local,
                            &remote,
                        )?;
                        continue;
                    }
                    let replacement = self.replace_remote_binary(&entity, local).await?;
                    materialized_digest = replacement.1;
                }
                (Some(local), Some(_), None) if self.config.adopt_local_on_first_sync => {
                    if !self.config.allow_binary_writes {
                        self.record_binary_conflict(
                            &entity,
                            "binary-writes-disabled",
                            None,
                            &local,
                            &remote,
                        )?;
                        continue;
                    }
                    let replacement = self.replace_remote_binary(&entity, local).await?;
                    materialized_digest = replacement.1;
                }
                (Some(local), _, _) => {
                    self.record_binary_conflict(
                        &entity,
                        "binary-overlapping-edits",
                        None,
                        &local,
                        &remote,
                    )?;
                    continue;
                }
            }
            let current = self
                .model
                .get_by_path(&entity.path)?
                .cloned()
                .unwrap_or(entity);
            self.store.upsert_file(entity_record(
                &current.id,
                &current.path,
                Some(materialized_digest),
                "file",
            ))?;
        }
        self.store.persist()?;
        Ok(())
    }

    async fn replace_remote_binary(
        &mut self,
        entity: &ProjectEntity,
        bytes: Vec<u8>,
    ) -> Result<(ProjectEntity, String), NativeSyncError> {
        let parent_id = entity
            .parent_id
            .clone()
            .ok_or_else(|| NativeSyncError::InvalidConfig("binary file has no parent".into()))?;
        let desired_digest = digest_bytes(&bytes);
        let stage_name = format!(
            ".semantic-zed-stage-{}-{}",
            &entity.id[..entity.id.len().min(10)],
            entity.name
        );
        let operation_id = self.store.begin_operation(json!({
            "kind": "binary-replace",
            "path": entity.path,
            "oldId": entity.id,
            "parentId": parent_id,
            "originalName": entity.name,
            "stageName": stage_name,
            "desiredDigest": desired_digest
        }))?;
        self.http
            .rename_entity(&self.config.project_id, "file", &entity.id, &stage_name)
            .await?;
        self.model.rename(&entity.id, &stage_name)?;
        self.store
            .update_operation(&operation_id, json!({ "status": "staged" }))?;

        let uploaded = match self
            .http
            .upload_file(&self.config.project_id, &parent_id, &entity.name, bytes)
            .await
        {
            Ok(uploaded) => uploaded,
            Err(error) => {
                let rollback = self
                    .http
                    .rename_entity(&self.config.project_id, "file", &entity.id, &entity.name)
                    .await;
                if rollback.is_ok() {
                    self.model.rename(&entity.id, &entity.name)?;
                    self.store.complete_operation(&operation_id)?;
                }
                return Err(error.into());
            }
        };
        let replacement = self.model.insert(&parent_id, EntityKind::File, uploaded)?;
        self.store.update_operation(
            &operation_id,
            json!({
                "status": "uploaded",
                "newId": replacement.id,
                "replacementId": replacement.id
            }),
        )?;
        let authoritative = self
            .http
            .download_file(&self.config.project_id, &replacement.id)
            .await?;
        let authoritative_digest = digest_bytes(&authoritative);
        if authoritative_digest != desired_digest {
            let _ = self
                .http
                .delete_entity(&self.config.project_id, "file", &replacement.id)
                .await;
            self.model.remove(&replacement.id);
            let rollback = self
                .http
                .rename_entity(&self.config.project_id, "file", &entity.id, &entity.name)
                .await;
            if rollback.is_ok() {
                self.model.rename(&entity.id, &entity.name)?;
                self.store.complete_operation(&operation_id)?;
            }
            return Err(NativeSyncError::WriteNotProven(entity.path.clone()));
        }
        self.store
            .update_operation(&operation_id, json!({ "status": "verified" }))?;
        self.http
            .delete_entity(&self.config.project_id, "file", &entity.id)
            .await?;
        self.model.remove(&entity.id);
        self.store.remove_file(&entity.id);
        self.store.complete_operation(&operation_id)?;
        Ok((replacement, authoritative_digest))
    }

    fn record_binary_conflict(
        &mut self,
        entity: &ProjectEntity,
        reason: &str,
        base: Option<&[u8]>,
        local: &[u8],
        remote: &[u8],
    ) -> Result<(), NativeSyncError> {
        self.store.record_conflict(
            &entity.id,
            &entity.path,
            "binary",
            reason,
            base,
            Some(local),
            Some(remote),
        )?;
        Ok(())
    }

    fn bootstrap_document(
        &mut self,
        entity: &ProjectEntity,
        snapshot: DocumentSnapshot,
        adopt_local_on_first_sync: bool,
    ) -> Result<NativeDocument, NativeSyncError> {
        let path = self.root.join(&entity.path);
        let stored = self.store.document(&entity.id).cloned();
        let remote_text = snapshot.text.clone();
        let disk_bytes = read_optional_bytes(&path)?;
        let (disk_text, invalid_disk) = match disk_bytes {
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(text) => (Some(text), None),
                Err(error) => (None, Some(error.into_bytes())),
            },
            None => (None, None),
        };
        if let Some(invalid_disk) = invalid_disk {
            if !self.path_is_conflicted(&entity.path) {
                self.store.record_conflict(
                    &entity.id,
                    &entity.path,
                    "text",
                    "document-not-utf8",
                    stored.as_ref().map(|record| record.base_text.as_bytes()),
                    Some(&invalid_disk),
                    Some(remote_text.as_bytes()),
                )?;
            }
            return Ok(NativeDocument {
                id: entity.id.clone(),
                path: entity.path.clone(),
                version: snapshot.version,
                ot_type: snapshot.ot_type,
                state: "conflicted".into(),
                base_text: stored
                    .as_ref()
                    .map(|record| record.base_text.clone())
                    .unwrap_or_else(|| remote_text.clone()),
                local_text: stored
                    .as_ref()
                    .map(|record| record.local_text.clone())
                    .unwrap_or_else(|| remote_text.clone()),
                remote_text,
                raw_snapshot: snapshot.raw_snapshot,
                ranges: snapshot.ranges,
                read_only_reason: snapshot.read_only_reason,
                pending: false,
            });
        }
        if self.path_is_conflicted(&entity.path) {
            let local_text = disk_text
                .or_else(|| stored.as_ref().map(|record| record.local_text.clone()))
                .unwrap_or_else(|| remote_text.clone());
            return Ok(NativeDocument {
                id: entity.id.clone(),
                path: entity.path.clone(),
                version: snapshot.version,
                ot_type: snapshot.ot_type,
                state: "conflicted".into(),
                base_text: stored
                    .as_ref()
                    .map(|record| record.base_text.clone())
                    .unwrap_or_else(|| remote_text.clone()),
                local_text,
                remote_text,
                raw_snapshot: snapshot.raw_snapshot,
                ranges: snapshot.ranges,
                read_only_reason: snapshot.read_only_reason,
                pending: false,
            });
        }
        let (base_text, local_text, state) = if let Some(stored) = stored {
            let local = disk_text.unwrap_or(stored.local_text);
            match merge_text(&stored.base_text, &local, &remote_text) {
                MergeResult::Merged(merged) => {
                    let state = if merged == remote_text {
                        "ready"
                    } else {
                        "ready-dirty"
                    };
                    (remote_text.clone(), merged, state.into())
                }
                MergeResult::Conflict { reason } => {
                    self.store.record_conflict(
                        &entity.id,
                        &entity.path,
                        "text",
                        &format!("reconnect-{reason}"),
                        Some(stored.base_text.as_bytes()),
                        Some(local.as_bytes()),
                        Some(remote_text.as_bytes()),
                    )?;
                    (remote_text.clone(), local, "conflicted".into())
                }
            }
        } else if let Some(local) = disk_text {
            if local == remote_text {
                (remote_text.clone(), local, "ready".into())
            } else if adopt_local_on_first_sync {
                (remote_text.clone(), local, "ready-dirty".into())
            } else {
                self.store.record_conflict(
                    &entity.id,
                    &entity.path,
                    "text",
                    "unbased-local-file",
                    Some(remote_text.as_bytes()),
                    Some(local.as_bytes()),
                    Some(remote_text.as_bytes()),
                )?;
                (remote_text.clone(), local, "conflicted".into())
            }
        } else {
            (remote_text.clone(), remote_text.clone(), "ready".into())
        };
        if state != "conflicted" {
            atomic_write(&path, local_text.as_bytes(), 0o644)?;
        }
        Ok(NativeDocument {
            id: entity.id.clone(),
            path: entity.path.clone(),
            version: snapshot.version,
            ot_type: snapshot.ot_type,
            state,
            base_text,
            local_text,
            remote_text,
            raw_snapshot: snapshot.raw_snapshot,
            ranges: snapshot.ranges,
            read_only_reason: snapshot.read_only_reason,
            pending: false,
        })
    }

    async fn apply_snapshot(
        &mut self,
        path: &str,
        text: String,
        origin: &str,
    ) -> Result<NativeDocumentSnapshot, NativeSyncError> {
        let id = self
            .model
            .get_by_path(path)?
            .filter(|entity| entity.kind == EntityKind::Document)
            .map(|entity| entity.id.clone())
            .ok_or_else(|| NativeSyncError::UnknownDocument(path.into()))?;
        {
            let document = self
                .documents
                .get_mut(&id)
                .ok_or_else(|| NativeSyncError::UnknownDocument(path.into()))?;
            if document.state == "conflicted" {
                return Err(NativeSyncError::Conflicted(path.into()));
            }
            if let Some(reason) = &document.read_only_reason {
                return Err(NativeSyncError::ReadOnly(reason.clone()));
            }
            document.local_text = text;
            document.state = if document.local_text == document.remote_text {
                "ready".into()
            } else {
                "ready-dirty".into()
            };
            atomic_write(
                &self.root.join(&document.path),
                document.local_text.as_bytes(),
                0o644,
            )?;
            self.store.upsert_document(document.record())?;
            self.store.persist()?;
        }
        self.flush_document(&id, origin).await?;
        let document = self
            .documents
            .get(&id)
            .ok_or_else(|| NativeSyncError::UnknownDocument(path.into()))?;
        let snapshot = document.snapshot();
        let _ = self.events.send(NativeSyncEvent::DocumentChanged {
            path: document.path.clone(),
            origin: origin.into(),
            text: document.local_text.clone(),
        });
        self.emit_status();
        Ok(snapshot)
    }

    async fn flush_document(&mut self, id: &str, _reason: &str) -> Result<(), NativeSyncError> {
        let mut document = self
            .documents
            .remove(id)
            .ok_or_else(|| NativeSyncError::UnknownDocument(id.into()))?;
        let result = self.flush_removed_document(&mut document).await;
        self.store.upsert_document(document.record())?;
        self.store.persist()?;
        self.documents.insert(id.into(), document);
        result
    }

    async fn flush_removed_document(
        &mut self,
        document: &mut NativeDocument,
    ) -> Result<(), NativeSyncError> {
        if document.state == "conflicted" {
            return Err(NativeSyncError::Conflicted(document.path.clone()));
        }
        if let Some(reason) = &document.read_only_reason {
            return Err(NativeSyncError::ReadOnly(reason.clone()));
        }
        let desired = match merge_text(
            &document.base_text,
            &document.local_text,
            &document.remote_text,
        ) {
            MergeResult::Merged(text) => text,
            MergeResult::Conflict { reason } => {
                self.record_document_conflict(document, &format!("pre-submit-{reason}"))?;
                return Err(NativeSyncError::Conflicted(document.path.clone()));
            }
        };
        if desired == document.remote_text {
            document.base_text = desired.clone();
            document.local_text = desired;
            document.state = "ready".into();
            return Ok(());
        }
        let submitted_remote = document.remote_text.clone();
        document.pending = true;
        match document.ot_type.as_str() {
            "sharejs-text-ot" => {
                let operations = diff_to_sharejs_operations(&submitted_remote, &desired);
                self.realtime
                    .apply_sharejs_update(&document.id, document.version, &operations)
                    .await?;
            }
            "history-ot" => {
                let operations = diff_to_history_operations(&submitted_remote, &desired);
                self.realtime
                    .apply_history_update(&document.id, document.version, &operations)
                    .await?;
            }
            unsupported => {
                return Err(RealtimeError::UnsupportedOtType(unsupported.into()).into());
            }
        }
        let authoritative = self
            .confirm_document_write(&document.id, &submitted_remote, &desired)
            .await?;
        document.pending = false;
        self.reconcile_authoritative(document, authoritative, "local-confirmed")?;
        Ok(())
    }

    async fn confirm_document_write(
        &mut self,
        id: &str,
        submitted_remote: &str,
        desired: &str,
    ) -> Result<DocumentSnapshot, NativeSyncError> {
        for delay in CONFIRM_DELAYS {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            let snapshot = self.realtime.join_document(id, None, None).await?;
            if desired_change_is_present(submitted_remote, desired, &snapshot.text) {
                return Ok(snapshot);
            }
        }
        Err(NativeSyncError::WriteNotProven(id.into()))
    }

    fn reconcile_authoritative(
        &mut self,
        document: &mut NativeDocument,
        snapshot: DocumentSnapshot,
        origin: &str,
    ) -> Result<(), NativeSyncError> {
        let remote_text = snapshot.text;
        let previous_local = document.local_text.clone();
        let merged = match merge_text(&document.base_text, &previous_local, &remote_text) {
            MergeResult::Merged(text) => text,
            MergeResult::Conflict { reason } => {
                self.record_document_conflict(document, &format!("{origin}-{reason}"))?;
                return Err(NativeSyncError::Conflicted(document.path.clone()));
            }
        };
        let disk = read_optional_text(&self.root.join(&document.path))?;
        let materialized = match disk {
            Some(current) if current != previous_local => {
                match merge_text(&previous_local, &merged, &current) {
                    MergeResult::Merged(text) => text,
                    MergeResult::Conflict { reason } => {
                        self.store.record_conflict(
                            &document.id,
                            &document.path,
                            "text",
                            &format!("concurrent-materialization-{reason}"),
                            Some(previous_local.as_bytes()),
                            Some(current.as_bytes()),
                            Some(merged.as_bytes()),
                        )?;
                        document.state = "conflicted".into();
                        return Err(NativeSyncError::Conflicted(document.path.clone()));
                    }
                }
            }
            _ => merged,
        };
        atomic_write(
            &self.root.join(&document.path),
            materialized.as_bytes(),
            0o644,
        )?;
        document.version = snapshot.version;
        document.ot_type = snapshot.ot_type;
        document.raw_snapshot = snapshot.raw_snapshot;
        document.ranges = snapshot.ranges;
        document.read_only_reason = snapshot.read_only_reason;
        document.remote_text = remote_text.clone();
        document.base_text = remote_text;
        document.local_text = materialized;
        document.pending = false;
        document.state = if document.local_text == document.remote_text {
            "ready".into()
        } else {
            "ready-dirty".into()
        };
        let _ = self.events.send(NativeSyncEvent::DocumentChanged {
            path: document.path.clone(),
            origin: origin.into(),
            text: document.local_text.clone(),
        });
        Ok(())
    }

    async fn refresh_document(&mut self, id: &str, origin: &str) -> Result<(), NativeSyncError> {
        let snapshot = self.realtime.join_document(id, None, None).await?;
        let mut document = self
            .documents
            .remove(id)
            .ok_or_else(|| NativeSyncError::UnknownDocument(id.into()))?;
        let result = self.reconcile_authoritative(&mut document, snapshot, origin);
        let needs_flush = result.is_ok()
            && document.state != "conflicted"
            && document.local_text != document.remote_text;
        self.store.upsert_document(document.record())?;
        self.store.persist()?;
        self.documents.insert(id.into(), document);
        result?;
        if needs_flush {
            self.flush_document(id, "remote-rebase").await?;
        }
        Ok(())
    }

    async fn refresh_all_documents(&mut self, origin: &str) -> Result<(), NativeSyncError> {
        let ids = self.documents.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            if self.documents[&id].state != "conflicted" {
                self.refresh_document(&id, origin).await?;
            }
        }
        Ok(())
    }

    async fn scan_documents(&mut self, origin: &str) -> Result<(), NativeSyncError> {
        let snapshot = scan_tree(&self.root)?;
        let mut documents = Vec::new();
        let mut invalid_documents = Vec::new();
        for (id, document) in &self.documents {
            let Some(file) = snapshot.files.get(&document.path) else {
                continue;
            };
            match String::from_utf8(file.bytes.clone()) {
                Ok(text) if text != document.local_text => documents.push((id.clone(), text)),
                Ok(_) => {}
                Err(_) => invalid_documents.push((
                    id.clone(),
                    document.path.clone(),
                    file.bytes.clone(),
                    document.remote_text.clone(),
                )),
            }
        }
        for (id, path, local, remote) in invalid_documents {
            if !self.path_is_conflicted(&path) {
                self.store.record_conflict(
                    &id,
                    &path,
                    "text",
                    "document-not-utf8",
                    None,
                    Some(&local),
                    Some(remote.as_bytes()),
                )?;
            }
            if let Some(document) = self.documents.get_mut(&id) {
                document.state = "conflicted".into();
                document.pending = false;
            }
        }
        for (id, text) in documents {
            let path = self.documents[&id].path.clone();
            self.apply_snapshot(&path, text, origin).await?;
        }
        self.sync_local_directories(&snapshot).await?;
        self.infer_local_renames(&snapshot).await?;
        self.sync_existing_binary_files(&snapshot).await?;
        self.sync_new_local_files(&snapshot).await?;
        self.sync_local_deletions(&snapshot).await?;
        let dirty = self
            .documents
            .values()
            .filter(|document| {
                document.state != "conflicted" && document.local_text != document.remote_text
            })
            .map(|document| (document.id.clone(), document.pending))
            .collect::<Vec<_>>();
        for (id, pending) in dirty {
            if pending {
                self.refresh_document(&id, "pending-recovery").await?;
            } else {
                self.flush_document(&id, origin).await?;
            }
        }
        self.persist_status()?;
        Ok(())
    }

    async fn sync_local_directories(
        &mut self,
        snapshot: &TreeSnapshot,
    ) -> Result<(), NativeSyncError> {
        let mut directories = snapshot.directories.iter().cloned().collect::<Vec<_>>();
        directories.sort_by_key(|path| path.matches('/').count());
        for path in directories {
            if path.is_empty()
                || self.path_is_conflicted(&path)
                || self.model.get_by_path(&path)?.is_some()
            {
                continue;
            }
            let (parent_path, name) = split_parent(&path);
            let parent = self
                .model
                .get_by_path(parent_path)?
                .filter(|entity| entity.kind == EntityKind::Folder)
                .cloned()
                .ok_or_else(|| {
                    NativeSyncError::InvalidConfig(format!(
                        "missing Overleaf parent for local directory {path}"
                    ))
                })?;
            let raw = self
                .http
                .add_folder(&self.config.project_id, &parent.id, name)
                .await?;
            let entity = self.model.insert(&parent.id, EntityKind::Folder, raw)?;
            self.store
                .upsert_directory(entity_record(&entity.id, &entity.path, None, "folder"))?;
        }
        self.store.persist()?;
        Ok(())
    }

    async fn infer_local_renames(
        &mut self,
        snapshot: &TreeSnapshot,
    ) -> Result<(), NativeSyncError> {
        let mut new_by_digest = HashMap::<String, Vec<String>>::new();
        for file in snapshot.files.values() {
            if !self.path_is_conflicted(&file.path) && self.model.get_by_path(&file.path)?.is_none()
            {
                new_by_digest
                    .entry(file.digest.clone())
                    .or_default()
                    .push(file.path.clone());
            }
        }
        let mut missing_by_digest = HashMap::<String, Vec<ProjectEntity>>::new();
        for entity in self.model.documents().chain(self.model.files()).cloned() {
            if snapshot.files.contains_key(&entity.path) || self.path_is_conflicted(&entity.path) {
                continue;
            }
            let digest = match entity.kind {
                EntityKind::Document => self
                    .documents
                    .get(&entity.id)
                    .map(|document| digest_bytes(document.local_text.as_bytes())),
                EntityKind::File => self.store.state().files.get(&entity.id).and_then(|record| {
                    record
                        .local_digest
                        .clone()
                        .or_else(|| record.digest.clone())
                }),
                EntityKind::Folder => None,
            };
            if let Some(digest) = digest {
                missing_by_digest.entry(digest).or_default().push(entity);
            }
        }
        let candidates = missing_by_digest
            .into_iter()
            .filter_map(|(digest, entities)| {
                let paths = new_by_digest.get(&digest)?;
                (entities.len() == 1 && paths.len() == 1)
                    .then(|| (entities[0].clone(), paths[0].clone()))
            })
            .collect::<Vec<_>>();
        for (entity, target_path) in candidates {
            if entity.kind == EntityKind::Document && !is_text_document_path(&target_path) {
                continue;
            }
            self.rename_remote_entity(&entity, &target_path).await?;
        }
        Ok(())
    }

    async fn rename_remote_entity(
        &mut self,
        entity: &ProjectEntity,
        target_path: &str,
    ) -> Result<(), NativeSyncError> {
        let (parent_path, name) = split_parent(target_path);
        let parent = self
            .model
            .get_by_path(parent_path)?
            .filter(|entity| entity.kind == EntityKind::Folder)
            .cloned()
            .ok_or_else(|| {
                NativeSyncError::InvalidConfig(format!(
                    "missing Overleaf parent for renamed path {target_path}"
                ))
            })?;
        if entity.name != name {
            self.http
                .rename_entity(
                    &self.config.project_id,
                    entity.kind.route_name(),
                    &entity.id,
                    name,
                )
                .await?;
            self.model.rename(&entity.id, name)?;
        }
        let current_parent = self
            .model
            .get_by_id(&entity.id)
            .and_then(|entity| entity.parent_id.as_deref());
        if current_parent != Some(parent.id.as_str()) {
            self.http
                .move_entity(
                    &self.config.project_id,
                    entity.kind.route_name(),
                    &entity.id,
                    &parent.id,
                )
                .await?;
            self.model.move_to(&entity.id, &parent.id)?;
        }
        let current = self.model.get_by_id(&entity.id).cloned().ok_or_else(|| {
            NativeSyncError::InvalidConfig(format!(
                "renamed Overleaf entity disappeared: {}",
                entity.id
            ))
        })?;
        match current.kind {
            EntityKind::Document => {
                if let Some(document) = self.documents.get_mut(&current.id) {
                    document.path = current.path.clone();
                    self.store.upsert_document(document.record())?;
                }
            }
            EntityKind::File => {
                if let Some(mut record) = self.store.state().files.get(&current.id).cloned() {
                    record.path = current.path.clone();
                    self.store.upsert_file(record)?;
                }
            }
            EntityKind::Folder => {}
        }
        self.missing_counts
            .remove(&format!("{}:{}", current.kind.route_name(), current.id));
        self.store.persist()?;
        Ok(())
    }

    async fn sync_existing_binary_files(
        &mut self,
        snapshot: &TreeSnapshot,
    ) -> Result<(), NativeSyncError> {
        let files = self.model.files().cloned().collect::<Vec<_>>();
        for entity in files {
            let Some(local) = snapshot.files.get(&entity.path) else {
                continue;
            };
            self.missing_counts.remove(&format!("file:{}", entity.id));
            if self.path_is_conflicted(&entity.path) {
                continue;
            }
            let stored_digest = self
                .store
                .state()
                .files
                .get(&entity.id)
                .and_then(|record| record.local_digest.as_deref().or(record.digest.as_deref()));
            if stored_digest == Some(local.digest.as_str()) {
                continue;
            }
            if !self.config.allow_binary_writes {
                let remote = self
                    .http
                    .download_file(&self.config.project_id, &entity.id)
                    .await?;
                self.record_binary_conflict(
                    &entity,
                    "binary-writes-disabled",
                    None,
                    &local.bytes,
                    &remote,
                )?;
                continue;
            }
            let (replacement, digest) = self
                .replace_remote_binary(&entity, local.bytes.clone())
                .await?;
            self.store.upsert_file(entity_record(
                &replacement.id,
                &replacement.path,
                Some(digest),
                "file",
            ))?;
        }
        self.store.persist()?;
        Ok(())
    }

    async fn sync_new_local_files(
        &mut self,
        snapshot: &TreeSnapshot,
    ) -> Result<(), NativeSyncError> {
        let new_files = snapshot
            .files
            .values()
            .filter_map(|file| {
                self.model
                    .get_by_path(&file.path)
                    .ok()
                    .flatten()
                    .is_none()
                    .then(|| file.clone())
            })
            .collect::<Vec<_>>();
        for file in new_files {
            if self.path_is_conflicted(&file.path) {
                continue;
            }
            let (parent_path, name) = split_parent(&file.path);
            let parent = self
                .model
                .get_by_path(parent_path)?
                .filter(|entity| entity.kind == EntityKind::Folder)
                .cloned()
                .ok_or_else(|| {
                    NativeSyncError::InvalidConfig(format!(
                        "missing Overleaf parent for local file {}",
                        file.path
                    ))
                })?;
            if is_text_document_path(&file.path) {
                let text = String::from_utf8(file.bytes).map_err(|error| {
                    NativeSyncError::InvalidConfig(format!(
                        "text document {} is not UTF-8: {error}",
                        file.path
                    ))
                })?;
                let raw = self
                    .http
                    .add_document(&self.config.project_id, &parent.id, name)
                    .await?;
                let entity = self.model.insert(&parent.id, EntityKind::Document, raw)?;
                let remote = self.realtime.join_document(&entity.id, None, None).await?;
                let document = NativeDocument {
                    id: entity.id.clone(),
                    path: entity.path.clone(),
                    version: remote.version,
                    ot_type: remote.ot_type,
                    state: "ready-dirty".into(),
                    base_text: remote.text.clone(),
                    local_text: text,
                    remote_text: remote.text,
                    raw_snapshot: remote.raw_snapshot,
                    ranges: remote.ranges,
                    read_only_reason: remote.read_only_reason,
                    pending: false,
                };
                self.store.upsert_document(document.record())?;
                self.documents.insert(entity.id.clone(), document);
                self.flush_document(&entity.id, "local-create").await?;
            } else {
                let raw = self
                    .http
                    .upload_file(
                        &self.config.project_id,
                        &parent.id,
                        name,
                        file.bytes.clone(),
                    )
                    .await?;
                let entity = self.model.insert(&parent.id, EntityKind::File, raw)?;
                let authoritative = self
                    .http
                    .download_file(&self.config.project_id, &entity.id)
                    .await?;
                let digest = digest_bytes(&authoritative);
                if digest != file.digest {
                    return Err(NativeSyncError::WriteNotProven(file.path));
                }
                self.store.upsert_file(entity_record(
                    &entity.id,
                    &entity.path,
                    Some(digest),
                    "file",
                ))?;
            }
        }
        self.store.persist()?;
        Ok(())
    }

    async fn sync_local_deletions(
        &mut self,
        snapshot: &TreeSnapshot,
    ) -> Result<(), NativeSyncError> {
        let entities = self
            .model
            .documents()
            .chain(self.model.files())
            .cloned()
            .collect::<Vec<_>>();
        for entity in entities {
            let key = format!("{}:{}", entity.kind.route_name(), entity.id);
            if snapshot.files.contains_key(&entity.path) {
                self.missing_counts.remove(&key);
                continue;
            }
            if self.path_is_conflicted(&entity.path) || !self.missing_ready(&key) {
                continue;
            }
            let safe = match entity.kind {
                EntityKind::Document => self
                    .documents
                    .get(&entity.id)
                    .is_some_and(NativeDocument::clean),
                EntityKind::File => self
                    .store
                    .state()
                    .files
                    .get(&entity.id)
                    .is_some_and(|file| {
                        file.local_digest == file.remote_digest
                            || (file.local_digest.is_none() && file.digest.is_some())
                    }),
                EntityKind::Folder => false,
            };
            if !safe {
                continue;
            }
            self.http
                .delete_entity(
                    &self.config.project_id,
                    entity.kind.route_name(),
                    &entity.id,
                )
                .await?;
            if entity.kind == EntityKind::Document {
                let _ = self.realtime.leave_document(&entity.id).await;
                self.documents.remove(&entity.id);
                self.store.remove_document(&entity.id);
            } else {
                self.store.remove_file(&entity.id);
            }
            self.model.remove(&entity.id);
            self.missing_counts.remove(&key);
        }

        let mut folders = self.model.folders().cloned().collect::<Vec<_>>();
        folders.sort_by_key(|entity| std::cmp::Reverse(entity.path.matches('/').count()));
        for entity in folders {
            if entity.path.is_empty() {
                continue;
            }
            let key = format!("folder:{}", entity.id);
            if snapshot.directories.contains(&entity.path) {
                self.missing_counts.remove(&key);
                continue;
            }
            let prefix = format!("{}/", entity.path);
            if self
                .model
                .all()
                .any(|candidate| candidate.id != entity.id && candidate.path.starts_with(&prefix))
                || !self.missing_ready(&key)
            {
                continue;
            }
            self.http
                .delete_entity(&self.config.project_id, "folder", &entity.id)
                .await?;
            self.model.remove(&entity.id);
            self.store.remove_directory(&entity.id);
            self.missing_counts.remove(&key);
        }
        self.store.persist()?;
        Ok(())
    }

    fn missing_ready(&mut self, key: &str) -> bool {
        let count = self.missing_counts.entry(key.into()).or_default();
        *count = count.saturating_add(1);
        *count >= 2
    }

    fn path_is_conflicted(&self, path: &str) -> bool {
        self.store.unresolved_conflicts().any(|conflict| {
            conflict.path == path
                || conflict.path.starts_with(&format!("{path}/"))
                || path.starts_with(&format!("{}/", conflict.path))
        })
    }

    fn is_managed_stage_entity(&self, entity: &ProjectEntity) -> bool {
        if entity.kind != EntityKind::File {
            return false;
        }
        if entity.name.starts_with(".semantic-zed-stage-") {
            return true;
        }
        self.store.state().operations.values().any(|operation| {
            operation_string(operation, "kind") == Some("binary-replace")
                && operation_string(operation, "oldId") == Some(entity.id.as_str())
        })
    }

    async fn compile(&mut self, root_resource_path: Option<&str>) -> Result<(), NativeSyncError> {
        self.scan_documents("compile-scan").await?;
        if self.store.unresolved_conflicts().next().is_some() {
            return Err(NativeSyncError::CompileBlocked);
        }
        let dirty = self
            .documents
            .values()
            .filter(|document| document.local_text != document.remote_text)
            .map(|document| document.id.clone())
            .collect::<Vec<_>>();
        for id in dirty {
            self.flush_document(&id, "compile-barrier").await?;
        }
        if self.documents.values().any(|document| !document.clean()) {
            return Err(NativeSyncError::CompileBlocked);
        }
        let compile = self
            .http
            .compile(&self.config.project_id, root_resource_path)
            .await?;
        let pdf = self
            .http
            .download_compile_output(&compile, "output.pdf")
            .await?;
        let output = self.root.join(".semantic-zed/output");
        fs::create_dir_all(&output)?;
        let pdf_path = output.join("output.pdf");
        atomic_write(&pdf_path, &pdf, 0o600)?;
        let compile_bytes = serde_json::to_vec_pretty(&compile)?;
        atomic_write(&output.join("compile.json"), &compile_bytes, 0o600)?;
        self.last_compile = Some(compile.clone());
        let _ = self.events.send(NativeSyncEvent::PdfChanged {
            path: pdf_path,
            compile,
        });
        self.persist_status()?;
        Ok(())
    }

    async fn update_position(
        &mut self,
        path: &str,
        row: u32,
        column: u32,
    ) -> Result<(), NativeSyncError> {
        let entity = self
            .model
            .get_by_path(path)?
            .filter(|entity| entity.kind == EntityKind::Document)
            .ok_or_else(|| NativeSyncError::UnknownDocument(path.into()))?;
        self.realtime
            .update_position(json!({
                "row": row,
                "column": column,
                "doc_id": entity.id
            }))
            .await?;
        Ok(())
    }

    async fn handle_remote_create(
        &mut self,
        parent_id: &str,
        kind: EntityKind,
        raw: Value,
    ) -> Result<(), NativeSyncError> {
        let raw_id = raw
            .get("_id")
            .or_else(|| raw.get("id"))
            .and_then(Value::as_str);
        if raw_id.is_some_and(|id| self.model.get_by_id(id).is_some()) {
            return Ok(());
        }
        let entity = self.model.insert(parent_id, kind, raw)?;
        match kind {
            EntityKind::Folder => {
                fs::create_dir_all(self.root.join(&entity.path))?;
                self.store.upsert_directory(entity_record(
                    &entity.id,
                    &entity.path,
                    None,
                    "folder",
                ))?;
            }
            EntityKind::Document => {
                let snapshot = self.realtime.join_document(&entity.id, None, None).await?;
                let document = self.bootstrap_document(&entity, snapshot, false)?;
                self.store.upsert_document(document.record())?;
                self.documents.insert(entity.id.clone(), document);
            }
            EntityKind::File => {
                if !self.is_managed_stage_entity(&entity) {
                    self.bootstrap_remote_file(&entity).await?;
                }
            }
        }
        self.store.persist()?;
        self.emit_status();
        Ok(())
    }

    async fn bootstrap_remote_file(
        &mut self,
        entity: &ProjectEntity,
    ) -> Result<(), NativeSyncError> {
        let remote = self
            .http
            .download_file(&self.config.project_id, &entity.id)
            .await?;
        let remote_digest = digest_bytes(&remote);
        let local_path = self.root.join(&entity.path);
        let local = read_optional_bytes(&local_path)?;
        match local {
            None => {
                atomic_write(&local_path, &remote, 0o644)?;
                self.store.upsert_file(entity_record(
                    &entity.id,
                    &entity.path,
                    Some(remote_digest),
                    "file",
                ))?;
            }
            Some(local) if digest_bytes(&local) == remote_digest => {
                self.store.upsert_file(entity_record(
                    &entity.id,
                    &entity.path,
                    Some(remote_digest),
                    "file",
                ))?;
            }
            Some(local) => {
                self.record_binary_conflict(
                    entity,
                    "remote-create-local-path-exists",
                    None,
                    &local,
                    &remote,
                )?;
            }
        }
        Ok(())
    }

    fn handle_remote_repath(
        &mut self,
        entity_id: &str,
        rename: Option<&str>,
        parent_id: Option<&str>,
    ) -> Result<(), NativeSyncError> {
        let Some(current) = self.model.get_by_id(entity_id).cloned() else {
            return Ok(());
        };
        let result = if let Some(name) = rename {
            if current.name == name {
                return Ok(());
            }
            self.model.rename(entity_id, name)?
        } else if let Some(parent_id) = parent_id {
            if current.parent_id.as_deref() == Some(parent_id) {
                return Ok(());
            }
            self.model.move_to(entity_id, parent_id)?
        } else {
            None
        };
        let Some((old_path, new_path)) = result else {
            return Ok(());
        };
        if old_path != new_path {
            self.move_materialized_path(&old_path, &new_path)?;
        }
        self.update_stored_subtree_paths(entity_id)?;
        self.store.persist()?;
        self.emit_status();
        Ok(())
    }

    fn update_stored_subtree_paths(&mut self, entity_id: &str) -> Result<(), NativeSyncError> {
        let Some(root) = self.model.get_by_id(entity_id).cloned() else {
            return Ok(());
        };
        let prefix = format!("{}/", root.path);
        let entities = self
            .model
            .all()
            .filter(|entity| entity.id == root.id || entity.path.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        for entity in entities {
            match entity.kind {
                EntityKind::Document => {
                    if let Some(document) = self.documents.get_mut(&entity.id) {
                        document.path = entity.path.clone();
                        self.store.upsert_document(document.record())?;
                    } else if let Some(mut record) = self.store.document(&entity.id).cloned() {
                        record.path = entity.path.clone();
                        self.store.upsert_document(record)?;
                    }
                }
                EntityKind::File => {
                    let mut record = self
                        .store
                        .state()
                        .files
                        .get(&entity.id)
                        .cloned()
                        .unwrap_or_else(|| entity_record(&entity.id, &entity.path, None, "file"));
                    record.path = entity.path.clone();
                    self.store.upsert_file(record)?;
                }
                EntityKind::Folder => {
                    let mut record = self
                        .store
                        .state()
                        .directories
                        .get(&entity.id)
                        .cloned()
                        .unwrap_or_else(|| entity_record(&entity.id, &entity.path, None, "folder"));
                    record.path = entity.path.clone();
                    self.store.upsert_directory(record)?;
                }
            }
        }
        Ok(())
    }

    async fn handle_remote_remove(&mut self, entity_id: &str) -> Result<(), NativeSyncError> {
        let mut removed = self.model.remove(entity_id);
        removed.sort_by_key(|entity| std::cmp::Reverse(entity.path.matches('/').count()));
        for entity in removed {
            let local_path = self.root.join(&entity.path);
            let symlink = fs::symlink_metadata(&local_path)
                .ok()
                .is_some_and(|metadata| metadata.file_type().is_symlink());
            match entity.kind {
                EntityKind::Document => {
                    let document = self.documents.remove(&entity.id);
                    let stored = self.store.document(&entity.id).cloned();
                    let local = if symlink {
                        None
                    } else {
                        read_optional_bytes(&local_path)?
                    };
                    let expected = document
                        .as_ref()
                        .map(|document| document.local_text.as_bytes().to_vec())
                        .or_else(|| {
                            stored
                                .as_ref()
                                .map(|document| document.local_text.as_bytes().to_vec())
                        });
                    let dirty = document.as_ref().is_some_and(|document| !document.clean());
                    let safe = !symlink
                        && !dirty
                        && match (&local, &expected) {
                            (None, _) => true,
                            (Some(local), Some(expected)) => local == expected,
                            _ => false,
                        };
                    if safe {
                        if local.is_some() {
                            fs::remove_file(&local_path)?;
                        }
                        let _ = self.realtime.leave_document(&entity.id).await;
                        self.store.remove_document(&entity.id);
                    } else if !self.path_is_conflicted(&entity.path) {
                        self.store.record_conflict(
                            &entity.id,
                            &entity.path,
                            "text",
                            if symlink {
                                "remote-delete-local-symlink"
                            } else {
                                "remote-delete-local-edit"
                            },
                            stored.as_ref().map(|record| record.base_text.as_bytes()),
                            local.as_deref().or(expected.as_deref()),
                            Some(&[]),
                        )?;
                    }
                }
                EntityKind::File => {
                    let local = if symlink {
                        None
                    } else {
                        read_optional_bytes(&local_path)?
                    };
                    let expected_digest =
                        self.store.state().files.get(&entity.id).and_then(|record| {
                            record.local_digest.as_deref().or(record.digest.as_deref())
                        });
                    let safe = !symlink
                        && local.as_ref().is_none_or(|bytes| {
                            expected_digest.is_some_and(|digest| digest_bytes(bytes) == digest)
                        });
                    if safe {
                        if local.is_some() {
                            fs::remove_file(&local_path)?;
                        }
                        self.store.remove_file(&entity.id);
                    } else if !self.path_is_conflicted(&entity.path) {
                        self.store.record_conflict(
                            &entity.id,
                            &entity.path,
                            "binary",
                            if symlink {
                                "remote-delete-local-symlink"
                            } else {
                                "remote-delete-local-edit"
                            },
                            None,
                            local.as_deref(),
                            Some(&[]),
                        )?;
                    }
                }
                EntityKind::Folder => {
                    let exists = fs::symlink_metadata(&local_path).is_ok();
                    let empty = !exists
                        || (!symlink
                            && local_path.is_dir()
                            && fs::read_dir(&local_path)?.next().is_none());
                    if empty {
                        if exists {
                            fs::remove_dir(&local_path)?;
                        }
                        self.store.remove_directory(&entity.id);
                    } else if !self.path_is_conflicted(&entity.path) {
                        self.store.record_conflict(
                            &entity.id,
                            &entity.path,
                            "directory",
                            if symlink {
                                "remote-delete-local-symlink"
                            } else {
                                "remote-delete-local-directory-not-empty"
                            },
                            None,
                            None,
                            Some(&[]),
                        )?;
                    }
                }
            }
        }
        self.store.persist()?;
        self.emit_status();
        Ok(())
    }

    async fn handle_realtime_event(&mut self, event: RealtimeEvent) -> Result<(), NativeSyncError> {
        match event.name.as_str() {
            "otUpdateApplied" => {
                if let Some(id) = event.args.first().and_then(|update| {
                    update
                        .get("doc")
                        .or_else(|| update.get("doc_id"))
                        .and_then(Value::as_str)
                }) && self.documents.contains_key(id)
                {
                    self.refresh_document(id, "remote-operation").await?;
                }
            }
            "reciveNewDoc" | "reciveNewFile" | "reciveNewFolder" => {
                if let (Some(parent_id), Some(raw)) = (
                    event.args.first().and_then(Value::as_str),
                    event.args.get(1).cloned(),
                ) {
                    let kind = match event.name.as_str() {
                        "reciveNewDoc" => EntityKind::Document,
                        "reciveNewFile" => EntityKind::File,
                        _ => EntityKind::Folder,
                    };
                    self.handle_remote_create(parent_id, kind, raw).await?;
                }
            }
            "reciveEntityRename" => {
                if let (Some(id), Some(name)) = (
                    event.args.first().and_then(Value::as_str),
                    event.args.get(1).and_then(Value::as_str),
                ) {
                    self.handle_remote_repath(id, Some(name), None)?;
                }
            }
            "reciveEntityMove" => {
                if let (Some(id), Some(parent_id)) = (
                    event.args.first().and_then(Value::as_str),
                    event.args.get(1).and_then(Value::as_str),
                ) {
                    self.handle_remote_repath(id, None, Some(parent_id))?;
                }
            }
            "removeEntity" => {
                if let Some(id) = event.args.first().and_then(Value::as_str) {
                    self.handle_remote_remove(id).await?;
                }
            }
            "clientTracking.clientUpdated" => {
                if let Some(user) = event.args.first()
                    && let Some(id) = client_id(user)
                {
                    self.presence.insert(id.into(), user.clone());
                    self.emit_presence();
                }
            }
            "clientTracking.clientDisconnected" => {
                if let Some(id) = event.args.first().and_then(Value::as_str) {
                    self.presence.remove(id);
                    self.emit_presence();
                }
            }
            _ => {}
        }
        self.persist_status()?;
        Ok(())
    }

    async fn refresh_presence(&mut self) -> Result<(), NativeSyncError> {
        self.presence.clear();
        for user in self.realtime.connected_users().await? {
            if let Some(id) = client_id(&user) {
                self.presence.insert(id.into(), user);
            }
        }
        self.emit_presence();
        Ok(())
    }

    async fn reconnect(&mut self) -> Result<broadcast::Receiver<RealtimeEvent>, NativeSyncError> {
        let realtime = Arc::new(
            OverleafRealtimeSession::connect_and_join(
                &self.config.server,
                &self.identity,
                &self.config.project_id,
            )
            .await?,
        );
        let model = ProjectModel::from_project(realtime.project().clone())?;
        let mut removed = self
            .model
            .all()
            .filter(|entity| entity.parent_id.is_some() && model.get_by_id(&entity.id).is_none())
            .cloned()
            .collect::<Vec<_>>();
        removed.sort_by_key(|entity| entity.path.matches('/').count());
        for entity in removed {
            if self.model.get_by_id(&entity.id).is_some() {
                self.handle_remote_remove(&entity.id).await?;
            }
        }
        self.model = model;
        self.realtime = realtime;
        self.recover_pending_operations().await;
        self.bootstrap_directories()?;
        self.bootstrap_documents().await?;
        self.bootstrap_files().await?;
        self.refresh_presence().await?;
        self.connection_state = "live".into();
        self.last_error = None;
        self.persist_status()?;
        self.emit_status();
        Ok(self.realtime.subscribe())
    }

    fn move_stored_document_if_needed(
        &mut self,
        entity: &ProjectEntity,
    ) -> Result<(), NativeSyncError> {
        let Some(stored_path) = self
            .store
            .document(&entity.id)
            .map(|stored| stored.path.clone())
        else {
            return Ok(());
        };
        if stored_path == entity.path {
            return Ok(());
        }
        self.move_materialized_path(&stored_path, &entity.path)
    }

    fn move_materialized_path(
        &mut self,
        old_path: &str,
        new_path: &str,
    ) -> Result<(), NativeSyncError> {
        let old = self.root.join(old_path);
        let new = self.root.join(new_path);
        if !old.exists() || old_path == new_path {
            return Ok(());
        }
        if new.exists() {
            let old_bytes = if old.is_file() {
                read_optional_bytes(&old)?
            } else {
                None
            };
            let new_bytes = if new.is_file() {
                read_optional_bytes(&new)?
            } else {
                None
            };
            if old_bytes.is_some() && old_bytes == new_bytes {
                fs::remove_file(&old)?;
                return Ok(());
            }
            if !self.path_is_conflicted(new_path) {
                self.store.record_conflict(
                    &format!("path:{old_path}"),
                    new_path,
                    "path",
                    "remote-move-target-exists",
                    None,
                    new_bytes.as_deref(),
                    old_bytes.as_deref(),
                )?;
            }
            if !self.path_is_conflicted(old_path) {
                self.store.record_conflict(
                    &format!("path-source:{old_path}"),
                    old_path,
                    "path",
                    "remote-move-source-preserved",
                    None,
                    old_bytes.as_deref(),
                    None,
                )?;
            }
            return Ok(());
        }
        if let Some(parent) = new.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(old, new)?;
        Ok(())
    }

    fn record_document_conflict(
        &mut self,
        document: &mut NativeDocument,
        reason: &str,
    ) -> Result<(), NativeSyncError> {
        self.store.record_conflict(
            &document.id,
            &document.path,
            "text",
            reason,
            Some(document.base_text.as_bytes()),
            Some(document.local_text.as_bytes()),
            Some(document.remote_text.as_bytes()),
        )?;
        document.pending = false;
        document.state = "conflicted".into();
        Ok(())
    }

    fn collaborators(&self) -> Vec<NativePresence> {
        let own_public_id = self.realtime.public_id();
        let mut collaborators = self
            .presence
            .values()
            .filter_map(|user| presence_from_value(user, &self.model))
            .filter(|presence| own_public_id.as_deref() != Some(&presence.client_id))
            .collect::<Vec<_>>();
        collaborators.sort_by(|left, right| left.client_id.cmp(&right.client_id));
        collaborators
    }

    fn status(&self) -> NativeStatus {
        let mut documents = DocumentCounters {
            total: self.documents.len(),
            ..DocumentCounters::default()
        };
        for document in self.documents.values() {
            if document.clean() {
                documents.clean += 1;
            } else if document.state == "conflicted" {
                documents.conflicted += 1;
            } else {
                documents.dirty += 1;
            }
            if document.pending {
                documents.pending += 1;
            }
        }
        NativeStatus {
            state: self.connection_state.clone(),
            connected: self.connection_state == "live",
            server: self.config.server.clone(),
            project_id: self.config.project_id.clone(),
            project_name: self
                .model
                .project()
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            root: self.root.clone(),
            documents,
            conflicts: self.store.unresolved_conflicts().cloned().collect(),
            pending_operations: self.store.state().operations.values().cloned().collect(),
            collaborators: self.collaborators(),
            last_compile: self.last_compile.clone(),
            last_error: self.last_error.clone(),
        }
    }

    fn emit_presence(&self) {
        let _ = self.events.send(NativeSyncEvent::PresenceChanged {
            collaborators: self.collaborators(),
        });
    }

    fn emit_status(&self) {
        let _ = self.events.send(NativeSyncEvent::StatusChanged {
            status: self.status(),
        });
    }

    fn persist_status(&self) -> Result<(), NativeSyncError> {
        let bytes = serde_json::to_vec_pretty(&self.status())?;
        atomic_write(&self.root.join(".semantic-zed/status.json"), &bytes, 0o600)?;
        Ok(())
    }

    fn record_error(&mut self, error: NativeSyncError) {
        let connection_error = matches!(
            &error,
            NativeSyncError::Realtime(_) | NativeSyncError::Http(HttpError::Request(_))
        );
        let message = error.to_string();
        self.last_error = Some(message.clone());
        if connection_error && self.connection_state == "live" {
            self.connection_state = "offline".into();
        }
        let _ = self.events.send(NativeSyncEvent::Error { message });
        let _ = self.persist_status();
        self.emit_status();
    }
}

fn client_id(value: &Value) -> Option<&str> {
    value
        .get("client_id")
        .or_else(|| value.get("clientId"))
        .and_then(Value::as_str)
}

fn presence_from_value(value: &Value, model: &ProjectModel) -> Option<NativePresence> {
    let client_id = client_id(value)?.to_owned();
    let cursor = value
        .get("cursorData")
        .or_else(|| value.get("cursor_data"))?;
    let document_id = cursor
        .get("doc_id")
        .or_else(|| cursor.get("docId"))
        .and_then(Value::as_str)?;
    let document_path = model.get_by_id(document_id)?.path.clone();
    let first_name = value
        .get("first_name")
        .or_else(|| value.get("firstName"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let last_name = value
        .get("last_name")
        .or_else(|| value.get("lastName"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = format!("{first_name} {last_name}").trim().to_owned();
    let name = if name.is_empty() {
        value
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("Overleaf collaborator")
            .to_owned()
    } else {
        name
    };
    Some(NativePresence {
        client_id,
        name,
        document_path,
        row: cursor.get("row").and_then(Value::as_u64).unwrap_or(0) as u32,
        column: cursor.get("column").and_then(Value::as_u64).unwrap_or(0) as u32,
    })
}

fn split_parent(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn operation_string<'a>(operation: &'a Value, key: &str) -> Option<&'a str> {
    operation.get(key).and_then(Value::as_str)
}

fn required_operation_string<'a>(
    operation: &'a Value,
    key: &str,
) -> Result<&'a str, NativeSyncError> {
    operation_string(operation, key)
        .ok_or_else(|| NativeSyncError::InvalidConfig(format!("replica operation has no {key}")))
}

fn read_optional_text(path: &Path) -> Result<Option<String>, NativeSyncError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_optional_bytes(path: &Path) -> Result<Option<Vec<u8>>, NativeSyncError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn entity_record(id: &str, path: &str, digest: Option<String>, kind: &str) -> EntityRecord {
    EntityRecord {
        id: id.into(),
        path: path.into(),
        digest: digest.clone(),
        base_digest: digest.clone(),
        local_digest: digest.clone(),
        remote_digest: digest,
        kind: Some(kind.into()),
        updated_at: String::new(),
        extra: Map::new(),
    }
}

fn read_optional_json(path: &Path) -> Option<Value> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_existing_project_metadata_contract() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join(".semantic-zed")).unwrap();
        fs::write(
            directory.path().join(".semantic-zed/project.json"),
            serde_json::to_vec(&json!({
                "version": 1,
                "server": "https://www.overleaf.com/",
                "projectId": "paper-1",
                "scanIntervalMs": 500,
                "adoptLocalOnFirstSync": false,
                "allowBinaryWrites": true
            }))
            .unwrap(),
        )
        .unwrap();
        let config = NativeProjectConfig::load(directory.path()).unwrap();
        assert_eq!(config.project_id, "paper-1");
        assert_eq!(config.scan_interval_ms, 500);
    }

    #[test]
    fn creates_native_project_metadata_without_a_node_initializer() {
        let directory = tempfile::tempdir().unwrap();
        NativeProjectConfig::new("paper-native")
            .unwrap()
            .save(directory.path())
            .unwrap();

        let config = NativeProjectConfig::load(directory.path()).unwrap();
        assert_eq!(config.project_id, "paper-native");
        assert_eq!(config.server, "https://www.overleaf.com/");
        assert_eq!(config.scan_interval_ms, 750);
        assert!(config.allow_binary_writes);
    }

    #[test]
    fn normalizes_presence_to_a_stable_document_path() {
        let model = ProjectModel::from_project(json!({
            "rootFolder": [{
                "_id": "root",
                "docs": [{ "_id": "doc-1", "name": "main.tex" }],
                "fileRefs": [],
                "folders": []
            }]
        }))
        .unwrap();
        let presence = presence_from_value(
            &json!({
                "client_id": "client-1",
                "first_name": "Alex",
                "last_name": "Lee",
                "cursorData": { "doc_id": "doc-1", "row": 4, "column": 9 }
            }),
            &model,
        )
        .unwrap();
        assert_eq!(presence.document_path, "main.tex");
        assert_eq!(presence.name, "Alex Lee");
        assert_eq!((presence.row, presence.column), (4, 9));
    }
}
