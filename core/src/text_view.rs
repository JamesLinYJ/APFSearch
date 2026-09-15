//! Borrowed text composed of at most two immutable UTF-8 regions.
use memchr::memmem::Finder;
use serde::{Serialize, Serializer};
use std::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
};

#[derive(Clone, Copy, Debug)]
pub struct TextView<'a> {
    pub(crate) prefix: &'a str,
    pub(crate) suffix: &'a str,
}

impl<'a> TextView<'a> {
    pub fn new(prefix: &'a str, suffix: &'a str) -> Self {
        Self { prefix, suffix }
    }
    pub fn len(self) -> usize {
        self.prefix.len() + self.suffix.len()
    }
    pub fn is_empty(self) -> bool {
        self.prefix.is_empty() && self.suffix.is_empty()
    }
    pub fn bytes(self) -> impl Iterator<Item = u8> + Clone + 'a {
        self.prefix.bytes().chain(self.suffix.bytes())
    }
    pub fn write_to(self, output: &mut String) {
        output.clear();
        output.reserve(self.len());
        output.push_str(self.prefix);
        output.push_str(self.suffix);
    }
    pub fn with_str<R>(self, work: impl FnOnce(&str) -> R) -> R {
        if self.prefix.is_empty() {
            return work(self.suffix);
        }
        if self.suffix.is_empty() {
            return work(self.prefix);
        }
        let mut scratch = crate::query_scratch::TextScratch::acquire(self.len());
        self.with_scratch(&mut scratch, work)
    }
    pub(crate) fn with_scratch<R>(
        self,
        scratch: &mut crate::query_scratch::TextScratch,
        work: impl FnOnce(&str) -> R,
    ) -> R {
        if self.prefix.is_empty() {
            return work(self.suffix);
        }
        if self.suffix.is_empty() {
            return work(self.prefix);
        }
        scratch.assign(self);
        work(scratch)
    }
    pub fn contains(self, finder: &Finder<'_>) -> bool {
        finder.find(self.prefix.as_bytes()).is_some()
            || finder.find(self.suffix.as_bytes()).is_some()
            || self.crosses_boundary(finder)
    }
    pub(crate) fn crosses_boundary(self, finder: &Finder<'_>) -> bool {
        let overlap = finder.needle().len().saturating_sub(1);
        if overlap == 0 || self.prefix.is_empty() || self.suffix.is_empty() {
            return false;
        }
        if self.prefix.ends_with('/') && !self.suffix.as_bytes().contains(&b'/') {
            let Some(separator) = finder.needle().iter().rposition(|&byte| byte == b'/') else {
                return false;
            };
            let split = separator + 1;
            return split < finder.needle().len()
                && self.prefix.as_bytes().ends_with(&finder.needle()[..split])
                && self
                    .suffix
                    .as_bytes()
                    .starts_with(&finder.needle()[split..]);
        }
        // Byte boundaries are intentional: the searcher consumes bytes, and
        // the concatenation preserves the exact UTF-8 subject at the join.
        crate::query_scratch::with_bytes(
            self.prefix.len().min(overlap) + self.suffix.len().min(overlap),
            |scratch| {
                scratch.extend_from_slice(
                    &self.prefix.as_bytes()[self.prefix.len().saturating_sub(overlap)..],
                );
                scratch
                    .extend_from_slice(&self.suffix.as_bytes()[..self.suffix.len().min(overlap)]);
                finder.find(scratch).is_some()
            },
        )
    }
    pub fn natural_cmp(self, other: Self) -> Ordering {
        if self.prefix == other.prefix
            && self
                .prefix
                .as_bytes()
                .last()
                .is_none_or(|byte| !byte.is_ascii_digit())
        {
            return crate::query::natural_cmp_folded(self.suffix, other.suffix);
        }
        let mut left = self.bytes().peekable();
        let mut right = other.bytes().peekable();
        loop {
            match (left.peek().copied(), right.peek().copied()) {
                (Some(a), Some(b)) if a.is_ascii_digit() && b.is_ascii_digit() => {
                    while left.peek() == Some(&b'0') {
                        left.next();
                    }
                    while right.peek() == Some(&b'0') {
                        right.next();
                    }
                    let first = left.clone().take_while(u8::is_ascii_digit);
                    let second = right.clone().take_while(u8::is_ascii_digit);
                    let first_len = first.clone().count();
                    let second_len = second.clone().count();
                    let order = first_len.cmp(&second_len).then_with(|| first.cmp(second));
                    if order != Ordering::Equal {
                        return order;
                    }
                    for _ in 0..first_len {
                        left.next();
                    }
                    for _ in 0..second_len {
                        right.next();
                    }
                }
                (Some(a), Some(b)) => {
                    let order = a.cmp(&b);
                    if order != Ordering::Equal {
                        return order;
                    }
                    left.next();
                    right.next();
                }
                (a, b) => return a.cmp(&b),
            }
        }
    }
}
impl<'a> From<&'a str> for TextView<'a> {
    fn from(value: &'a str) -> Self {
        Self::new("", value)
    }
}
impl fmt::Display for TextView<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(self.prefix)?;
        output.write_str(self.suffix)
    }
}
impl PartialEq for TextView<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.cmp(other) == Ordering::Equal
    }
}
impl Eq for TextView<'_> {}
impl PartialOrd for TextView<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TextView<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.prefix == other.prefix {
            return self.suffix.cmp(other.suffix);
        }
        // Compare contiguous spans with slice comparison. At most three span
        // comparisons are needed; a split must not turn a long common prefix
        // into a branch for every byte in every sorting tie-break.
        let mut left = [self.prefix.as_bytes(), self.suffix.as_bytes()];
        let mut right = [other.prefix.as_bytes(), other.suffix.as_bytes()];
        let (mut a, mut b) = (0, 0);
        loop {
            while a < 2 && left[a].is_empty() {
                a += 1;
            }
            while b < 2 && right[b].is_empty() {
                b += 1;
            }
            if a == 2 || b == 2 {
                return (a != 2).cmp(&(b != 2));
            }
            let length = left[a].len().min(right[b].len());
            let order = left[a][..length].cmp(&right[b][..length]);
            if order != Ordering::Equal {
                return order;
            }
            left[a] = &left[a][length..];
            right[b] = &right[b][length..];
        }
    }
}
impl Hash for TextView<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Identical logical text must hash identically regardless of its split.
        for byte in self.bytes() {
            state.write_u8(byte);
        }
        state.write_u8(0xff);
    }
}
impl PartialEq<&str> for TextView<'_> {
    fn eq(&self, other: &&str) -> bool {
        self.len() == other.len()
            && other.as_bytes().starts_with(self.prefix.as_bytes())
            && other.as_bytes()[self.prefix.len()..] == *self.suffix.as_bytes()
    }
}
impl PartialEq<TextView<'_>> for &str {
    fn eq(&self, other: &TextView<'_>) -> bool {
        other == self
    }
}
impl Serialize for TextView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl From<TextView<'_>> for crate::shared_text::SharedText {
    fn from(value: TextView<'_>) -> Self {
        value.with_str(|text| Self::from(text))
    }
}
impl From<TextView<'_>> for String {
    fn from(value: TextView<'_>) -> Self {
        let mut output = String::with_capacity(value.len());
        value.write_to(&mut output);
        output
    }
}
impl PartialEq<String> for TextView<'_> {
    fn eq(&self, other: &String) -> bool {
        self == &other.as_str()
    }
}
impl PartialEq<TextView<'_>> for String {
    fn eq(&self, other: &TextView<'_>) -> bool {
        other == &self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_matching_and_ordering_equal_contiguous_reference() {
        let texts = [
            "",
            "/",
            "//",
            "relative",
            "/alpha/report002.txt",
            "/alpha/report2.txt",
            "/alpha/report10.txt",
            "/目录/报告.txt",
            "café",
            "cafe\u{301}",
            "009",
            "9",
            "000",
            "0",
            "/a././leaf/",
        ];
        for first in texts {
            for split in (0..=first.len()).filter(|&split| first.is_char_boundary(split)) {
                let view = TextView::new(&first[..split], &first[split..]);
                assert_eq!(view.to_string(), first);
                for needle in [
                    "",
                    "/",
                    "a/",
                    "ha/re",
                    "002",
                    "报",
                    "目录/报",
                    "é",
                    "e\u{301}",
                    "./leaf/",
                ] {
                    assert_eq!(
                        view.contains(&Finder::new(needle)),
                        first.contains(needle),
                        "{first:?} at {split}, {needle:?}"
                    );
                }
                for second in texts {
                    assert_eq!(view.cmp(&second.into()), first.cmp(second));
                    assert_eq!(
                        view.natural_cmp(second.into()),
                        crate::query::natural_cmp_folded(first, second)
                    );
                }
            }
        }
    }
    #[test]
    fn hash_and_serialization_do_not_depend_on_fragment_boundaries() {
        use std::collections::hash_map::DefaultHasher;
        let hash = |value: TextView<'_>| {
            let mut state = DefaultHasher::new();
            value.hash(&mut state);
            state.finish()
        };
        let split = TextView::new("/目录/", "a\"b.txt");
        let full = "/目录/a\"b.txt";
        assert_eq!(hash(split), hash(full.into()));
        assert_eq!(
            serde_json::to_value(split).unwrap(),
            serde_json::json!(full)
        );
    }
}
