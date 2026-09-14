//! A hot name-reference column; string bytes remain shared with file records.
use crate::{
    bitmap_builder::RunBitmapBuilder,
    chunked_vec::ChunkedVec,
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
    names: ChunkedVec<crate::shared_text::SharedText>,
}

impl NameColumn {
    pub(crate) fn build(entries: &EntryTable) -> Self {
        Self {
            names: entries
                .iter()
                .map(|entry| entry.search_name.clone())
                .collect(),
        }
    }

    pub(crate) fn set(&mut self, slot: usize, name: &crate::shared_text::SharedText) {
        if slot == self.names.len() {
            self.names.push(name.clone());
        } else if self.names[slot].as_ref() != name.as_ref() {
            self.names[slot] = name.clone();
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
        for slot in candidates {
            if cancelled.load(Ordering::Relaxed) {
                return Err("Query cancelled".into());
            }
            let name = self.names[slot as usize].as_bytes();
            if query.matches(name) && !exclusions.iter().any(|exclusion| exclusion.matches(name)) {
                matched.insert(slot);
            }
        }
        Ok(matched.finish())
    }
}

#[cfg(test)]
#[path = "name_parallel_tests.rs"]
mod parallel_tests;
