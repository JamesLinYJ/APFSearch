//! Build immutable text arenas with direct, byte-identical prefix sharing.
//! References remain contiguous ranges; readers do not decode or reconstruct text.
use crate::entry_table::TextRef;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct TextPoolBuilder {
    text: String,
    references: HashMap<String, TextRef>,
}

impl TextPoolBuilder {
    pub(crate) fn intern(&mut self, value: &str) -> TextRef {
        if let Some(&reference) = self.references.get(value) {
            return reference;
        }
        let reference = TextRef {
            offset: u32::try_from(self.text.len()).expect("Chunk text exceeds 4 GiB"),
            length: u32::try_from(value.len()).expect("Text exceeds 4 GiB"),
        };
        self.text
            .len()
            .checked_add(value.len())
            .filter(|&end| end <= u32::MAX as usize)
            .expect("Chunk text exceeds 4 GiB");
        self.text.push_str(value);
        self.references.insert(value.into(), reference);
        reference
    }

    /// Reverse byte order places a string before every proper prefix. Each
    /// prefix can therefore borrow the most recently visited descendant. The
    /// keys are temporary construction state and are released with the builder.
    pub(crate) fn intern_prefixes<'a>(&mut self, values: impl IntoIterator<Item = &'a str>) {
        let mut values: Vec<_> = values.into_iter().collect();
        values.sort_unstable_by(|a, b| b.cmp(a));
        values.dedup();
        let mut previous: Option<(&str, TextRef)> = None;
        for value in values {
            let reference = if let Some(&reference) = self.references.get(value) {
                reference
            } else if let Some((descendant, reference)) = previous
                && descendant.starts_with(value)
            {
                let reference = TextRef {
                    offset: reference.offset,
                    length: u32::try_from(value.len()).expect("Prefix fits its descendant"),
                };
                self.references.insert(value.into(), reference);
                reference
            } else {
                self.intern(value)
            };
            previous = Some((value, reference));
        }
    }

    pub(crate) fn finish(mut self) -> String {
        self.text.shrink_to_fit();
        self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_prefixes_share_bytes_and_hot_names_remain_contiguous() {
        let mut builder = TextPoolBuilder::default();
        let names = ["alpha", "café", "报告"];
        let hot: Vec<_> = names.iter().map(|name| builder.intern(name)).collect();
        let prefixes = [
            "/",
            "/目录/",
            "/目录/子目录/",
            "/目录",
            "/目录/子目录",
            "/else/",
            "",
        ];
        builder.intern_prefixes(prefixes);
        let references: Vec<_> = prefixes.iter().map(|text| builder.intern(text)).collect();
        let text = builder.finish();
        assert!(text.starts_with(&names.concat()));
        assert_eq!(
            text.len(),
            names.concat().len() + "/目录/子目录/".len() + "/else/".len()
        );
        for (expected, reference) in names
            .iter()
            .chain(prefixes.iter())
            .zip(hot.iter().chain(&references))
        {
            assert_eq!(
                &text[reference.offset as usize..(reference.offset + reference.length) as usize],
                *expected
            );
        }
    }

    #[test]
    fn preexisting_prefixes_and_branching_unicode_preserve_exact_bytes() {
        let values = [
            "a", "ab", "abc", "abd", "é", "e\u{301}", "é/", "é/x", "ø", "🙂", "🙂/", "🙂/x", "",
        ];
        for count in 0..=values.len() {
            let mut builder = TextPoolBuilder::default();
            for value in &values[..count] {
                builder.intern(value);
            }
            builder.intern_prefixes(values);
            let references: Vec<_> = values.iter().map(|value| builder.intern(value)).collect();
            let text = builder.finish();
            for (&expected, reference) in values.iter().zip(references) {
                assert_eq!(
                    &text
                        [reference.offset as usize..(reference.offset + reference.length) as usize],
                    expected
                );
            }
        }
    }
}
