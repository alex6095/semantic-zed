use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use thiserror::Error;
use time::OffsetDateTime;

const IGNORED_NAMES: [&str; 5] = [".git", ".zed", ".semantic-zed", "node_modules", ".DS_Store"];

#[derive(Clone, Debug)]
pub struct ScannedFile {
    pub path: String,
    pub absolute_path: PathBuf,
    pub bytes: Vec<u8>,
    pub digest: String,
    pub size: u64,
}

#[derive(Clone, Debug, Default)]
pub struct TreeSnapshot {
    pub files: HashMap<String, ScannedFile>,
    pub directories: HashSet<String>,
    pub errors: Vec<(String, String)>,
}

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("replica scan failed: {0}")]
    Io(#[from] io::Error),
    #[error("path is outside the replica: {0}")]
    OutsideReplica(PathBuf),
    #[error("file changed while it was being read: {0}")]
    Unstable(PathBuf),
}

pub fn scan_tree(root: &Path) -> Result<TreeSnapshot, ScanError> {
    let mut snapshot = TreeSnapshot::default();
    snapshot.directories.insert(String::new());
    scan_directory(root, root, &mut snapshot)?;
    Ok(snapshot)
}

fn scan_directory(
    root: &Path,
    directory: &Path,
    snapshot: &mut TreeSnapshot,
) -> Result<(), ScanError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if IGNORED_NAMES.contains(&name.as_ref()) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let absolute_path = entry.path();
        let relative_path = relative_path(root, &absolute_path)?;
        if file_type.is_dir() {
            snapshot.directories.insert(relative_path);
            scan_directory(root, &absolute_path, snapshot)?;
        } else if file_type.is_file() {
            match stable_read(&absolute_path) {
                Ok((bytes, metadata)) => {
                    snapshot.files.insert(
                        relative_path.clone(),
                        ScannedFile {
                            path: relative_path,
                            absolute_path,
                            digest: digest_bytes(&bytes),
                            size: metadata.len(),
                            bytes,
                        },
                    );
                }
                Err(error) => snapshot.errors.push((relative_path, error.to_string())),
            }
        }
    }
    Ok(())
}

pub fn stable_read(path: &Path) -> Result<(Vec<u8>, Metadata), ScanError> {
    let delays = [0, 25, 100, 300];
    for (attempt, delay) in delays.into_iter().enumerate() {
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay));
        }
        let before_path = fs::metadata(path)?;
        if !before_path.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file").into());
        }
        let mut file = File::open(path)?;
        let before_file = file.metadata()?;
        let mut bytes = Vec::with_capacity(before_file.len() as usize);
        file.read_to_end(&mut bytes)?;
        let after_file = file.metadata()?;
        let after_path = fs::metadata(path)?;
        if same_identity(&before_file, &after_file) && same_identity(&before_path, &after_path) {
            return Ok((bytes, after_path));
        }
        if attempt == delays.len() - 1 {
            break;
        }
    }
    Err(ScanError::Unstable(path.into()))
}

pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), ScanError> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let token = format!(
        "{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let temp = parent.join(format!(".semantic-zed-write-{token}"));
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

pub fn is_text_document_path(path: &str) -> bool {
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    matches!(
        extension.as_deref(),
        Some(
            "tex"
                | "ltx"
                | "bib"
                | "sty"
                | "cls"
                | "bbx"
                | "cbx"
                | "txt"
                | "md"
                | "csv"
                | "json"
                | "yaml"
                | "yml"
                | "py"
                | "r"
                | "jl"
                | "m"
        )
    )
}

pub fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn relative_path(root: &Path, path: &Path) -> Result<String, ScanError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ScanError::OutsideReplica(path.into()))?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn same_identity(left: &Metadata, right: &Metadata) -> bool {
    if left.len() != right.len() || left.modified().ok() != right.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_normal_files_but_not_replica_metadata_or_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("sections")).unwrap();
        fs::create_dir_all(directory.path().join(".semantic-zed")).unwrap();
        fs::write(directory.path().join("main.tex"), "main").unwrap();
        fs::write(directory.path().join("sections/intro.tex"), "intro").unwrap();
        fs::write(directory.path().join(".semantic-zed/private"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("main.tex", directory.path().join("linked.tex")).unwrap();

        let snapshot = scan_tree(directory.path()).unwrap();
        assert_eq!(snapshot.files.len(), 2);
        assert!(snapshot.files.contains_key("main.tex"));
        assert!(snapshot.files.contains_key("sections/intro.tex"));
        assert!(!snapshot.files.contains_key(".semantic-zed/private"));
        assert!(!snapshot.files.contains_key("linked.tex"));
        assert!(snapshot.directories.contains("sections"));
    }

    #[test]
    fn atomic_writes_replace_a_complete_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.tex");
        atomic_write(&path, b"first", 0o644).unwrap();
        atomic_write(&path, b"second", 0o644).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"second");
    }
}
