//! Immutable UTF-8 slices with block ownership, without one allocation per string.
use std::{
    borrow::Borrow,
    cmp::Ordering,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::Arc,
};

#[derive(Clone)]
pub struct SharedText {
    owner: Arc<str>,
    start: u32,
    length: u32,
}
impl SharedText {
    pub fn ptr_eq(left: &Self, right: &Self) -> bool {
        Arc::ptr_eq(&left.owner, &right.owner)
            && left.start == right.start
            && left.length == right.length
    }
    #[cfg(test)]
    pub(crate) fn allocation_identity(&self) -> (usize, usize) {
        (
            Arc::as_ptr(&self.owner) as *const u8 as usize,
            self.owner.len(),
        )
    }
}
impl Deref for SharedText {
    type Target = str;
    fn deref(&self) -> &str {
        &self.owner[self.start as usize..self.start as usize + self.length as usize]
    }
}
impl AsRef<str> for SharedText {
    fn as_ref(&self) -> &str {
        self
    }
}
impl Borrow<str> for SharedText {
    fn borrow(&self) -> &str {
        self
    }
}
impl From<String> for SharedText {
    fn from(text: String) -> Self {
        let length =
            u32::try_from(text.len()).expect("An individual indexed string must fit in u32");
        Self {
            owner: Arc::from(text),
            start: 0,
            length,
        }
    }
}
impl From<&str> for SharedText {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}
impl Default for SharedText {
    fn default() -> Self {
        String::new().into()
    }
}
impl PartialEq for SharedText {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl Eq for SharedText {}
impl PartialOrd for SharedText {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SharedText {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_ref().cmp(other.as_ref())
    }
}
impl Hash for SharedText {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

pub(crate) struct TextArena {
    blocks: Vec<(usize, Arc<str>)>,
    length: usize,
}
impl TextArena {
    const BLOCK_BYTES: usize = 1024 * 1024;
    pub(crate) fn new(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut blocks = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let mut end = start.saturating_add(Self::BLOCK_BYTES).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            blocks.push((start, Arc::from(&text[start..end])));
            start = end;
        }
        Some(Self {
            blocks,
            length: text.len(),
        })
    }
    pub(crate) fn get(&self, offset: usize, length: u32) -> Option<SharedText> {
        let end = offset.checked_add(length as usize)?;
        if end > self.length {
            return None;
        }
        let index = self
            .blocks
            .partition_point(|(start, _)| *start <= offset)
            .checked_sub(1);
        let Some(index) = index else {
            return (offset == 0 && length == 0).then(SharedText::default);
        };
        let (start, owner) = &self.blocks[index];
        let local = offset - start;
        if local + length as usize <= owner.len() {
            owner.get(local..local + length as usize)?;
            return Some(SharedText {
                owner: owner.clone(),
                start: local as u32,
                length,
            });
        }
        // Rare strings spanning blocks own only their bytes, so an old result
        // cannot retain the whole arena. Validate boundaries before copying.
        let first = owner.get(local..)?;
        let mut result = String::with_capacity(length as usize);
        result.push_str(first);
        for (_, block) in &self.blocks[index + 1..] {
            let remaining = length as usize - result.len();
            let count = remaining.min(block.len());
            result.push_str(block.get(..count)?);
            if result.len() == length as usize {
                return Some(result.into());
            }
        }
        None
    }
}

impl std::fmt::Display for SharedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_ref().fmt(f)
    }
}

impl std::fmt::Debug for SharedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_ref(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_retained_result_keeps_only_its_block_alive() {
        let arena = TextArena::new(&vec![b'a'; TextArena::BLOCK_BYTES * 3]).unwrap();
        let blocks: Vec<_> = arena
            .blocks
            .iter()
            .map(|(_, owner)| Arc::downgrade(owner))
            .collect();
        let retained = arena.get(TextArena::BLOCK_BYTES + 7, 20).unwrap();
        drop(arena);
        assert!(blocks[0].upgrade().is_none());
        assert!(blocks[1].upgrade().is_some());
        assert!(blocks[2].upgrade().is_none());
        assert_eq!(retained.len(), 20);
        drop(retained);
        assert!(blocks[1].upgrade().is_none());
    }

    #[test]
    fn invalid_utf8_and_out_of_bounds_references_are_rejected() {
        assert!(TextArena::new(&[0xff]).is_none());
        let arena = TextArena::new("中文".as_bytes()).unwrap();
        assert!(arena.get(1, 3).is_none());
        assert!(arena.get(0, 4).is_none());
        assert!(arena.get(7, 0).is_none());
        assert_eq!(arena.get(6, 0).unwrap().as_ref(), "");
        assert_eq!(TextArena::new(&[]).unwrap().get(0, 0).unwrap().as_ref(), "");
    }

    #[test]
    fn block_slices_survive_arena_drop_and_preserve_unicode_boundaries() {
        let text = format!("{}中文-tail", "a".repeat(TextArena::BLOCK_BYTES - 1));
        let arena = TextArena::new(text.as_bytes()).unwrap();
        let first = arena.get(3, 7).unwrap();
        let repeated = arena.get(3, 7).unwrap();
        assert!(SharedText::ptr_eq(&first, &repeated));
        let crossing = arena.get(TextArena::BLOCK_BYTES - 2, 8).unwrap();
        assert_eq!(crossing.as_ref(), "a中文-");
        assert!(arena.get(TextArena::BLOCK_BYTES, 1).is_none());
        assert!(arena.get(usize::MAX, 1).is_none());
        drop(arena);
        assert_eq!(first.as_ref(), "aaaaaaa");
        assert_eq!(crossing.as_ref(), "a中文-");
        assert_eq!(std::mem::size_of::<SharedText>(), 24);
    }
}
