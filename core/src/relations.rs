//! Snapshot-local directory topology and aggregate queries. No filesystem I/O.
//! A dense parent column and a directory-only postorder avoid one subtree scan
//! per directory. Hard-link directory entries contribute logical size separately.
use crate::{
    index_store::{IndexedFile, SearchSnapshot},
    query::{Query, Term},
};
use roaring::RoaringTreemap;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const NO_PARENT: u32 = u32::MAX;

#[derive(Debug)]
pub struct ResolvedPredicate {
    pub(crate) yes: RoaringTreemap,
    pub(crate) unknown: RoaringTreemap,
}
impl ResolvedPredicate {
    pub(crate) fn truth(&self, file: &IndexedFile) -> Option<bool> {
        let id = file.id as u64;
        if self.yes.contains(id) {
            Some(true)
        } else if self.unknown.contains(id) {
            None
        } else {
            Some(false)
        }
    }
}

pub(crate) fn is_metric(field: &str) -> bool {
    matches!(
        field,
        "foldersize"
            | "childcount"
            | "childfilecount"
            | "childfoldercount"
            | "descendantcount"
            | "descendantfilecount"
            | "descendantfoldercount"
    )
}

#[derive(Default)]
pub(crate) struct Hierarchy {
    parents: Vec<u32>,
    directory_slots: Vec<u32>,
    postorder: Vec<usize>,
    own_files: Vec<u64>,
    own_folders: Vec<u64>,
    all_files: Vec<u64>,
    all_folders: Vec<u64>,
    sizes: Vec<Option<u64>>,
}
impl Hierarchy {
    pub(crate) fn build(snapshot: &SearchSnapshot, cancelled: &AtomicBool) -> Result<Self, String> {
        let mut tree = Self {
            parents: vec![NO_PARENT; snapshot.entries.len()],
            ..Self::default()
        };
        for slot in &snapshot.live {
            check(cancelled)?;
            if snapshot.entries[slot as usize].is_dir && !snapshot.entries[slot as usize].is_symlink
            {
                tree.directory_slots.push(slot);
            }
        }
        let directories: HashMap<&str, usize> = tree
            .directory_slots
            .iter()
            .enumerate()
            .map(|(index, slot)| (snapshot.entries[*slot as usize].path.as_str(), index))
            .collect();
        let count = directories.len();
        tree.own_files = vec![0; count];
        tree.own_folders = vec![0; count];
        tree.all_files = vec![0; count];
        tree.all_folders = vec![0; count];
        tree.sizes = vec![Some(0); count];
        for slot in &snapshot.live {
            check(cancelled)?;
            let file = &snapshot.entries[slot as usize];
            let mut parent = Path::new(&file.path).parent();
            let direct = parent;
            // Missing intermediate directory rows are possible in imported or
            // incomplete indexes. Ancestors can still receive positive evidence.
            while let Some(path) = parent {
                if let Some(&index) = path.to_str().and_then(|path| directories.get(path)) {
                    tree.parents[slot as usize] = index as u32;
                    let is_direct = Some(path) == direct;
                    if file.is_dir && !file.is_symlink {
                        tree.all_folders[index] += 1;
                        tree.own_folders[index] += u64::from(is_direct);
                    } else {
                        tree.all_files[index] += 1;
                        tree.own_files[index] += u64::from(is_direct);
                        tree.sizes[index] =
                            tree.sizes[index].and_then(|size| size.checked_add(file.size));
                    }
                    break;
                }
                parent = path.parent();
            }
        }
        tree.postorder = (0..count).collect();
        // Parent paths are strictly shorter, including non-ASCII names. Sorting
        // directory indexes by length is a stack-safe topological order.
        tree.postorder.sort_unstable_by_key(|index| {
            std::cmp::Reverse(
                snapshot.entries[tree.directory_slots[*index] as usize]
                    .path
                    .len(),
            )
        });
        for &index in &tree.postorder {
            check(cancelled)?;
            let parent = tree.parents[tree.directory_slots[index] as usize];
            if parent != NO_PARENT {
                let parent = parent as usize;
                tree.all_files[parent] += tree.all_files[index];
                tree.all_folders[parent] += tree.all_folders[index];
                tree.sizes[parent] = tree.sizes[parent]
                    .zip(tree.sizes[index])
                    .and_then(|(a, b)| a.checked_add(b));
            }
        }
        Ok(tree)
    }

    fn incomplete(
        &self,
        snapshot: &SearchSnapshot,
        coverage: &Value,
        cancelled: &AtomicBool,
    ) -> Result<Vec<bool>, String> {
        let complete = coverage["complete"].as_bool().unwrap_or(false);
        let roots: std::collections::HashSet<&str> = coverage["roots"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let gaps: std::collections::HashSet<&str> = coverage["uncovered"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let mut gap_ancestors = std::collections::HashSet::new();
        for gap in &gaps {
            check(cancelled)?;
            for ancestor in Path::new(gap).ancestors().filter_map(Path::to_str) {
                gap_ancestors.insert(ancestor);
            }
        }
        let mut uncertain = Vec::with_capacity(self.directory_slots.len());
        for &slot in &self.directory_slots {
            check(cancelled)?;
            let path = Path::new(&snapshot.entries[slot as usize].path);
            let covered = path
                .ancestors()
                .filter_map(Path::to_str)
                .any(|path| roots.contains(path));
            let denied = gap_ancestors.contains(snapshot.entries[slot as usize].path.as_str())
                || path
                    .ancestors()
                    .filter_map(Path::to_str)
                    .any(|path| gaps.contains(path));
            uncertain.push(!complete || !covered || denied);
        }
        Ok(uncertain)
    }

    pub(crate) fn info(
        &self,
        snapshot: &SearchSnapshot,
        coverage: &Value,
        paths: &[String],
        cancelled: &AtomicBool,
    ) -> Result<Value, String> {
        let incomplete = self.incomplete(snapshot, coverage, cancelled)?;
        let requested: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
        let mut found = HashMap::new();
        for (index, &slot) in self.directory_slots.iter().enumerate() {
            check(cancelled)?;
            let file = &snapshot.entries[slot as usize];
            if !requested.contains(file.path.as_str()) {
                continue;
            }
            let complete = !incomplete[index] && self.sizes[index].is_some();
            found.insert(file.path.as_str(), json!({"path":file.path, "complete":complete,
                "recursive_size":if complete { self.sizes[index] } else { None },
                "indexed_logical_size":self.sizes[index], "child_count":self.own_files[index]+self.own_folders[index],
                "descendant_count":self.all_files[index]+self.all_folders[index]}));
        }
        let rows: Vec<_> = paths.iter().map(|path| found.get(path.as_str()).cloned().unwrap_or_else(||
            json!({"path":path,"complete":false,"recursive_size":null,"reason":"not_an_indexed_directory"}))).collect();
        Ok(
            json!({"rows":rows,"generation":snapshot.generation,"size_semantics":"logical directory-entry sum; not physical or reclaimable bytes"}),
        )
    }
}

fn check(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("Query cancelled".into())
    } else {
        Ok(())
    }
}

pub(crate) fn resolve(
    query: &mut Query,
    snapshot: &SearchSnapshot,
    tree: &Hierarchy,
    coverage: &Value,
    cancelled: &AtomicBool,
    content: &mut impl FnMut(&IndexedFile) -> Result<Option<String>, String>,
) -> Result<bool, String> {
    check(cancelled)?;
    match query {
        Query::And(children) | Query::Or(children) => {
            let mut partial = false;
            for child in children {
                partial |= resolve(child, snapshot, tree, coverage, cancelled, content)?;
            }
            Ok(partial)
        }
        Query::Not(child) => resolve(child, snapshot, tree, coverage, cancelled, content),
        Query::Term(Term::Related {
            recursive,
            query: inner,
        }) => {
            let mut partial = resolve(inner, snapshot, tree, coverage, cancelled, content)?;
            let incomplete = tree.incomplete(snapshot, coverage, cancelled)?;
            let mut positive = vec![false; tree.directory_slots.len()];
            let mut unknown = incomplete;
            let needs_content = inner.requires_content();
            for slot in &snapshot.live {
                check(cancelled)?;
                let parent = tree.parents[slot as usize];
                if parent == NO_PARENT {
                    continue;
                }
                let parent = parent as usize;
                let file = &snapshot.entries[slot as usize];
                if !*recursive
                    && file.parent.as_ref()
                        != snapshot.entries[tree.directory_slots[parent] as usize].path
                {
                    continue;
                }
                let body = if needs_content { content(file)? } else { None };
                match inner.indexed_truth(file, body.as_deref())? {
                    Some(true) => positive[parent] = true,
                    None => unknown[parent] = true,
                    Some(false) => (),
                }
            }
            if *recursive {
                for &index in &tree.postorder {
                    check(cancelled)?;
                    let parent = tree.parents[tree.directory_slots[index] as usize];
                    if parent != NO_PARENT {
                        positive[parent as usize] |= positive[index];
                        unknown[parent as usize] |= unknown[index];
                    }
                }
            }
            let mut result = ResolvedPredicate {
                yes: RoaringTreemap::new(),
                unknown: RoaringTreemap::new(),
            };
            for (index, &slot) in tree.directory_slots.iter().enumerate() {
                let id = snapshot.entries[slot as usize].id as u64;
                if positive[index] {
                    result.yes.insert(id);
                } else if unknown[index] {
                    result.unknown.insert(id);
                }
            }
            partial |= !result.unknown.is_empty();
            *query = Query::Term(Term::Resolved(result));
            Ok(partial)
        }
        Query::Term(term @ (Term::Number { .. } | Term::Unknown { .. })) => {
            let (field, is_unknown) = match term {
                Term::Number { field, .. } => (field.clone(), false),
                Term::Unknown { field, .. } => (field.clone(), true),
                _ => unreachable!(),
            };
            if !is_metric(&field) {
                return Ok(false);
            }
            let incomplete = tree.incomplete(snapshot, coverage, cancelled)?;
            let mut result = ResolvedPredicate {
                yes: RoaringTreemap::new(),
                unknown: RoaringTreemap::new(),
            };
            for (index, &slot) in tree.directory_slots.iter().enumerate() {
                check(cancelled)?;
                let value = match field.as_str() {
                    "foldersize" => tree.sizes[index],
                    "childcount" => Some(tree.own_files[index] + tree.own_folders[index]),
                    "childfilecount" => Some(tree.own_files[index]),
                    "childfoldercount" => Some(tree.own_folders[index]),
                    "descendantcount" => Some(tree.all_files[index] + tree.all_folders[index]),
                    "descendantfilecount" => Some(tree.all_files[index]),
                    "descendantfoldercount" => Some(tree.all_folders[index]),
                    _ => unreachable!(),
                }
                .filter(|_| !incomplete[index])
                .map(|value| value as f64);
                let id = snapshot.entries[slot as usize].id as u64;
                if is_unknown || value.is_some() {
                    if term.matches_numeric_value(value) {
                        result.yes.insert(id);
                    }
                } else {
                    result.unknown.insert(id);
                }
            }
            let partial = !result.unknown.is_empty();
            *query = Query::Term(Term::Resolved(result));
            Ok(partial)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
#[path = "relations_tests.rs"]
mod tests;
