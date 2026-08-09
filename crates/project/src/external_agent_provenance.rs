use anyhow::{Result, bail, ensure};
use collections::HashMap;
use gpui::{App, Global};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const ACTIVE_EDIT_TTL: Duration = Duration::from_secs(5 * 60);
const COMPLETED_EDIT_TTL: Duration = Duration::from_secs(30);

/// The lifecycle edge reported by an external agent hook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalAgentEditPhase {
    Begin,
    End,
}

/// A file and its normalized UTF-8 SHA-256 at one lifecycle edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalAgentEditFile {
    pub path: PathBuf,
    pub sha256: Option<String>,
}

/// A fail-closed edit declaration sent by a CLI agent integration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalAgentEditRequest {
    pub phase: ExternalAgentEditPhase,
    pub agent: String,
    pub task_id: String,
    pub files: Vec<ExternalAgentEditFile>,
}

/// Identity retained when an external disk update is accepted as an agent edit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalAgentEditAttribution {
    pub agent: String,
    pub task_id: String,
    pub path: PathBuf,
}

#[derive(Debug)]
struct EditRecord {
    agent: String,
    task_id: String,
    before_sha256: Option<String>,
    after_sha256: Option<String>,
    completed: bool,
    expires_at: Instant,
}

#[derive(Default)]
struct ExternalAgentProvenance {
    edits_by_path: HashMap<PathBuf, Vec<EditRecord>>,
}

impl Global for ExternalAgentProvenance {}

pub fn init(cx: &mut App) {
    if cx.try_global::<ExternalAgentProvenance>().is_none() {
        cx.set_global(ExternalAgentProvenance::default());
    }
}

/// Registers an agent edit lifecycle edge. An `End` without a matching `Begin` is rejected, so a
/// process cannot retroactively claim an arbitrary filesystem update as its own.
pub fn register_external_agent_edit(
    request: ExternalAgentEditRequest,
    cx: &mut App,
) -> Result<usize> {
    init(cx);
    validate_request(&request)?;
    let now = Instant::now();
    let request = normalize_request(request)?;
    let provenance = cx.global_mut::<ExternalAgentProvenance>();
    provenance.prune(now);
    provenance.register(request, now)
}

/// Returns trusted identity only when an active declaration matches the buffer's pre-edit text.
/// Completed declarations additionally have to match the content currently present on disk.
pub fn match_external_agent_edit(
    path: &Path,
    current_buffer_text: &str,
    cx: &mut App,
) -> Option<ExternalAgentEditAttribution> {
    let now = Instant::now();
    let path = normalize_path(path).ok()?;
    let current_buffer_sha256 = normalized_text_sha256(current_buffer_text);
    cx.try_global::<ExternalAgentProvenance>()?;
    let provenance = cx.global_mut::<ExternalAgentProvenance>();
    provenance.prune(now);
    provenance.match_edit(&path, &current_buffer_sha256)
}

impl ExternalAgentProvenance {
    fn register(&mut self, request: ExternalAgentEditRequest, now: Instant) -> Result<usize> {
        match request.phase {
            ExternalAgentEditPhase::Begin => {
                for file in &request.files {
                    let edits = self.edits_by_path.entry(file.path.clone()).or_default();
                    edits.retain(|edit| {
                        edit.agent != request.agent || edit.task_id != request.task_id
                    });
                    edits.push(EditRecord {
                        agent: request.agent.clone(),
                        task_id: request.task_id.clone(),
                        before_sha256: file.sha256.clone(),
                        after_sha256: None,
                        completed: false,
                        expires_at: now + ACTIVE_EDIT_TTL,
                    });
                }
            }
            ExternalAgentEditPhase::End => {
                for file in &request.files {
                    let matching_edit = self
                        .edits_by_path
                        .get(&file.path)
                        .and_then(|edits| {
                            edits.iter().rev().find(|edit| {
                                edit.agent == request.agent && edit.task_id == request.task_id
                            })
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "no active {} edit for {} in task {}",
                                request.agent,
                                file.path.display(),
                                request.task_id
                            )
                        })?;
                    ensure!(
                        !matching_edit.completed,
                        "the {} edit for {} in task {} is already complete",
                        request.agent,
                        file.path.display(),
                        request.task_id
                    );
                }

                for file in &request.files {
                    let edit = self
                        .edits_by_path
                        .get_mut(&file.path)
                        .and_then(|edits| {
                            edits.iter_mut().rev().find(|edit| {
                                edit.agent == request.agent && edit.task_id == request.task_id
                            })
                        })
                        .expect("matching edits were validated above");
                    edit.after_sha256 = file.sha256.clone();
                    edit.completed = true;
                    edit.expires_at = now + COMPLETED_EDIT_TTL;
                }
            }
        }
        Ok(request.files.len())
    }

    fn match_edit(
        &self,
        path: &Path,
        current_buffer_sha256: &str,
    ) -> Option<ExternalAgentEditAttribution> {
        let edits = self.edits_by_path.get(path)?;
        let disk_sha256 = edits
            .iter()
            .any(|edit| edit.completed)
            .then(|| normalized_utf8_file_sha256(path));

        edits.iter().rev().find_map(|edit| {
            if edit.before_sha256.as_deref() != Some(current_buffer_sha256) {
                return None;
            }
            if edit.completed {
                match (&edit.after_sha256, disk_sha256.as_ref()) {
                    (Some(expected), Some(Some(actual))) if expected == actual => {}
                    (None, _) if !path.exists() => {}
                    _ => return None,
                }
            }
            Some(ExternalAgentEditAttribution {
                agent: edit.agent.clone(),
                task_id: edit.task_id.clone(),
                path: path.to_path_buf(),
            })
        })
    }

    fn prune(&mut self, now: Instant) {
        self.edits_by_path.retain(|_, edits| {
            edits.retain(|edit| edit.expires_at > now);
            !edits.is_empty()
        });
    }
}

fn validate_request(request: &ExternalAgentEditRequest) -> Result<()> {
    ensure!(!request.agent.trim().is_empty(), "agent must not be empty");
    ensure!(request.agent.len() <= 80, "agent name is too long");
    ensure!(
        !request.task_id.trim().is_empty(),
        "task id must not be empty"
    );
    ensure!(request.task_id.len() <= 256, "task id is too long");
    ensure!(!request.files.is_empty(), "at least one file is required");
    for file in &request.files {
        ensure!(file.path.is_absolute(), "agent edit paths must be absolute");
        if let Some(digest) = &file.sha256 {
            ensure!(
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid SHA-256 for {}",
                file.path.display()
            );
        }
    }
    Ok(())
}

fn normalize_request(mut request: ExternalAgentEditRequest) -> Result<ExternalAgentEditRequest> {
    for file in &mut request.files {
        file.path = normalize_path(&file.path)?;
        if let Some(digest) = &mut file.sha256 {
            digest.make_ascii_lowercase();
        }
    }
    Ok(request)
}

fn normalize_path(path: &Path) -> Result<PathBuf> {
    let Some(parent) = path.parent() else {
        bail!("{} has no parent directory", path.display());
    };
    let Some(file_name) = path.file_name() else {
        bail!("{} has no file name", path.display());
    };
    Ok(parent.canonicalize()?.join(file_name))
}

fn normalized_utf8_file_sha256(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    Some(normalized_text_sha256(&text))
}

fn normalized_text_sha256(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    format!("{:x}", Sha256::digest(normalized.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_matches_declared_content_transitions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("paper.tex");
        fs::write(&path, "before\r\n").unwrap();
        let mut provenance = ExternalAgentProvenance::default();
        let now = Instant::now();
        provenance
            .register(
                ExternalAgentEditRequest {
                    phase: ExternalAgentEditPhase::Begin,
                    agent: "codex".into(),
                    task_id: "task-1".into(),
                    files: vec![ExternalAgentEditFile {
                        path: path.clone(),
                        sha256: Some(normalized_text_sha256("before\n")),
                    }],
                },
                now,
            )
            .unwrap();
        fs::write(&path, "after\n").unwrap();
        provenance
            .register(
                ExternalAgentEditRequest {
                    phase: ExternalAgentEditPhase::End,
                    agent: "codex".into(),
                    task_id: "task-1".into(),
                    files: vec![ExternalAgentEditFile {
                        path: path.clone(),
                        sha256: Some(normalized_text_sha256("after\n")),
                    }],
                },
                now,
            )
            .unwrap();

        let attribution = provenance
            .match_edit(&path, &normalized_text_sha256("before\n"))
            .unwrap();
        assert_eq!(attribution.agent, "codex");
        assert_eq!(attribution.task_id, "task-1");
        assert!(
            provenance
                .match_edit(&path, &normalized_text_sha256("different"))
                .is_none()
        );
    }
}
