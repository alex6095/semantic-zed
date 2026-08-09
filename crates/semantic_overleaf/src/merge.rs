const DEFAULT_MAX_MERGE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeResult {
    Merged(String),
    Conflict { reason: &'static str },
}

impl MergeResult {
    pub fn into_merged(self) -> Option<String> {
        match self {
            Self::Merged(text) => Some(text),
            Self::Conflict { .. } => None,
        }
    }
}

pub fn merge_text(base: &str, local: &str, remote: &str) -> MergeResult {
    merge_text_with_limit(base, local, remote, DEFAULT_MAX_MERGE_BYTES)
}

pub fn merge_text_with_limit(
    base: &str,
    local: &str,
    remote: &str,
    max_bytes: usize,
) -> MergeResult {
    if base.len() > max_bytes || local.len() > max_bytes || remote.len() > max_bytes {
        return MergeResult::Conflict {
            reason: "text-too-large",
        };
    }
    if local == remote {
        return MergeResult::Merged(local.into());
    }
    if local == base {
        return MergeResult::Merged(remote.into());
    }
    if remote == base {
        return MergeResult::Merged(local.into());
    }
    match diffy::merge(base, local, remote) {
        Ok(text) => MergeResult::Merged(text),
        Err(_) => MergeResult::Conflict {
            reason: "overlapping-edits",
        },
    }
}

pub fn desired_change_is_present(
    submitted_remote: &str,
    desired: &str,
    authoritative: &str,
) -> bool {
    if desired == authoritative {
        return true;
    }
    matches!(
        merge_text(submitted_remote, desired, authoritative),
        MergeResult::Merged(merged) if merged == authoritative
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_independent_local_and_remote_edits() {
        let base = "title\nbody\nend\n";
        let local = "local title\nbody\nend\n";
        let remote = "title\nbody\nremote end\n";
        assert_eq!(
            merge_text(base, local, remote),
            MergeResult::Merged("local title\nbody\nremote end\n".into())
        );
    }

    #[test]
    fn refuses_overlapping_edits_without_materializing_markers() {
        assert_eq!(
            merge_text("same\n", "local\n", "remote\n"),
            MergeResult::Conflict {
                reason: "overlapping-edits"
            }
        );
    }

    #[test]
    fn detects_a_desired_change_inside_a_newer_authoritative_snapshot() {
        assert!(desired_change_is_present(
            "a\nb\nc\n",
            "local\nb\nc\n",
            "local\nb\nremote\n"
        ));
    }
}
