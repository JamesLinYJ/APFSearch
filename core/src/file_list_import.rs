//! Bounded offline imports. A temporary SQL table keeps failed or unfinished
//! imports out of both live queries and persistent snapshots.
use crate::{scanner::ScannedFile, RequestGuard, SearchEngine};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub(super) const MAX_ROWS: usize = 2_000_000;
const MAX_FIELD_BYTES: usize = 64 * 1024;
const MAX_TEXT_BYTES: usize = 256 * 1024 * 1024;
const BATCH_ROWS: usize = 4096;
const BATCH_BYTES: usize = 1024 * 1024;
const LIMIT: &str = "The file list exceeds the supported import limits.";

pub(super) struct ImportState {
    id: String,
    rows: usize,
    text_bytes: usize,
    cancelled: Arc<AtomicBool>,
}

fn strings(row: &Value) -> Result<(&str, &str, String), String> {
    let path = row["path"]
        .as_str()
        .ok_or("Every imported row needs path")?;
    let filename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let name = row["name"].as_str().unwrap_or(filename);
    if path.len() > MAX_FIELD_BYTES || name.len() > MAX_FIELD_BYTES {
        return Err(LIMIT.into());
    }
    let extension = if row["is_dir"].as_bool().unwrap_or(false) {
        String::new()
    } else {
        Path::new(filename)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_lowercase()
    };
    if extension.len() > MAX_FIELD_BYTES {
        return Err(LIMIT.into());
    }
    Ok((path, name, extension))
}
fn validate(
    rows: &[Value],
    prior_rows: usize,
    prior_bytes: usize,
    cancelled: &AtomicBool,
) -> Result<usize, String> {
    if rows.len() > MAX_ROWS.saturating_sub(prior_rows) {
        return Err(LIMIT.into());
    }
    let mut bytes = prior_bytes;
    for row in rows {
        if cancelled.load(Ordering::Acquire) {
            return Err("Query cancelled".into());
        }
        let (path, name, extension) = strings(row)?;
        bytes = bytes
            .checked_add(path.len() + name.len() + extension.len())
            .ok_or(LIMIT)?;
        if bytes > MAX_TEXT_BYTES {
            return Err(LIMIT.into());
        }
    }
    Ok(bytes)
}
fn entries(rows: &[Value], offset: usize) -> Result<Vec<ScannedFile>, String> {
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let (path, name, extension) = strings(row)?;
            Ok(ScannedFile {
                path: path.into(),
                name: name.into(),
                extension,
                size: row["size"].as_u64().unwrap_or(0),
                modified: row["modified"].as_i64().unwrap_or(0),
                created: row["created"].as_i64().unwrap_or(0),
                is_dir: row["is_dir"].as_bool().unwrap_or(false),
                file_id: (offset + index + 1) as u64,
                volume_id: "offline:filelist".into(),
                ..Default::default()
            })
        })
        .collect()
}
impl SearchEngine {
    fn complete_file_list_import(&self, count: usize) -> Result<Value, String> {
        self.index_store.lock().unwrap().finish_file_list_import()?;
        self.offline.store(true, Ordering::Release);
        {
            let mut state = self.state.lock().unwrap();
            state["offline"] = json!(true);
            state["state"] = json!("offline");
        }
        // Only the completed table is published; batches never build snapshots.
        self.refresh(true)?;
        Ok(json!({"imported":count,"offline":true}))
    }
    pub(super) fn import_file_list(&self, request: &Value) -> Result<Value, String> {
        let _restart = self.restart_lock.lock().unwrap();
        let pending = self.file_list_import.lock().unwrap();
        if self.active.load(Ordering::SeqCst) || pending.is_some() {
            return Err("Use a separate offline database for imported file lists".into());
        }
        let rows = request["rows"].as_array().ok_or("rows must be an array")?;
        let id = request["request_id"].as_str().map(str::to_owned);
        if id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 128)
        {
            return Err("Invalid import request_id".into());
        }
        let token = Arc::new(AtomicBool::new(false));
        if let Some(id) = &id {
            if let Some(previous) = self
                .requests
                .lock()
                .unwrap()
                .insert(id.clone(), token.clone())
            {
                previous.store(true, Ordering::Release);
            }
        }
        let _guard = RequestGuard {
            requests: &self.requests,
            id,
            token: token.clone(),
        };
        // Check the entire ordinary API request before changing even staging SQL.
        validate(rows, 0, 0, &token)?;
        self.index_store.lock().unwrap().begin_file_list_import()?;
        let result = (|| {
            for (index, batch) in rows.chunks(BATCH_ROWS).enumerate() {
                if token.load(Ordering::Acquire) {
                    return Err("Query cancelled".into());
                }
                // Bound converted metadata by bytes as well as row count.
                let mut start = 0;
                while start < batch.len() {
                    let mut end = start;
                    let mut bytes = 0;
                    while end < batch.len() {
                        let (path, name, extension) = strings(&batch[end])?;
                        let next = path.len() + name.len() + extension.len();
                        if end > start && bytes + next > BATCH_BYTES {
                            break;
                        }
                        bytes += next;
                        end += 1;
                    }
                    if token.load(Ordering::Acquire) {
                        return Err("Query cancelled".into());
                    }
                    let files = entries(&batch[start..end], index * BATCH_ROWS + start)?;
                    self.index_store
                        .lock()
                        .unwrap()
                        .append_file_list_import(&files)?;
                    start = end;
                }
            }
            if token.load(Ordering::Acquire) {
                return Err("Query cancelled".into());
            }
            self.complete_file_list_import(rows.len())
        })();
        if result.is_err() {
            let _ = self.index_store.lock().unwrap().abort_file_list_import();
        }
        result
    }
    pub(super) fn staged_file_list_import(
        &self,
        request: &Value,
        phase: &str,
    ) -> Result<Value, String> {
        let id = request["request_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or("Invalid import request_id")?;
        let _restart = self.restart_lock.lock().unwrap();
        let mut pending = self.file_list_import.lock().unwrap();
        if phase == "begin" {
            if self.active.load(Ordering::SeqCst) || pending.is_some() {
                return Err("Use a separate offline database for imported file lists".into());
            }
            self.index_store.lock().unwrap().begin_file_list_import()?;
            let cancelled = Arc::new(AtomicBool::new(false));
            if let Some(previous) = self
                .requests
                .lock()
                .unwrap()
                .insert(id.into(), cancelled.clone())
            {
                previous.store(true, Ordering::Release);
            }
            *pending = Some(ImportState {
                id: id.into(),
                rows: 0,
                text_bytes: 0,
                cancelled,
            });
            return Ok(json!({"importing":true}));
        }
        let state = pending
            .as_mut()
            .filter(|state| state.id == id)
            .ok_or("No matching file-list import")?;
        let result = (|| {
            if phase == "abort" {
                return Ok(json!({"aborted":true}));
            }
            if state.cancelled.load(Ordering::Acquire) {
                return Err("Query cancelled".into());
            }
            if phase == "finish" {
                return self.complete_file_list_import(state.rows);
            }
            let rows = request["rows"].as_array().ok_or("rows must be an array")?;
            if rows.len() > BATCH_ROWS {
                return Err(LIMIT.into());
            }
            let bytes = validate(rows, state.rows, state.text_bytes, &state.cancelled)?;
            if bytes - state.text_bytes > BATCH_BYTES {
                return Err(LIMIT.into());
            }
            let files = entries(rows, state.rows)?;
            self.index_store
                .lock()
                .unwrap()
                .append_file_list_import(&files)?;
            state.rows += rows.len();
            state.text_bytes = bytes;
            Ok(json!({"imported":state.rows,"importing":true}))
        })();
        if result.is_err() || phase == "finish" || phase == "abort" {
            let state = pending.take().unwrap();
            let _guard = RequestGuard {
                requests: &self.requests,
                id: Some(state.id),
                token: state.cancelled,
            };
            self.index_store.lock().unwrap().abort_file_list_import()?;
        }
        result
    }
    pub(super) fn cancel_file_list_import(&self, id: &str) {
        let mut pending = self.file_list_import.lock().unwrap();
        if pending.as_ref().is_some_and(|state| state.id == id) {
            let state = pending.take().unwrap();
            let _guard = RequestGuard {
                requests: &self.requests,
                id: Some(state.id),
                token: state.cancelled,
            };
            let _ = self.index_store.lock().unwrap().abort_file_list_import();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cumulative_budgets_and_cancel_are_checked_before_allocation() {
        let running = AtomicBool::new(false);
        let rows = [json!({"path":"/tiny.txt"})];
        assert!(validate(&rows, MAX_ROWS, 0, &running).is_err());
        assert!(validate(&rows, 0, MAX_TEXT_BYTES - 1, &running).is_err());
        assert!(validate(&rows, MAX_ROWS - 1, MAX_TEXT_BYTES - 100, &running).is_ok());
        running.store(true, Ordering::Release);
        assert_eq!(
            validate(&rows, 0, 0, &running).unwrap_err(),
            "Query cancelled"
        );
    }
}
