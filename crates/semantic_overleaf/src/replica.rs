use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use sqlez::connection::Connection;
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const STORE_VERSION: u32 = 3;
const RECORD_KINDS: [&str; 5] = [
    "documents",
    "files",
    "directories",
    "conflicts",
    "operations",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentRecord {
    pub id: String,
    pub path: String,
    pub version: u64,
    #[serde(default)]
    pub last_version: Option<u64>,
    pub ot_type: String,
    pub state: String,
    #[serde(default)]
    pub collaborator_revision: u64,
    pub base_text: String,
    pub local_text: String,
    pub remote_text: String,
    #[serde(default)]
    pub raw_snapshot: Value,
    #[serde(default)]
    pub ranges: Value,
    pub read_only_reason: Option<String>,
    #[serde(default)]
    pub pending: Option<Value>,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityRecord {
    pub id: String,
    pub path: String,
    pub digest: Option<String>,
    #[serde(default)]
    pub base_digest: Option<String>,
    #[serde(default)]
    pub local_digest: Option<String>,
    #[serde(default)]
    pub remote_digest: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    pub updated_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRecord {
    pub id: String,
    pub key: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub reason: String,
    pub created_at: String,
    pub base_file: Option<String>,
    pub local_file: Option<String>,
    pub remote_file: Option<String>,
    pub status: String,
    pub resolution: Option<String>,
    #[serde(default)]
    pub resolved_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaState {
    pub version: u32,
    pub server: String,
    pub project_id: String,
    pub updated_at: String,
    pub documents: HashMap<String, DocumentRecord>,
    pub files: HashMap<String, EntityRecord>,
    pub directories: HashMap<String, EntityRecord>,
    pub conflicts: HashMap<String, ConflictRecord>,
    pub operations: HashMap<String, Value>,
}

#[derive(Debug, Error)]
pub enum ReplicaError {
    #[error("replica I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("replica database failed: {0}")]
    Database(#[from] sqlez::anyhow::Error),
    #[error("replica JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("replica timestamp failed: {0}")]
    Timestamp(#[from] time::error::Format),
    #[error("replica database could not be opened durably: {0}")]
    NonPersistent(PathBuf),
    #[error("invalid replica content digest: {0}")]
    InvalidDigest(String),
    #[error("replica record has an unknown kind: {0}")]
    UnknownRecordKind(String),
    #[error("replica operation does not exist: {0}")]
    UnknownOperation(String),
}

pub struct ReplicaStore {
    root: PathBuf,
    database_path: PathBuf,
    conflict_path: PathBuf,
    blobs: BlobStore,
    connection: Connection,
    state: ReplicaState,
}

impl ReplicaStore {
    pub fn open(
        root: impl Into<PathBuf>,
        server: &str,
        project_id: &str,
    ) -> Result<Self, ReplicaError> {
        let root = root.into();
        let metadata = root.join(".semantic-zed");
        let database_path = metadata.join("replica-state.sqlite3");
        let conflict_path = metadata.join("conflicts");
        fs::create_dir_all(&conflict_path)?;
        let blobs = BlobStore::new(metadata.join("blobs"))?;
        let mut connection = open_connection(&database_path)?;
        let stored_server = metadata_value(&connection, "server")?;
        let stored_project = metadata_value(&connection, "project_id")?;
        let mut initialized = stored_server.is_some() || stored_project.is_some();
        if stored_server
            .as_deref()
            .is_some_and(|value| value != server)
            || stored_project
                .as_deref()
                .is_some_and(|value| value != project_id)
        {
            drop(connection);
            preserve_mismatched_database(&database_path)?;
            connection = open_connection(&database_path)?;
            initialized = false;
        }
        let mut store = Self {
            root,
            database_path,
            conflict_path,
            blobs,
            connection,
            state: ReplicaState {
                version: STORE_VERSION,
                server: server.into(),
                project_id: project_id.into(),
                updated_at: now()?,
                ..ReplicaState::default()
            },
        };
        store.load_records()?;
        store.state.version = STORE_VERSION;
        store.state.server = server.into();
        store.state.project_id = project_id.into();
        if !initialized {
            store.persist()?;
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn state(&self) -> &ReplicaState {
        &self.state
    }

    pub fn document(&self, id: &str) -> Option<&DocumentRecord> {
        self.state.documents.get(id)
    }

    pub fn upsert_document(&mut self, mut document: DocumentRecord) -> Result<(), ReplicaError> {
        document.updated_at = now()?;
        self.state.documents.insert(document.id.clone(), document);
        Ok(())
    }

    pub fn remove_document(&mut self, id: &str) {
        self.state.documents.remove(id);
    }

    pub fn upsert_file(&mut self, mut file: EntityRecord) -> Result<(), ReplicaError> {
        file.updated_at = now()?;
        self.state.files.insert(file.id.clone(), file);
        Ok(())
    }

    pub fn remove_file(&mut self, id: &str) {
        self.state.files.remove(id);
    }

    pub fn upsert_directory(&mut self, mut directory: EntityRecord) -> Result<(), ReplicaError> {
        directory.updated_at = now()?;
        self.state
            .directories
            .insert(directory.id.clone(), directory);
        Ok(())
    }

    pub fn remove_directory(&mut self, id: &str) {
        self.state.directories.remove(id);
    }

    pub fn begin_operation(&mut self, mut operation: Value) -> Result<String, ReplicaError> {
        let created_at = now()?;
        let digest = digest_bytes(format!("{operation}:{created_at}").as_bytes());
        let id = operation
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "{}-{}",
                    OffsetDateTime::now_utc().unix_timestamp(),
                    &digest[..12]
                )
            });
        if let Some(object) = operation.as_object_mut() {
            object.insert("id".into(), Value::String(id.clone()));
            object
                .entry("status")
                .or_insert_with(|| Value::String("planned".into()));
            object
                .entry("createdAt")
                .or_insert_with(|| Value::String(created_at.clone()));
            object.insert("updatedAt".into(), Value::String(created_at));
        }
        self.state.operations.insert(id.clone(), operation);
        self.persist()?;
        Ok(id)
    }

    pub fn update_operation(&mut self, id: &str, patch: Value) -> Result<(), ReplicaError> {
        let Some(operation) = self.state.operations.get_mut(id) else {
            return Err(ReplicaError::UnknownOperation(id.into()));
        };
        if let (Some(operation), Some(patch)) = (operation.as_object_mut(), patch.as_object()) {
            operation.extend(patch.clone());
            operation.insert("updatedAt".into(), Value::String(now()?));
        }
        self.persist()?;
        Ok(())
    }

    pub fn complete_operation(&mut self, id: &str) -> Result<(), ReplicaError> {
        self.state.operations.remove(id);
        self.persist()?;
        Ok(())
    }

    pub fn unresolved_conflicts(&self) -> impl Iterator<Item = &ConflictRecord> {
        self.state
            .conflicts
            .values()
            .filter(|conflict| conflict.status == "unresolved")
    }

    pub fn record_conflict(
        &mut self,
        key: &str,
        path: &str,
        kind: &str,
        reason: &str,
        base: Option<&[u8]>,
        local: Option<&[u8]>,
        remote: Option<&[u8]>,
    ) -> Result<ConflictRecord, ReplicaError> {
        let created_at = now()?;
        let digest = digest_bytes(format!("{key}:{path}:{reason}:{created_at}").as_bytes());
        let id = format!(
            "{}-{}",
            OffsetDateTime::now_utc().unix_timestamp(),
            &digest[..12]
        );
        let evidence = |suffix: &str| Some(format!(".semantic-zed/conflicts/{id}.{suffix}"));
        if let Some(bytes) = base {
            atomic_write(&self.conflict_path.join(format!("{id}.base")), bytes, 0o600)?;
        }
        if let Some(bytes) = local {
            atomic_write(
                &self.conflict_path.join(format!("{id}.local")),
                bytes,
                0o600,
            )?;
        }
        if let Some(bytes) = remote {
            atomic_write(
                &self.conflict_path.join(format!("{id}.remote")),
                bytes,
                0o600,
            )?;
        }
        let record = ConflictRecord {
            id: id.clone(),
            key: key.into(),
            path: path.into(),
            kind: kind.into(),
            reason: reason.into(),
            created_at,
            base_file: base.and_then(|_| evidence("base")),
            local_file: local.and_then(|_| evidence("local")),
            remote_file: remote.and_then(|_| evidence("remote")),
            status: "unresolved".into(),
            resolution: None,
            resolved_at: None,
            extra: Map::new(),
        };
        self.state.conflicts.insert(id, record.clone());
        self.persist()?;
        Ok(record)
    }

    pub fn resolve_conflicts_for_path(
        &mut self,
        path: &str,
        resolution: &str,
    ) -> Result<usize, ReplicaError> {
        let mut changed = 0;
        for conflict in self.state.conflicts.values_mut() {
            if conflict.status == "unresolved"
                && (conflict.path == path
                    || conflict.path.starts_with(&format!("{path}/"))
                    || path.starts_with(&format!("{}/", conflict.path)))
            {
                conflict.status = "resolved".into();
                conflict.resolution = Some(resolution.into());
                conflict.resolved_at = Some(now()?);
                changed += 1;
            }
        }
        if changed > 0 {
            self.persist()?;
        }
        Ok(changed)
    }

    pub fn persist(&mut self) -> Result<(), ReplicaError> {
        self.state.version = STORE_VERSION;
        self.state.updated_at = now()?;
        let mut serialized = HashMap::<String, HashMap<String, Value>>::new();
        let mut referenced = HashSet::new();
        let mut documents = HashMap::new();
        for (id, document) in &self.state.documents {
            documents.insert(
                id.clone(),
                self.externalize_document(document, &mut referenced)?,
            );
        }
        serialized.insert("documents".into(), documents);
        serialized.insert("files".into(), serialize_map(&self.state.files)?);
        serialized.insert(
            "directories".into(),
            serialize_map(&self.state.directories)?,
        );
        serialized.insert("conflicts".into(), serialize_map(&self.state.conflicts)?);
        serialized.insert("operations".into(), self.state.operations.clone());

        self.connection.exec("BEGIN IMMEDIATE")?()?;
        let result = (|| -> Result<(), ReplicaError> {
            self.connection.exec("DELETE FROM records")?()?;
            let mut insert = self.connection.exec_bound::<(String, String, String)>(
                "INSERT INTO records(kind, id, json) VALUES (?, ?, ?)",
            )?;
            for kind in RECORD_KINDS {
                if let Some(records) = serialized.get(kind) {
                    for (id, value) in records {
                        insert((kind.into(), id.clone(), serde_json::to_string(value)?))?;
                    }
                }
            }
            drop(insert);
            set_metadata(
                &self.connection,
                "store_version",
                &STORE_VERSION.to_string(),
            )?;
            set_metadata(&self.connection, "server", &self.state.server)?;
            set_metadata(&self.connection, "project_id", &self.state.project_id)?;
            set_metadata(&self.connection, "updated_at", &self.state.updated_at)?;
            self.connection.exec("COMMIT")?()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self
                .connection
                .exec("ROLLBACK")
                .and_then(|mut rollback| rollback());
        }
        result?;
        self.blobs.collect(&referenced)?;
        Ok(())
    }

    fn load_records(&mut self) -> Result<(), ReplicaError> {
        let rows = self
            .connection
            .select::<(String, String, String)>("SELECT kind, id, json FROM records")?(
        )?;
        for (kind, id, json) in rows {
            match kind.as_str() {
                "documents" => {
                    let mut value: Value = serde_json::from_str(&json)?;
                    insert_record_id(&mut value, &id);
                    self.state
                        .documents
                        .insert(id, self.hydrate_document(value)?);
                }
                "files" => {
                    let mut value: Value = serde_json::from_str(&json)?;
                    insert_record_id(&mut value, &id);
                    self.state.files.insert(id, serde_json::from_value(value)?);
                }
                "directories" => {
                    let mut value: Value = serde_json::from_str(&json)?;
                    insert_record_id(&mut value, &id);
                    self.state
                        .directories
                        .insert(id, serde_json::from_value(value)?);
                }
                "conflicts" => {
                    self.state
                        .conflicts
                        .insert(id, serde_json::from_str(&json)?);
                }
                "operations" => {
                    self.state
                        .operations
                        .insert(id, serde_json::from_str(&json)?);
                }
                unknown => return Err(ReplicaError::UnknownRecordKind(unknown.into())),
            }
        }
        if let Some(updated_at) = metadata_value(&self.connection, "updated_at")? {
            self.state.updated_at = updated_at;
        }
        Ok(())
    }

    fn externalize_document(
        &self,
        document: &DocumentRecord,
        referenced: &mut HashSet<String>,
    ) -> Result<Value, ReplicaError> {
        let mut value = serde_json::to_value(document)?;
        let object = value.as_object_mut().expect("document record is an object");
        for field in ["baseText", "localText", "remoteText"] {
            let text = object
                .remove(field)
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
            let digest = self.blobs.put(text.as_bytes())?;
            referenced.insert(digest.clone());
            object.insert(format!("{field}Blob"), Value::String(digest));
        }
        if let Some(raw_snapshot) = object.get_mut("rawSnapshot").and_then(Value::as_object_mut)
            && let Some(content) = raw_snapshot
                .remove("content")
                .and_then(|value| value.as_str().map(str::to_owned))
        {
            let digest = self.blobs.put(content.as_bytes())?;
            referenced.insert(digest.clone());
            raw_snapshot.insert("contentBlob".into(), Value::String(digest));
        }
        Ok(value)
    }

    fn hydrate_document(&self, mut value: Value) -> Result<DocumentRecord, ReplicaError> {
        let object = value.as_object_mut().expect("document record is an object");
        for field in ["baseText", "localText", "remoteText"] {
            let blob_field = format!("{field}Blob");
            if let Some(digest) = object
                .remove(&blob_field)
                .and_then(|value| value.as_str().map(str::to_owned))
            {
                let text = String::from_utf8(self.blobs.get(&digest)?)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                object.insert(field.into(), Value::String(text));
            }
        }
        if let Some(raw_snapshot) = object.get_mut("rawSnapshot").and_then(Value::as_object_mut)
            && let Some(digest) = raw_snapshot
                .remove("contentBlob")
                .and_then(|value| value.as_str().map(str::to_owned))
        {
            let content = String::from_utf8(self.blobs.get(&digest)?)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            raw_snapshot.insert("content".into(), Value::String(content));
        }
        Ok(serde_json::from_value(value)?)
    }
}

struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    fn new(root: PathBuf) -> Result<Self, ReplicaError> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, digest: &str) -> Result<PathBuf, ReplicaError> {
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ReplicaError::InvalidDigest(digest.into()));
        }
        Ok(self.root.join(&digest[..2]).join(&digest[2..]))
    }

    fn put(&self, bytes: &[u8]) -> Result<String, ReplicaError> {
        let digest = digest_bytes(bytes);
        let path = self.path_for(&digest)?;
        if !path.is_file() {
            atomic_write(&path, bytes, 0o600)?;
        }
        Ok(digest)
    }

    fn get(&self, digest: &str) -> Result<Vec<u8>, ReplicaError> {
        Ok(fs::read(self.path_for(digest)?)?)
    }

    fn collect(&self, referenced: &HashSet<String>) -> Result<(), ReplicaError> {
        for directory in fs::read_dir(&self.root)? {
            let directory = directory?;
            if !directory.file_type()?.is_dir() {
                continue;
            }
            let prefix = directory.file_name().to_string_lossy().into_owned();
            if prefix.len() != 2 {
                continue;
            }
            for entry in fs::read_dir(directory.path())? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let digest = format!("{prefix}{}", entry.file_name().to_string_lossy());
                    if !referenced.contains(&digest) {
                        fs::remove_file(entry.path())?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn serialize_map<T: Serialize>(
    map: &HashMap<String, T>,
) -> Result<HashMap<String, Value>, ReplicaError> {
    map.iter()
        .map(|(id, value)| Ok((id.clone(), serde_json::to_value(value)?)))
        .collect()
}

fn insert_record_id(value: &mut Value, id: &str) {
    if let Some(object) = value.as_object_mut() {
        object
            .entry("id")
            .or_insert_with(|| Value::String(id.into()));
    }
}

fn open_connection(path: &Path) -> Result<Connection, ReplicaError> {
    let connection = Connection::open_file(path.to_string_lossy().as_ref());
    if !connection.persistent() {
        return Err(ReplicaError::NonPersistent(path.into()));
    }
    let _journal_mode = connection.select_row::<String>("PRAGMA journal_mode=WAL")?()?;
    connection.exec("PRAGMA synchronous=FULL")?()?;
    connection.exec("PRAGMA foreign_keys=ON")?()?;
    connection.exec("PRAGMA busy_timeout=5000")?()?;
    connection
        .exec("CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL)")?(
    )?;
    connection.exec("CREATE TABLE IF NOT EXISTS records(kind TEXT NOT NULL, id TEXT NOT NULL, json TEXT NOT NULL, PRIMARY KEY(kind, id))")?()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(connection)
}

fn metadata_value(connection: &Connection, key: &str) -> Result<Option<String>, ReplicaError> {
    Ok(connection.select_row_bound::<String, String>(
        "SELECT value FROM metadata WHERE key = ?",
    )?(key.into())?)
}

fn set_metadata(connection: &Connection, key: &str, value: &str) -> Result<(), ReplicaError> {
    connection.exec_bound::<(String, String)>(
        "INSERT INTO metadata(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
    )?((key.into(), value.into()))?;
    Ok(())
}

fn preserve_mismatched_database(path: &Path) -> Result<(), ReplicaError> {
    let timestamp = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let suffix = format!("mismatched-{timestamp}");
    for candidate in [
        path.to_path_buf(),
        path.with_extension("sqlite3-wal"),
        path.with_extension("sqlite3-shm"),
    ] {
        if candidate.exists() {
            let file_name = candidate.file_name().unwrap().to_string_lossy();
            fs::rename(
                &candidate,
                candidate.with_file_name(format!("{file_name}.{suffix}")),
            )?;
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), ReplicaError> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let token = format!(
        "{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let temp = path.with_extension(format!("tmp-{token}"));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode);
    }
    let mut file = options.open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temp, path)?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now() -> Result<String, ReplicaError> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> DocumentRecord {
        DocumentRecord {
            id: "doc-1".into(),
            path: "main.tex".into(),
            version: 7,
            last_version: Some(6),
            ot_type: "sharejs-text-ot".into(),
            state: "ready".into(),
            collaborator_revision: 0,
            base_text: "durable base".into(),
            local_text: "durable local".into(),
            remote_text: "durable remote".into(),
            raw_snapshot: Value::Null,
            ranges: Value::Null,
            read_only_reason: None,
            pending: None,
            updated_at: String::new(),
            extra: Map::new(),
        }
    }

    #[test]
    fn persists_documents_in_sqlite_wal_with_cas_text() {
        let directory = tempfile::tempdir().unwrap();
        {
            let mut store =
                ReplicaStore::open(directory.path(), "https://overleaf.test/", "paper-1").unwrap();
            store.upsert_document(document()).unwrap();
            store.persist().unwrap();
            assert!(store.database_path().is_file());
            assert_eq!(store.unresolved_conflicts().count(), 0);
        }
        let store =
            ReplicaStore::open(directory.path(), "https://overleaf.test/", "paper-1").unwrap();
        let reloaded = store.document("doc-1").unwrap();
        let mut expected = document();
        expected.updated_at = reloaded.updated_at.clone();
        assert_eq!(reloaded, &expected);
        let database = fs::read(store.database_path()).unwrap();
        assert!(!String::from_utf8_lossy(&database).contains("durable local"));
    }

    #[test]
    fn records_private_conflict_evidence_and_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            ReplicaStore::open(directory.path(), "https://overleaf.test/", "paper-1").unwrap();
        let conflict = store
            .record_conflict(
                "doc-1",
                "main.tex",
                "text",
                "overlapping-edits",
                Some(b"base"),
                Some(b"local"),
                Some(b"remote"),
            )
            .unwrap();
        assert!(
            directory
                .path()
                .join(conflict.local_file.unwrap())
                .is_file()
        );
        assert_eq!(store.unresolved_conflicts().count(), 1);
        assert_eq!(
            store
                .resolve_conflicts_for_path("main.tex", "local")
                .unwrap(),
            1
        );
        assert_eq!(store.unresolved_conflicts().count(), 0);
    }

    #[test]
    fn persists_and_completes_recoverable_operations() {
        let directory = tempfile::tempdir().unwrap();
        let operation_id;
        {
            let mut store =
                ReplicaStore::open(directory.path(), "https://overleaf.test/", "paper-1").unwrap();
            operation_id = store
                .begin_operation(serde_json::json!({
                    "id": "replace-1",
                    "kind": "binary-replace",
                    "path": "figure.png"
                }))
                .unwrap();
            store
                .update_operation(
                    &operation_id,
                    serde_json::json!({ "status": "verified", "replacementId": "file-2" }),
                )
                .unwrap();
        }
        {
            let mut store =
                ReplicaStore::open(directory.path(), "https://overleaf.test/", "paper-1").unwrap();
            let operation = &store.state().operations[&operation_id];
            assert_eq!(operation["status"], "verified");
            assert_eq!(operation["replacementId"], "file-2");
            store.complete_operation(&operation_id).unwrap();
        }
        let store =
            ReplicaStore::open(directory.path(), "https://overleaf.test/", "paper-1").unwrap();
        assert!(store.state().operations.is_empty());
    }

    #[test]
    fn preserves_a_database_with_the_wrong_project_identity() {
        let directory = tempfile::tempdir().unwrap();
        drop(ReplicaStore::open(directory.path(), "https://overleaf.test/", "paper-1").unwrap());
        drop(ReplicaStore::open(directory.path(), "https://overleaf.test/", "paper-2").unwrap());
        let metadata = directory.path().join(".semantic-zed");
        assert!(
            fs::read_dir(metadata)
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains("mismatched-"))
        );
    }

    #[test]
    fn opens_the_node_v2_record_shape_without_rewriting_it() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory
            .path()
            .join(".semantic-zed")
            .join("replica-state.sqlite3");
        fs::create_dir_all(database_path.parent().unwrap()).unwrap();
        let connection = open_connection(&database_path).unwrap();
        set_metadata(&connection, "store_version", "2").unwrap();
        set_metadata(&connection, "server", "https://overleaf.test/").unwrap();
        set_metadata(&connection, "project_id", "paper-1").unwrap();
        let blobs = BlobStore::new(directory.path().join(".semantic-zed/blobs")).unwrap();
        let digest = blobs.put(b"legacy text").unwrap();
        connection
            .exec_bound::<(String, String, String)>(
                "INSERT INTO records(kind, id, json) VALUES (?, ?, ?)",
            )
            .unwrap()((
            "documents".into(),
            "doc-legacy".into(),
            serde_json::to_string(&serde_json::json!({
                "path": "main.tex",
                "version": 9,
                "lastVersion": 8,
                "state": "ready",
                "collaboratorRevision": 2,
                "otType": "sharejs-text-ot",
                "baseTextBlob": digest,
                "localTextBlob": digest,
                "remoteTextBlob": digest,
                "updatedAt": "2026-08-09T00:00:00Z"
            }))
            .unwrap(),
        ))
        .unwrap();
        drop(connection);

        let store =
            ReplicaStore::open(directory.path(), "https://overleaf.test/", "paper-1").unwrap();
        let document = store.document("doc-legacy").unwrap();
        assert_eq!(document.id, "doc-legacy");
        assert_eq!(document.local_text, "legacy text");
        assert_eq!(document.last_version, Some(8));
        assert_eq!(document.collaborator_revision, 2);
        assert_eq!(
            metadata_value(&store.connection, "store_version")
                .unwrap()
                .as_deref(),
            Some("2")
        );
    }
}
