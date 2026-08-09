use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, System};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipRecord {
    pub version: u32,
    pub token: String,
    pub pid: u32,
    pub hostname: String,
    pub project_id: String,
    pub root: PathBuf,
    pub created_at: String,
}

#[derive(Debug, Error)]
pub enum OwnershipError {
    #[error("workspace ownership I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("workspace ownership record is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace ownership timestamp failed: {0}")]
    Timestamp(#[from] time::error::Format),
    #[error("another process owns this Overleaf replica: {path}")]
    AlreadyOwned {
        path: PathBuf,
        owner: Option<Box<OwnershipRecord>>,
    },
}

pub struct WorkspaceOwnership {
    path: PathBuf,
    record: OwnershipRecord,
}

impl WorkspaceOwnership {
    pub fn acquire(root: &Path, project_id: &str) -> Result<Self, OwnershipError> {
        let metadata = root.join(".semantic-zed");
        fs::create_dir_all(&metadata)?;
        let path = metadata.join("sync-owner.json");
        let record = OwnershipRecord {
            version: 1,
            token: Uuid::new_v4().to_string(),
            pid: std::process::id(),
            hostname: System::host_name().unwrap_or_else(|| "unknown-host".into()),
            project_id: project_id.into(),
            root: fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
            created_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
        };
        match write_new_record(&path, &record) {
            Ok(()) => return Ok(Self { path, record }),
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error.into()),
            Err(_) => {}
        }

        let current = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<OwnershipRecord>(&bytes).ok());
        let stale = current
            .as_ref()
            .is_some_and(|owner| owner.hostname == record.hostname && !process_is_alive(owner.pid));
        if stale {
            fs::remove_file(&path)?;
            write_new_record(&path, &record)?;
            return Ok(Self { path, record });
        }
        Err(OwnershipError::AlreadyOwned {
            path,
            owner: current.map(Box::new),
        })
    }

    pub fn record(&self) -> &OwnershipRecord {
        &self.record
    }

    pub fn release(self) {}
}

impl Drop for WorkspaceOwnership {
    fn drop(&mut self) {
        let current = fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<OwnershipRecord>(&bytes).ok());
        if current.as_ref().map(|record| &record.token) == Some(&self.record.token) {
            let _ = fs::remove_file(&self.path);
            if let Some(parent) = self.path.parent()
                && let Ok(directory) = File::open(parent)
            {
                let _ = directory.sync_all();
            }
        }
    }
}

fn write_new_record(path: &Path, record: &OwnershipRecord) -> Result<(), io::Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let mut bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn process_is_alive(pid: u32) -> bool {
    let mut system = System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
    );
    system.process(Pid::from_u32(pid)).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provides_single_process_ownership_and_token_guarded_release() {
        let directory = tempfile::tempdir().unwrap();
        let ownership = WorkspaceOwnership::acquire(directory.path(), "paper-1").unwrap();
        assert!(matches!(
            WorkspaceOwnership::acquire(directory.path(), "paper-1"),
            Err(OwnershipError::AlreadyOwned { .. })
        ));
        let path = directory.path().join(".semantic-zed/sync-owner.json");
        assert!(path.is_file());
        drop(ownership);
        assert!(!path.exists());
        drop(WorkspaceOwnership::acquire(directory.path(), "paper-1").unwrap());
    }

    #[test]
    fn reclaims_a_stale_owner_from_the_same_host() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join(".semantic-zed")).unwrap();
        let stale = OwnershipRecord {
            version: 1,
            token: "stale".into(),
            pid: u32::MAX,
            hostname: System::host_name().unwrap_or_else(|| "unknown-host".into()),
            project_id: "paper-1".into(),
            root: directory.path().into(),
            created_at: "2026-08-09T00:00:00Z".into(),
        };
        write_new_record(
            &directory.path().join(".semantic-zed/sync-owner.json"),
            &stale,
        )
        .unwrap();
        let ownership = WorkspaceOwnership::acquire(directory.path(), "paper-1").unwrap();
        assert_ne!(ownership.record().token, "stale");
    }
}
