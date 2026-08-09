use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::http::{Identity, canonical_server_url};

const DEFAULT_KEYCHAIN_SERVICE: &str = "com.semantic-zed-overleaf.credentials";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialBackend {
    MacOsKeychain,
    PrivateFile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialRecord {
    pub version: u32,
    pub server: String,
    pub identity: Identity,
    pub saved_at: String,
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("could not determine the user home directory")]
    MissingHome,
    #[error("invalid Overleaf server URL: {0}")]
    InvalidServer(#[from] url::ParseError),
    #[error("credential I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("stored credential JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("macOS Keychain operation failed: {0}")]
    Keychain(String),
    #[error("stored Overleaf credentials are incomplete for {0}")]
    Incomplete(String),
    #[error("stored credential server mismatch: expected {expected}, got {actual}")]
    ServerMismatch { expected: String, actual: String },
    #[error("could not format credential timestamp: {0}")]
    Timestamp(#[from] time::error::Format),
}

#[derive(Clone, Debug)]
pub struct CredentialStore {
    app_data_override: Option<PathBuf>,
    backend_override: Option<CredentialBackend>,
    keychain_service: String,
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self {
            app_data_override: None,
            backend_override: None,
            keychain_service: std::env::var("SZO_KEYCHAIN_SERVICE")
                .unwrap_or_else(|_| DEFAULT_KEYCHAIN_SERVICE.into()),
        }
    }
}

impl CredentialStore {
    pub fn with_app_data_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.app_data_override = Some(path.into());
        self
    }

    pub fn with_backend(mut self, backend: CredentialBackend) -> Self {
        self.backend_override = Some(backend);
        self
    }

    pub fn preferred_backend(&self) -> CredentialBackend {
        if let Some(backend) = self.backend_override {
            return backend;
        }
        match std::env::var("SZO_CREDENTIAL_BACKEND").ok().as_deref() {
            Some("file") => CredentialBackend::PrivateFile,
            Some("keychain") => CredentialBackend::MacOsKeychain,
            _ if cfg!(target_os = "macos") => CredentialBackend::MacOsKeychain,
            _ => CredentialBackend::PrivateFile,
        }
    }

    pub fn save(
        &self,
        server: &str,
        identity: Identity,
    ) -> Result<CredentialRecord, CredentialError> {
        let canonical = canonical_server_url(server)?.to_string();
        let record = CredentialRecord {
            version: 2,
            server: canonical,
            identity,
            saved_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
        };
        match self.preferred_backend() {
            CredentialBackend::MacOsKeychain => {
                self.save_keychain(server, &record)?;
                remove_if_exists(&self.credentials_path(server)?)?;
            }
            CredentialBackend::PrivateFile => {
                write_private_json(&self.credentials_path(server)?, &record)?;
            }
        }
        Ok(record)
    }

    pub fn load(&self, server: &str) -> Result<Option<CredentialRecord>, CredentialError> {
        if self.preferred_backend() == CredentialBackend::MacOsKeychain {
            match self.load_keychain(server) {
                Ok(record) => return Ok(Some(record)),
                Err(CredentialError::Keychain(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let path = self.credentials_path(server)?;
        let value = match fs::read_to_string(path) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let record: CredentialRecord = serde_json::from_str(&value)?;
        self.validate(server, &record)?;
        Ok(Some(record))
    }

    // Keychain access is a short, explicit platform boundary. Callers run this store off the UI
    // thread; keeping the credential API synchronous also keeps private-file fallback atomic.
    #[allow(clippy::disallowed_methods)]
    pub fn delete(&self, server: &str) -> Result<Vec<CredentialBackend>, CredentialError> {
        let mut removed = Vec::new();
        if cfg!(target_os = "macos")
            && self.backend_override != Some(CredentialBackend::PrivateFile)
        {
            let account = account_for_server(server)?;
            let output = Command::new("/usr/bin/security")
                .args([
                    "delete-generic-password",
                    "-s",
                    &self.keychain_service,
                    "-a",
                    &account,
                ])
                .output();
            if output.is_ok_and(|output| output.status.success()) {
                removed.push(CredentialBackend::MacOsKeychain);
            }
        }
        let path = self.credentials_path(server)?;
        if path.exists() {
            fs::remove_file(path)?;
            removed.push(CredentialBackend::PrivateFile);
        }
        Ok(removed)
    }

    pub fn credentials_path(&self, server: &str) -> Result<PathBuf, CredentialError> {
        Ok(self
            .app_data_dir()?
            .join("credentials")
            .join(format!("{}.json", account_for_server(server)?)))
    }

    pub fn browser_profile_path(&self, server: &str) -> Result<PathBuf, CredentialError> {
        Ok(self
            .app_data_dir()?
            .join("browser-profiles")
            .join(account_for_server(server)?))
    }

    fn app_data_dir(&self) -> Result<PathBuf, CredentialError> {
        if let Some(path) = &self.app_data_override {
            return Ok(path.clone());
        }
        if let Some(path) = std::env::var_os("SZO_HOME") {
            return Ok(PathBuf::from(path));
        }
        let home = dirs::home_dir().ok_or(CredentialError::MissingHome)?;
        if cfg!(target_os = "macos") {
            Ok(home
                .join("Library")
                .join("Application Support")
                .join("Semantic Zed Overleaf"))
        } else if cfg!(target_os = "windows") {
            Ok(std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData").join("Roaming"))
                .join("Semantic Zed Overleaf"))
        } else {
            Ok(std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("semantic-zed-overleaf"))
        }
    }

    #[allow(clippy::disallowed_methods)]
    fn save_keychain(
        &self,
        server: &str,
        record: &CredentialRecord,
    ) -> Result<(), CredentialError> {
        if !cfg!(target_os = "macos") {
            return Err(CredentialError::Keychain(
                "Keychain is only available on macOS".into(),
            ));
        }
        let account = account_for_server(server)?;
        let value = serde_json::to_string(record)?;
        let output = Command::new("/usr/bin/security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                &self.keychain_service,
                "-a",
                &account,
                "-w",
                &value,
            ])
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(CredentialError::Keychain(
                String::from_utf8_lossy(&output.stderr).trim().into(),
            ))
        }
    }

    #[allow(clippy::disallowed_methods)]
    fn load_keychain(&self, server: &str) -> Result<CredentialRecord, CredentialError> {
        if !cfg!(target_os = "macos") {
            return Err(CredentialError::Keychain(
                "Keychain is only available on macOS".into(),
            ));
        }
        let account = account_for_server(server)?;
        let output = Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                &self.keychain_service,
                "-a",
                &account,
                "-w",
            ])
            .output()?;
        if !output.status.success() {
            return Err(CredentialError::Keychain(
                String::from_utf8_lossy(&output.stderr).trim().into(),
            ));
        }
        let record: CredentialRecord = serde_json::from_slice(&output.stdout)?;
        self.validate(server, &record)?;
        Ok(record)
    }

    fn validate(&self, server: &str, record: &CredentialRecord) -> Result<(), CredentialError> {
        if record.identity.cookies.is_empty() || record.identity.csrf_token.is_empty() {
            return Err(CredentialError::Incomplete(server.into()));
        }
        let expected = canonical_server_url(server)?.to_string();
        let actual = canonical_server_url(&record.server)?.to_string();
        if expected != actual {
            return Err(CredentialError::ServerMismatch { expected, actual });
        }
        Ok(())
    }
}

fn account_for_server(server: &str) -> Result<String, CredentialError> {
    Ok(URL_SAFE_NO_PAD.encode(canonical_server_url(server)?.as_str()))
}

fn remove_if_exists(path: &Path) -> Result<(), io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn write_private_json(path: &Path, value: &CredentialRecord) -> Result<(), CredentialError> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "credential path has no parent")
    })?;
    fs::create_dir_all(parent)?;
    let token = format!(
        "{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let temp_path = path.with_extension(format!("json.tmp-{token}"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path)?;
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    remove_if_exists(path)?;
    fs::rename(&temp_path, path)?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_file_backend_is_node_compatible_and_mode_0600() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::default()
            .with_app_data_dir(directory.path())
            .with_backend(CredentialBackend::PrivateFile);
        let server = "https://credential-store.test.invalid";
        let identity = Identity {
            cookies: "overleaf_session2=secret".into(),
            csrf_token: "csrf-secret".into(),
            user_id: "user-1".into(),
            user_email: "person@example.com".into(),
        };
        store.save(server, identity.clone()).unwrap();
        let loaded = store.load(server).unwrap().unwrap();
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.server, "https://credential-store.test.invalid/");
        assert_eq!(loaded.identity, identity);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(store.credentials_path(server).unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_eq!(
            store.delete(server).unwrap(),
            vec![CredentialBackend::PrivateFile]
        );
        assert!(store.load(server).unwrap().is_none());
    }

    #[test]
    fn uses_the_same_base64url_server_key_as_the_node_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::default().with_app_data_dir(directory.path());
        assert_eq!(
            store
                .credentials_path("https://www.overleaf.com")
                .unwrap()
                .file_name()
                .unwrap(),
            "aHR0cHM6Ly93d3cub3ZlcmxlYWYuY29tLw.json"
        );
    }
}
