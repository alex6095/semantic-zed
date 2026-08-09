use serde::{Deserialize, Serialize};
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OtError {
    #[error("UTF-16 position {position} is outside a string of length {length}")]
    PositionOutOfBounds { position: usize, length: usize },
    #[error("UTF-16 position {position} splits a surrogate pair")]
    SplitSurrogate { position: usize },
    #[error("operation at UTF-16 position {position} must contain exactly one insert or delete")]
    InvalidShareJsOperation { position: usize },
    #[error("delete at UTF-16 position {position} does not match the authoritative snapshot")]
    DeleteMismatch { position: usize },
    #[error("invalid history-OT scan operation: {0}")]
    InvalidHistoryScan(Value),
    #[error("history-OT operation consumed {consumed} of {length} UTF-16 units")]
    IncompleteHistoryOperation { consumed: usize, length: usize },
    #[error("invalid history-OT snapshot")]
    InvalidHistorySnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareJsOperation {
    pub p: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub i: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d: Option<String>,
}

impl ShareJsOperation {
    pub fn insert(position: usize, text: impl Into<String>) -> Self {
        Self {
            p: position,
            i: Some(text.into()),
            d: None,
        }
    }

    pub fn delete(position: usize, text: impl Into<String>) -> Self {
        Self {
            p: position,
            i: None,
            d: Some(text.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HistoryScan {
    Count(i64),
    Insert(String),
    Object(Value),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryOperation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_operation: Option<Vec<HistoryScan>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_op: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// The number of JavaScript/Overleaf UTF-16 code units in `text`.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

pub(crate) fn byte_index_for_utf16(text: &str, position: usize) -> Result<usize, OtError> {
    let mut utf16_position = 0;
    for (byte_index, character) in text.char_indices() {
        if utf16_position == position {
            return Ok(byte_index);
        }
        let next = utf16_position + character.len_utf16();
        if position < next {
            return Err(OtError::SplitSurrogate { position });
        }
        utf16_position = next;
    }
    if utf16_position == position {
        Ok(text.len())
    } else {
        Err(OtError::PositionOutOfBounds {
            position,
            length: utf16_position,
        })
    }
}

fn utf16_slice(text: &str, start: usize, length: usize) -> Result<&str, OtError> {
    let start_byte = byte_index_for_utf16(text, start)?;
    let end_byte = byte_index_for_utf16(text, start + length)?;
    Ok(&text[start_byte..end_byte])
}

fn decode_latin1_packed_utf8(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    for character in value.chars() {
        let codepoint = character as u32;
        if codepoint > u8::MAX as u32 {
            return None;
        }
        bytes.push(codepoint as u8);
    }
    String::from_utf8(bytes).ok()
}

/// Decode UTF-8 bytes packed into a legacy Socket.IO latin-1 string.
pub fn decode_packed_utf8(value: &str) -> String {
    decode_latin1_packed_utf8(value).unwrap_or_else(|| value.to_owned())
}

/// Apply ShareJS text operations using Overleaf's UTF-16 positions.
pub fn apply_sharejs_operations(
    text: &str,
    operations: &[ShareJsOperation],
) -> Result<String, OtError> {
    let mut content = text.to_owned();
    for operation in operations {
        match (&operation.i, &operation.d) {
            (Some(inserted), None) => {
                let byte_index = byte_index_for_utf16(&content, operation.p)?;
                content.insert_str(byte_index, inserted);
            }
            (None, Some(deleted)) => {
                let mut candidates = vec![deleted.as_str()];
                let decoded = decode_latin1_packed_utf8(deleted);
                if let Some(decoded) = decoded.as_deref()
                    && decoded != deleted
                {
                    candidates.push(decoded);
                }

                let matching = candidates.into_iter().find_map(|candidate| {
                    let length = utf16_len(candidate);
                    utf16_slice(&content, operation.p, length)
                        .ok()
                        .filter(|actual| *actual == candidate)
                        .map(|_| length)
                });
                let Some(deleted_length) = matching else {
                    return Err(OtError::DeleteMismatch {
                        position: operation.p,
                    });
                };
                let start = byte_index_for_utf16(&content, operation.p)?;
                let end = byte_index_for_utf16(&content, operation.p + deleted_length)?;
                content.replace_range(start..end, "");
            }
            _ => {
                return Err(OtError::InvalidShareJsOperation {
                    position: operation.p,
                });
            }
        }
    }
    Ok(content)
}

/// Generate compact ShareJS operations which transform `before` into `after`.
pub fn diff_to_sharejs_operations(before: &str, after: &str) -> Vec<ShareJsOperation> {
    let diff = TextDiff::from_chars(before, after);
    let mut operations = Vec::new();
    let mut position = 0;

    for diff_op in diff.ops() {
        let mut equal = String::new();
        let mut deleted = String::new();
        let mut inserted = String::new();
        for change in diff.iter_changes(diff_op) {
            match change.tag() {
                ChangeTag::Equal => equal.push_str(change.value()),
                ChangeTag::Delete => deleted.push_str(change.value()),
                ChangeTag::Insert => inserted.push_str(change.value()),
            }
        }

        if !equal.is_empty() {
            position += utf16_len(&equal);
            continue;
        }
        if !deleted.is_empty() {
            operations.push(ShareJsOperation::delete(position, deleted));
        }
        if !inserted.is_empty() {
            position += utf16_len(&inserted);
            operations.push(ShareJsOperation::insert(
                position - utf16_len(&inserted),
                inserted,
            ));
        }
    }
    operations
}

fn consume_utf16(content: &str, cursor: &mut usize, count: usize) -> Result<String, OtError> {
    let value = utf16_slice(content, *cursor, count)?.to_owned();
    *cursor += count;
    Ok(value)
}

/// Apply Overleaf history-OT scans without flattening their protocol type.
pub fn apply_history_operations(
    text: &str,
    operations: &[HistoryOperation],
) -> Result<String, OtError> {
    let mut content = text.to_owned();
    for operation in operations {
        if operation.no_op || operation.text_operation.is_none() {
            continue;
        }
        let mut cursor = 0;
        let mut output = String::new();
        for scan in operation.text_operation.as_deref().unwrap_or_default() {
            match scan {
                HistoryScan::Count(count) if *count >= 0 => {
                    output.push_str(&consume_utf16(&content, &mut cursor, *count as usize)?);
                }
                HistoryScan::Count(count) => {
                    let _ = consume_utf16(&content, &mut cursor, count.unsigned_abs() as usize)?;
                }
                HistoryScan::Insert(inserted) => output.push_str(inserted),
                HistoryScan::Object(value) => {
                    let Some(object) = value.as_object() else {
                        return Err(OtError::InvalidHistoryScan(value.clone()));
                    };
                    if let Some(retain) = object.get("r").and_then(Value::as_u64) {
                        output.push_str(&consume_utf16(&content, &mut cursor, retain as usize)?);
                    } else if let Some(inserted) = object.get("i").and_then(Value::as_str) {
                        output.push_str(inserted);
                    } else if let Some(deleted) = object.get("d").and_then(Value::as_i64) {
                        let _ =
                            consume_utf16(&content, &mut cursor, deleted.unsigned_abs() as usize)?;
                    } else {
                        return Err(OtError::InvalidHistoryScan(value.clone()));
                    }
                }
            }
        }
        let length = utf16_len(&content);
        if cursor != length {
            return Err(OtError::IncompleteHistoryOperation {
                consumed: cursor,
                length,
            });
        }
        content = output;
    }
    Ok(content)
}

fn push_count(scans: &mut Vec<HistoryScan>, count: i64) {
    if count == 0 {
        return;
    }
    if let Some(HistoryScan::Count(previous)) = scans.last_mut()
        && previous.signum() == count.signum()
    {
        *previous += count;
        return;
    }
    scans.push(HistoryScan::Count(count));
}

fn push_insert(scans: &mut Vec<HistoryScan>, inserted: String) {
    if inserted.is_empty() {
        return;
    }
    if let Some(HistoryScan::Insert(previous)) = scans.last_mut() {
        previous.push_str(&inserted);
    } else {
        scans.push(HistoryScan::Insert(inserted));
    }
}

/// Generate one content-only history-OT scan operation.
pub fn diff_to_history_operations(before: &str, after: &str) -> Vec<HistoryOperation> {
    if before == after {
        return Vec::new();
    }
    let diff = TextDiff::from_chars(before, after);
    let mut scans = Vec::new();
    for diff_op in diff.ops() {
        let mut equal = String::new();
        let mut deleted = String::new();
        let mut inserted = String::new();
        for change in diff.iter_changes(diff_op) {
            match change.tag() {
                ChangeTag::Equal => equal.push_str(change.value()),
                ChangeTag::Delete => deleted.push_str(change.value()),
                ChangeTag::Insert => inserted.push_str(change.value()),
            }
        }
        if !equal.is_empty() {
            push_count(&mut scans, utf16_len(&equal) as i64);
        } else {
            push_insert(&mut scans, inserted);
            push_count(&mut scans, -(utf16_len(&deleted) as i64));
        }
    }
    vec![HistoryOperation {
        text_operation: Some(scans),
        no_op: false,
    }]
}

/// Materialize a history-OT snapshot while hiding tracked deletion ranges.
pub fn history_snapshot_text(raw: &Value) -> Result<String, OtError> {
    let content = raw
        .get("content")
        .and_then(Value::as_str)
        .ok_or(OtError::InvalidHistorySnapshot)?;
    let mut deleted_ranges = raw
        .get("trackedChanges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|change| change.pointer("/tracking/type").and_then(Value::as_str) == Some("delete"))
        .filter_map(|change| {
            let start = change.pointer("/range/start")?.as_u64()? as usize;
            let end = change.pointer("/range/end")?.as_u64()? as usize;
            (end >= start).then_some((start, end))
        })
        .collect::<Vec<_>>();
    deleted_ranges.sort_unstable_by_key(|range| range.0);

    let mut output = String::new();
    let mut cursor = 0;
    for (start, end) in deleted_ranges {
        if start > cursor {
            output.push_str(utf16_slice(content, cursor, start - cursor)?);
        }
        cursor = cursor.max(end);
    }
    let length = utf16_len(content);
    if cursor < length {
        output.push_str(utf16_slice(content, cursor, length - cursor)?);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn applies_operations_in_emitted_order() {
        let operations = vec![
            ShareJsOperation::insert(5, "-new"),
            ShareJsOperation::delete(9, " beta"),
        ];
        assert_eq!(
            apply_sharejs_operations("alpha beta", &operations).unwrap(),
            "alpha-new"
        );
    }

    #[test]
    fn uses_utf16_positions_for_non_bmp_characters() {
        let operations = vec![
            ShareJsOperation::insert(2, "한"),
            ShareJsOperation::delete(3, "b"),
        ];
        assert_eq!(
            apply_sharejs_operations("😀bc", &operations).unwrap(),
            "😀한c"
        );
        assert_eq!(
            byte_index_for_utf16("😀bc", 1),
            Err(OtError::SplitSurrogate { position: 1 })
        );
    }

    #[test]
    fn decodes_legacy_latin1_packed_utf8() {
        let packed = String::from_utf8("한글".as_bytes().to_vec())
            .unwrap()
            .bytes()
            .map(char::from)
            .collect::<String>();
        assert_eq!(decode_packed_utf8(&packed), "한글");
    }

    #[test]
    fn rejects_a_mismatched_delete() {
        assert_eq!(
            apply_sharejs_operations("abcdef", &[ShareJsOperation::delete(2, "ZZ")]),
            Err(OtError::DeleteMismatch { position: 2 })
        );
    }

    #[test]
    fn generated_sharejs_operations_round_trip_unicode() {
        let before = "alpha 😀 한글 beta";
        let after = "alpha 새 😀 한글 result";
        let operations = diff_to_sharejs_operations(before, after);
        assert_eq!(
            apply_sharejs_operations(before, &operations).unwrap(),
            after
        );
    }

    #[test]
    fn generated_history_operations_round_trip_unicode() {
        let before = "History 😀 draft.\n";
        let after = "History OT 😀 content.\n";
        let operations = diff_to_history_operations(before, after);
        assert_eq!(
            apply_history_operations(before, &operations).unwrap(),
            after
        );
    }

    #[test]
    fn applies_object_history_scans() {
        let operations = vec![HistoryOperation {
            text_operation: Some(vec![
                HistoryScan::Object(json!({ "r": 2 })),
                HistoryScan::Object(json!({ "i": "한" })),
                HistoryScan::Object(json!({ "d": -1 })),
                HistoryScan::Count(1),
            ]),
            no_op: false,
        }];
        assert_eq!(
            apply_history_operations("😀bc", &operations).unwrap(),
            "😀한c"
        );
    }

    #[test]
    fn materializes_history_snapshot_without_tracked_deletes() {
        let snapshot = json!({
            "content": "a😀bc",
            "trackedChanges": [{
                "tracking": { "type": "delete" },
                "range": { "start": 1, "end": 3 }
            }]
        });
        assert_eq!(history_snapshot_text(&snapshot).unwrap(), "abc");
    }
}
