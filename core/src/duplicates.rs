//! Duplicate results describe freshly verified regular-file objects. Directory
//! entries (hard-link aliases) are reported separately from independent objects.
use crate::{index_store::SearchSnapshot, query};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{File, Metadata, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::file_identity::FileIdentity as Identity;
#[derive(Clone, Debug, Serialize)]
struct Row {
    path: String,
    name: String,
    #[serde(flatten)]
    identity: Identity,
}
struct Object {
    identity: Identity,
    rows: Vec<Row>,
    hash: Option<String>,
    clone_id: Option<u64>,
}
#[derive(Debug)]
enum Failure {
    Cancelled,
    File(String),
}
impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::File(error.to_string())
    }
}
fn check_cancel(cancelled: &AtomicBool) -> Result<(), Failure> {
    if cancelled.load(Ordering::Relaxed) {
        Err(Failure::Cancelled)
    } else {
        Ok(())
    }
}
fn ensure_regular(meta: &Metadata) -> Result<(), Failure> {
    if !meta.file_type().is_file() {
        return Err(Failure::File("Path is not a regular file".into()));
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        if meta.st_flags() & 0x4000_0000 != 0 {
            return Err(Failure::File(
                "Cloud placeholder skipped without reading its content".into(),
            ));
        }
    }
    Ok(())
}
fn fresh_identity(path: &str) -> Result<Identity, Failure> {
    let meta = std::fs::symlink_metadata(path)?;
    // Names and sizes remain available for placeholders without reading them.
    if !meta.file_type().is_file() {
        return Err(Failure::File("Path is not a regular file".into()));
    }
    Ok(Identity::from_metadata(&meta))
}
fn validate_path(row: &Row) -> Result<(), Failure> {
    if fresh_identity(&row.path)? != row.identity {
        return Err(Failure::File(
            "File identity or contents changed during duplicate detection; retry".into(),
        ));
    }
    Ok(())
}
fn validate_descriptor(file: &File, expected: &Identity) -> Result<(), Failure> {
    let meta = file.metadata()?;
    ensure_regular(&meta)?;
    if Identity::from_metadata(&meta) != *expected {
        return Err(Failure::File(
            "Opened file no longer matches the verified path; retry".into(),
        ));
    }
    Ok(())
}
fn open_verified(row: &Row) -> Result<File, Failure> {
    // Check placeholder state before open as well as on the returned descriptor.
    let path_meta = std::fs::symlink_metadata(&row.path)?;
    ensure_regular(&path_meta)?;
    if Identity::from_metadata(&path_meta) != row.identity {
        return Err(Failure::File("Path changed before hashing; retry".into()));
    }
    #[cfg(target_os = "macos")]
    let flags = libc::O_NOFOLLOW_ANY | libc::O_NONBLOCK;
    #[cfg(not(target_os = "macos"))]
    let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(&row.path)?;
    validate_descriptor(&file, &row.identity)?;
    Ok(file)
}
/// Accepting an already opened descriptor separates identity acquisition from
/// reading. Both phases validate it, and the final pathname check catches atomic
/// rename/replacement while an old descriptor still refers to unchanged bytes.
fn hash_and_validate(
    file: &mut File,
    row: &Row,
    cancelled: &AtomicBool,
    buffer: &mut [u8],
) -> Result<String, Failure> {
    check_cancel(cancelled)?;
    validate_descriptor(file, &row.identity)?;
    let mut hash = blake3::Hasher::new();
    let mut read = 0u64;
    loop {
        check_cancel(cancelled)?;
        // One extra byte detects growth; an appending writer cannot extend this
        // operation indefinitely by keeping EOF ahead of the reader.
        let limit = buffer
            .len()
            .min(row.identity.size.saturating_sub(read).saturating_add(1) as usize);
        let count = file.read(&mut buffer[..limit])?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > row.identity.size {
            return Err(Failure::File("File grew while hashing; retry".into()));
        }
        hash.update(&buffer[..count]);
    }
    check_cancel(cancelled)?;
    if read != row.identity.size {
        return Err(Failure::File("File shrank while hashing; retry".into()));
    }
    validate_descriptor(file, &row.identity)?;
    validate_path(row)?;
    Ok(hash.finalize().to_hex().to_string())
}
fn record_error(
    errors: &mut Vec<Value>,
    path: &str,
    stage: &str,
    failure: Failure,
) -> Result<(), String> {
    match failure {
        Failure::Cancelled => Err("Duplicate search cancelled".into()),
        Failure::File(message) => {
            errors.push(json!({"path":path,"stage":stage,"message":message}));
            Ok(())
        }
    }
}
fn poll(cancelled: &AtomicBool) -> Result<(), String> {
    check_cancel(cancelled).map_err(|_| "Duplicate search cancelled".to_owned())
}

pub(crate) fn find(
    snapshot: &SearchSnapshot,
    mode: &str,
    cancelled: &AtomicBool,
) -> Result<Value, String> {
    if !["content", "name", "size"].contains(&mode) {
        return Err("Duplicate mode must be content, name, or size".into());
    }
    let mut errors = Vec::new();
    let mut objects: Vec<Object> = Vec::new();
    let mut identities: HashMap<(u64, u64), usize> = HashMap::new();
    let mut examined_paths = 0usize;
    // Re-stat every indexed regular-file candidate BEFORE grouping by size.
    // Stale index size buckets otherwise miss duplicates after a file changes.
    for entry in snapshot.visible_entries() {
        poll(cancelled)?;
        if entry.is_dir || entry.is_symlink {
            continue;
        }
        examined_paths += 1;
        match fresh_identity(&entry.path) {
            Ok(identity) => {
                let row = Row {
                    path: entry.path.clone(),
                    name: entry.name.clone(),
                    identity: identity.clone(),
                };
                let key = (identity.device_id, identity.file_id);
                if let Some(index) = identities.get(&key) {
                    objects[*index].rows.push(row);
                } else {
                    identities.insert(key, objects.len());
                    objects.push(Object {
                        identity,
                        rows: vec![row],
                        hash: None,
                        clone_id: None,
                    });
                }
            }
            Err(error) => record_error(&mut errors, &entry.path, "metadata", error)?,
        }
    }
    let fresh_objects = objects.len();
    let mut size_groups: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (index, object) in objects.iter().enumerate() {
        size_groups
            .entry(object.identity.size)
            .or_default()
            .push(index);
    }
    let mut hash_read_count = 0usize;
    let mut buffer = vec![0u8; 1024 * 1024];
    if mode == "content" {
        for indexes in size_groups.values().filter(|g| g.len() > 1) {
            for &index in indexes {
                poll(cancelled)?;
                let object = &mut objects[index];
                // An inode changing between alias metadata reads is unstable;
                // no alias may borrow a digest from a different file version.
                if object.rows.iter().any(|r| r.identity != object.identity) {
                    for row in &object.rows {
                        record_error(
                            &mut errors,
                            &row.path,
                            "hash",
                            Failure::File(
                                "Hard-link aliases changed during metadata collection; retry"
                                    .into(),
                            ),
                        )?;
                    }
                    continue;
                }
                let row = &object.rows[0];
                let result = (|| {
                    check_cancel(cancelled)?;
                    let mut file = open_verified(row)?;
                    hash_read_count += 1;
                    let digest = hash_and_validate(&mut file, row, cancelled, &mut buffer)?;
                    let clone_id = crate::clone_identity::read(&file);
                    validate_descriptor(&file, &row.identity)?;
                    validate_path(row)?;
                    Ok::<_, Failure>((digest, clone_id))
                })();
                match result {
                    Ok((hash, clone_id)) => {
                        object.hash = Some(hash);
                        object.clone_id = clone_id;
                    }
                    Err(error) => record_error(&mut errors, &row.path, "hash", error)?,
                }
            }
        }
    }
    // A file hashed early may change while a later file is read. Verify every
    // reported alias again immediately before constructing the final groups.
    for object in &mut objects {
        let mut valid = Vec::new();
        for row in object.rows.drain(..) {
            poll(cancelled)?;
            if row.identity != object.identity {
                record_error(
                    &mut errors,
                    &row.path,
                    "final_validation",
                    Failure::File("Hard-link alias metadata changed; retry".into()),
                )?;
                continue;
            }
            match validate_path(&row) {
                Ok(()) => valid.push(row),
                Err(error) => record_error(&mut errors, &row.path, "final_validation", error)?,
            }
        }
        object.rows = valid;
    }
    let mut hardlinks = Vec::new();
    let mut grouped: BTreeMap<String, Vec<&Object>> = BTreeMap::new();
    let mut valid_paths = 0usize;
    for object in &mut objects {
        poll(cancelled)?;
        object.rows.sort_by(|a, b| a.path.cmp(&b.path));
        valid_paths += object.rows.len();
        if object.rows.is_empty() {
            continue;
        }
        if object.rows.len() > 1 {
            hardlinks.push(json!({"kind":"hardlinks","rows":object.rows,"distinct_files":1}));
        }
        match mode {
            "content" => {
                if let Some(hash) = &object.hash {
                    grouped.entry(hash.clone()).or_default().push(object);
                }
            }
            "size" => grouped
                .entry(object.identity.size.to_string())
                .or_default()
                .push(object),
            "name" => {
                let mut keys = std::collections::HashSet::new();
                for row in &object.rows {
                    keys.insert(query::fold(&row.name));
                }
                for key in keys {
                    grouped.entry(key).or_default().push(object);
                }
            }
            _ => unreachable!(),
        }
    }
    let mut groups = Vec::new();
    for (key, members) in grouped.into_iter().filter(|(_, objects)| objects.len() > 1) {
        poll(cancelled)?;
        let mut rows: Vec<&Row> = members
            .iter()
            .flat_map(|object| object.rows.iter())
            .filter(|row| mode != "name" || query::fold(&row.name) == key)
            .collect();
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        let mut group = json!({"kind":if mode=="content" {"same_content"} else {mode},"rows":rows,"distinct_files":members.len()});
        if mode == "content" {
            group["hash"] = json!(key);
            let mut streams: BTreeMap<(u64, u64), Vec<&Object>> = BTreeMap::new();
            for object in &members {
                if let Some(clone_id) = object.clone_id {
                    streams
                        .entry((object.identity.device_id, clone_id))
                        .or_default()
                        .push(object);
                }
            }
            let clones: Vec<_> = streams.into_iter().filter(|(_, objects)| objects.len() > 1)
                .map(|((device, clone_id), objects)| json!({
                    "device_id": device, "clone_id": clone_id, "distinct_files": objects.len(),
                    "paths": objects.iter().flat_map(|object| object.rows.iter().map(|row| &row.path)).collect::<Vec<_>>()
                })).collect();
            group["clone_relationship"] = json!(if clones.is_empty() {
                "unknown"
            } else {
                "verified_pure_clones"
            });
            group["clone_groups"] = json!(clones);
            group["reclaimable_bytes"] = Value::Null;
        }
        groups.push(group);
    }
    hardlinks.sort_by(|a, b| {
        a["rows"][0]["path"]
            .as_str()
            .cmp(&b["rows"][0]["path"].as_str())
    });
    poll(cancelled)?;
    Ok(
        json!({"groups":groups,"hardlinks":hardlinks,"partial":!errors.is_empty(),"errors":errors,"generation":snapshot.generation,
        "scope":"current regular-file metadata for indexed file paths; new unindexed paths require index reconciliation",
        "examined_paths":examined_paths,"validated_paths":valid_paths,"fresh_objects":fresh_objects,"hash_read_count":hash_read_count}),
    )
}

#[cfg(test)]
#[path = "duplicates_tests.rs"]
mod tests;
