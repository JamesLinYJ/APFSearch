//! Parsed result ordering and cancellable sorting shared by snapshots and queries.
use crate::{index_store::IndexedFile, query};
use serde_json::Value;
use std::{
    cmp::Ordering,
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Field {
    Name,
    Path,
    Extension,
    Size,
    Modified,
    Created,
    Directory,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ResultOrder(Vec<(Field, bool)>);
impl ResultOrder {
    pub(crate) fn parse(value: &Value) -> Result<Self, String> {
        let Some(items) = value.as_array() else {
            return Ok(Self::name());
        };
        items
            .iter()
            .map(|item| {
                let field = match item["field"].as_str().unwrap_or("name") {
                    "name" => Field::Name,
                    "path" => Field::Path,
                    "extension" => Field::Extension,
                    "size" => Field::Size,
                    "modified" => Field::Modified,
                    "created" => Field::Created,
                    "is_dir" => Field::Directory,
                    _ => return Err("Unsupported sort field".into()),
                };
                Ok((field, item["ascending"].as_bool().unwrap_or(true)))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
    pub(crate) fn name() -> Self {
        Self(vec![(Field::Name, true)])
    }
    pub(crate) fn path() -> Self {
        Self(vec![(Field::Path, true)])
    }
    pub(crate) fn primary_path_direction(&self) -> Option<bool> {
        self.0
            .first()
            .and_then(|(field, ascending)| (*field == Field::Path).then_some(*ascending))
    }
    pub(crate) fn affected_by_change(&self, previous: &IndexedFile, current: &IndexedFile) -> bool {
        if previous.path != current.path || previous.id != current.id {
            return true;
        }
        self.0.iter().any(|(field, _)| match field {
            Field::Name => previous.name != current.name,
            Field::Path => false, // checked above; also the fallback for every order
            Field::Extension => previous.extension != current.extension,
            Field::Size => previous.size != current.size,
            Field::Modified => previous.modified != current.modified,
            Field::Created => previous.created != current.created,
            Field::Directory => previous.is_dir != current.is_dir,
        })
    }
    pub(crate) fn compare(&self, first: &IndexedFile, second: &IndexedFile) -> Ordering {
        for (field, ascending) in &self.0 {
            let order = match field {
                Field::Name => query::natural_cmp_folded(&first.folded_name, &second.folded_name),
                Field::Path => query::natural_cmp_folded(&first.folded_path, &second.folded_path),
                Field::Extension => first.extension.cmp(&second.extension),
                Field::Size => first.size.cmp(&second.size),
                Field::Modified => first.modified.cmp(&second.modified),
                Field::Created => first.created.cmp(&second.created),
                Field::Directory => first.is_dir.cmp(&second.is_dir),
            };
            if !order.is_eq() {
                return if *ascending { order } else { order.reverse() };
            }
        }
        first
            .path
            .cmp(&second.path)
            .then_with(|| first.id.cmp(&second.id))
    }
}
/// Sort bounded runs, then merge them. Cancellation never changes comparator
/// ordering and is checked during each merge, rather than waiting for a whole
/// multi-million-row standard-library sort to finish.
pub(crate) fn sort_slots(
    slots: &mut Vec<u32>,
    compare: impl Fn(u32, u32) -> Ordering,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    const RUN: usize = 1024;
    for chunk in slots.chunks_mut(RUN) {
        if cancelled.load(AtomicOrdering::Relaxed) {
            return Err("Query cancelled".into());
        }
        chunk.sort_unstable_by(|a, b| compare(*a, *b));
    }
    if slots.len() <= RUN {
        return Ok(());
    }
    let mut scratch = vec![0; slots.len()];
    let mut width = RUN;
    while width < slots.len() {
        for start in (0..slots.len()).step_by(width.saturating_mul(2)) {
            let middle = (start + width).min(slots.len());
            let end = (middle + width).min(slots.len());
            let (mut left, mut right) = (start, middle);
            for (position, destination) in scratch[start..end].iter_mut().enumerate() {
                if position % RUN == 0 && cancelled.load(AtomicOrdering::Relaxed) {
                    return Err("Query cancelled".into());
                }
                if right == end
                    || (left < middle && compare(slots[left], slots[right]) != Ordering::Greater)
                {
                    *destination = slots[left];
                    left += 1;
                } else {
                    *destination = slots[right];
                    right += 1;
                }
            }
        }
        std::mem::swap(slots, &mut scratch);
        width = width.saturating_mul(2);
    }
    Ok(())
}

/// Select only the requested prefix before sorting it. Three-way partitioning
/// handles equal keys, and a depth budget bounds adversarial partition choices.
pub(crate) fn select_prefix(
    slots: &mut Vec<u32>,
    end: usize,
    compare: impl Fn(u32, u32) -> Ordering + Copy,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let end = end.min(slots.len());
    if end == 0 {
        slots.clear();
        return Ok(());
    }
    let target = end - 1;
    let (mut low, mut high) = (0, slots.len());
    let mut budget = slots.len().ilog2() as usize * 2 + 1;
    while high - low > 1024 && target >= low && target < high {
        if cancelled.load(AtomicOrdering::Relaxed) {
            return Err("Query cancelled".into());
        }
        if budget == 0 {
            let mut remainder = slots[low..high].to_vec();
            sort_slots(&mut remainder, compare, cancelled)?;
            slots[low..high].copy_from_slice(&remainder);
            break;
        }
        budget -= 1;
        let mut pivots = [slots[low], slots[low + (high - low) / 2], slots[high - 1]];
        pivots.sort_unstable_by(|a, b| compare(*a, *b));
        let pivot = pivots[1];
        let (mut less, mut current, mut greater) = (low, low, high);
        let mut comparisons = 0;
        while current < greater {
            if comparisons % 1024 == 0 && cancelled.load(AtomicOrdering::Relaxed) {
                return Err("Query cancelled".into());
            }
            comparisons += 1;
            match compare(slots[current], pivot) {
                Ordering::Less => {
                    slots.swap(less, current);
                    less += 1;
                    current += 1;
                }
                Ordering::Greater => {
                    greater -= 1;
                    slots.swap(current, greater);
                }
                Ordering::Equal => current += 1,
            }
        }
        if target < less {
            high = less;
        } else if target >= greater {
            low = greater;
        } else {
            low = high;
            break;
        }
    }
    if low < high {
        slots[low..high].sort_unstable_by(|a, b| compare(*a, *b));
    }
    if cancelled.load(AtomicOrdering::Relaxed) {
        return Err("Query cancelled".into());
    }
    slots.truncate(end);
    sort_slots(slots, compare, cancelled)
}
