//! A hot name-reference column; string bytes remain shared with file records.
use crate::{
    bitmap_builder::RunBitmapBuilder,
    index_store::EntryTable,
    query::{Field, Query, Term},
};
use memchr::memmem::Finder;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) enum NameExpression<'a> {
    All,
    Text(&'a Finder<'static>),
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
}

impl<'a> NameExpression<'a> {
    pub(crate) fn compile(query: &'a Query) -> Option<Self> {
        match query {
            Query::All => Some(Self::All),
            Query::Term(Term::Text {
                field: Field::Name,
                sensitive: false,
                finder,
                ..
            }) => Some(Self::Text(finder)),
            Query::And(children) => Self::combine(children, true),
            Query::Or(children) => Self::combine(children, false),
            Query::Not(child) => Self::compile(child).map(|child| Self::Not(Box::new(child))),
            _ => None,
        }
    }

    fn combine(children: &'a [Query], conjunction: bool) -> Option<Self> {
        let mut compiled = children
            .iter()
            .map(Self::compile)
            .collect::<Option<Vec<_>>>()?;
        // A AND A and A OR A are idempotent. Compare the actual normalized
        // matcher bytes; never merge different fields, case modes, or operators.
        let mut literals = std::collections::HashSet::new();
        compiled.retain(|child| match child {
            Self::Text(finder) => literals.insert((*finder).needle()),
            _ => true,
        });
        if compiled.len() == 1 {
            return compiled.pop();
        }
        Some(if conjunction {
            Self::And(compiled)
        } else {
            Self::Or(compiled)
        })
    }

    fn matches(&self, name: &[u8]) -> bool {
        match self {
            Self::All => true,
            Self::Text(finder) => finder.find(name).is_some(),
            Self::And(children) => children.iter().all(|child| child.matches(name)),
            Self::Or(children) => children.iter().any(|child| child.matches(name)),
            Self::Not(child) => !child.matches(name),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct NameColumn {
    names: EntryTable,
}
impl NameColumn {
    pub(crate) fn inventory_directory(&self, inventory: &mut crate::memory_inventory::Inventory) {
        // The chunk payloads are the same allocations visited by EntryTable.
        inventory.record(
            "name_directory_capacity_bytes",
            self as *const _ as usize,
            self.names.chunks.capacity()
                * std::mem::size_of::<std::sync::Arc<crate::entry_table::EntryChunk>>(),
        );
    }
    pub(crate) fn build(entries: &EntryTable) -> Self {
        Self {
            names: entries.clone(),
        }
    }

    pub(crate) fn evaluate(
        &self,
        query: &NameExpression<'_>,
        exclusions: &[NameExpression<'_>],
        candidates: &RoaringBitmap,
        cancelled: &AtomicBool,
    ) -> Result<RoaringBitmap, String> {
        // Below this grain size, task dispatch costs more than the scan.
        // Count actual candidates, not the span of their possibly sparse IDs.
        const MIN_PARTITION_CANDIDATES: u64 = 32_768;
        let workers = (candidates.len() / MIN_PARTITION_CANDIDATES) as usize;
        if workers > 1
            && let Some(permit) = crate::cpu_executor::acquire(workers)
        {
            return permit.run(|| {
                self.evaluate_partitioned(
                    query,
                    exclusions,
                    candidates,
                    permit.workers(),
                    cancelled,
                )
            });
        }
        self.evaluate_slots(query, exclusions, candidates.iter(), cancelled)
    }

    fn evaluate_partitioned(
        &self,
        query: &NameExpression<'_>,
        exclusions: &[NameExpression<'_>],
        candidates: &RoaringBitmap,
        workers: usize,
        cancelled: &AtomicBool,
    ) -> Result<RoaringBitmap, String> {
        if workers <= 1 || candidates.is_empty() {
            return self.evaluate_slots(query, exclusions, candidates.iter(), cancelled);
        }
        let workers = workers.min(candidates.len() as usize);
        let boundaries: Vec<_> = (0..workers)
            .map(|worker| {
                candidates
                    .select((candidates.len() * worker as u64 / workers as u64) as u32)
                    .unwrap()
            })
            .collect();
        let partition = |worker: usize| match boundaries.get(worker + 1) {
            Some(&next) => candidates.range(boundaries[worker]..next),
            None => candidates.range(boundaries[worker]..),
        };
        (0..workers)
            .into_par_iter()
            .map(|worker| self.evaluate_slots(query, exclusions, partition(worker), cancelled))
            .try_reduce(RoaringBitmap::new, |mut result, part| {
                result |= part;
                Ok(result)
            })
    }

    fn evaluate_slots(
        &self,
        query: &NameExpression<'_>,
        exclusions: &[NameExpression<'_>],
        candidates: impl Iterator<Item = u32>,
        cancelled: &AtomicBool,
    ) -> Result<RoaringBitmap, String> {
        let mut matched = RunBitmapBuilder::default();
        let mut candidates = candidates.peekable();
        while let Some(&first) = candidates.peek() {
            let block_index = first as usize / crate::entry_table::CHUNK_LENGTH;
            let block = &self.names.chunks[block_index];
            let references = block.search_name.values();
            let text = block.text.text().as_bytes();
            let end = (block_index + 1) * crate::entry_table::CHUNK_LENGTH;
            while candidates.peek().is_some_and(|&slot| (slot as usize) < end) {
                let slot = candidates.next().unwrap();
                if cancelled.load(Ordering::Relaxed) {
                    return Err("Query cancelled".into());
                }
                let reference = references[slot as usize % crate::entry_table::CHUNK_LENGTH];
                let name = &text
                    [reference.offset as usize..(reference.offset + reference.length) as usize];
                if query.matches(name)
                    && !exclusions.iter().any(|exclusion| exclusion.matches(name))
                {
                    matched.insert(slot);
                }
            }
        }
        Ok(matched.finish())
    }
}

pub(crate) fn match_paths(
    entries: &EntryTable,
    finder: &Finder<'_>,
    mode: crate::query::PathTextMode,
    live: &RoaringBitmap,
    cancelled: &AtomicBool,
) -> Result<RoaringBitmap, String> {
    let mut result = RunBitmapBuilder::default();
    let mut parents = std::collections::HashMap::new();
    let boundary = finder
        .needle()
        .iter()
        .rposition(|&byte| byte == b'/')
        .map(|index| index + 1);
    for (index, block) in entries.chunks.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Query cancelled".into());
        }
        parents.clear();
        let references = match mode {
            crate::query::PathTextMode::Search => block.search_path.values(),
            crate::query::PathTextMode::Folded => block.folded_path.values(),
            crate::query::PathTextMode::Sensitive { .. } => block.path.values(),
        };
        let start = index * crate::entry_table::CHUNK_LENGTH;
        let mut previous = None;
        for slot in live.range(start as u32..(start + block.len()) as u32) {
            let reference = references[slot as usize - start];
            let parent_state = match previous {
                Some((prefix, state)) if prefix == reference.prefix => state,
                _ => {
                    let state = *parents.entry(reference.prefix).or_insert_with(|| {
                        let text = block.text_at(reference.prefix);
                        let normalized = mode.normalize(text);
                        let prefix = normalized.as_bytes();
                        if finder.find(prefix).is_some() {
                            2u8
                        } else if boundary
                            .is_none_or(|split| prefix.ends_with(&finder.needle()[..split]))
                        {
                            1
                        } else {
                            0
                        }
                    });
                    previous = Some((reference.prefix, state));
                    state
                }
            };
            let suffix_matches = || {
                let suffix = block.text_at(reference.suffix);
                let normalized = mode.normalize(suffix);
                match boundary {
                    Some(split) => normalized.as_bytes().starts_with(&finder.needle()[split..]),
                    None => finder.find(normalized.as_bytes()).is_some(),
                }
            };
            if parent_state == 2 || (parent_state == 1 && suffix_matches()) {
                result.insert(slot);
            }
        }
    }
    Ok(result.finish())
}

#[cfg(test)]
#[path = "name_parallel_tests.rs"]
mod parallel_tests;
