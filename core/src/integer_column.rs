//! Frame-of-reference integer columns. Arithmetic is exact, including signed
//! extremes; kernels select a representation once per block, not once per row.
use crate::entry_table::{Column, ColumnValue};
use std::sync::Arc;

pub(crate) trait Scalar: ColumnValue {
    fn rank(self) -> u64;
    fn from_rank(rank: u64) -> Option<Self>;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn roundtrip<T: Scalar + std::fmt::Debug>(values: &[T]) {
        let mut column = IntegerColumn::default();
        for (slot, &value) in values.iter().enumerate() {
            column.set(slot, value);
        }
        column.compact();
        let old = column.clone();
        let mut encoded = Vec::new();
        column.encode(&mut encoded);
        let mut mapping = memmap2::MmapMut::map_anon(encoded.len().max(1)).unwrap();
        mapping[..encoded.len()].copy_from_slice(&encoded);
        let mapping = Arc::new(mapping.make_read_only().unwrap());
        let mut position = 0;
        let decoded =
            IntegerColumn::<T>::decode(&mapping, &mut position, values.len(), encoded.len())
                .unwrap();
        assert_eq!(position, encoded.len());
        let mut actual = Vec::new();
        decoded.for_each(|_, value| actual.push(value));
        assert_eq!(actual, values);
        for (slot, &value) in values.iter().enumerate() {
            assert_eq!(decoded.get(slot), value);
        }
        let boundaries: Vec<_> = [0, u64::MAX]
            .into_iter()
            .chain(values.iter().map(|value| value.rank()))
            .collect();
        for &low in &boundaries {
            for &high in &boundaries {
                if low > high {
                    continue;
                }
                let mut bits = vec![0u8; values.len().div_ceil(8)];
                decoded.fill_rank_range(&mut bits, low, high);
                for (slot, value) in values.iter().enumerate() {
                    assert_eq!(
                        bits[slot / 8] & (1 << (slot % 8)) != 0,
                        (low..=high).contains(&value.rank())
                    );
                }
            }
        }
        column.set(0, T::from_rank(0).unwrap());
        column.compact();
        assert_eq!(old.get(0), values[0]);
    }
    #[test]
    fn all_widths_signed_extremes_and_copy_on_write_roundtrip() {
        roundtrip(&[7u64, 7, 7]);
        roundtrip(&[900000u64, 900255]);
        roundtrip(&[900000u64, 965535]);
        roundtrip(&[900000u64, 4295867295]);
        roundtrip(&[0u64, u64::MAX]);
        roundtrip(&[i64::MIN, i64::MAX]);
        roundtrip(&[i64::MIN, i64::MIN + 255]);
        roundtrip(&[-20i64, 19, 400]);
        roundtrip(&[42u32, 43, 65540]);
        roundtrip(&[0u32, u32::MAX]);
    }
    #[test]
    fn invalid_width_overflow_and_truncation_are_rejected() {
        for (base, width, tail) in [
            (u64::MAX, 1u64, vec![1u8]),
            (0, 3, vec![0; 3]),
            (0, 8, vec![0; 7]),
        ] {
            let mut bytes = base.to_le_bytes().to_vec();
            bytes.extend_from_slice(&width.to_le_bytes());
            bytes.extend_from_slice(&tail);
            let mut mapping = memmap2::MmapMut::map_anon(bytes.len()).unwrap();
            mapping.copy_from_slice(&bytes);
            assert!(
                IntegerColumn::<u64>::decode(
                    &Arc::new(mapping.make_read_only().unwrap()),
                    &mut 0,
                    1,
                    bytes.len()
                )
                .is_none()
            );
        }
    }
}
impl Scalar for u64 {
    fn rank(self) -> u64 {
        self
    }
    fn from_rank(rank: u64) -> Option<Self> {
        Some(rank)
    }
}
impl Scalar for i64 {
    fn rank(self) -> u64 {
        (self as u64) ^ (1 << 63)
    }
    fn from_rank(rank: u64) -> Option<Self> {
        Some((rank ^ (1 << 63)) as i64)
    }
}
impl Scalar for u32 {
    fn rank(self) -> u64 {
        self as u64
    }
    fn from_rank(rank: u64) -> Option<Self> {
        rank.try_into().ok()
    }
}
#[derive(Clone)]
pub(crate) enum Offsets {
    Constant(usize),
    Byte(Column<u8>),
    Short(Column<u16>),
    Word(Column<u32>),
}
#[derive(Clone)]
pub(crate) enum IntegerData<T> {
    Raw(Column<T>),
    Packed { base: u64, offsets: Offsets },
}
#[derive(Clone)]
pub(crate) struct IntegerColumn<T>(pub(crate) Arc<IntegerData<T>>);
impl<T: Scalar> Default for IntegerColumn<T> {
    fn default() -> Self {
        Self(Arc::new(IntegerData::Raw(Column::default())))
    }
}
impl<T: Scalar> IntegerColumn<T> {
    pub fn len(&self) -> usize {
        match self.0.as_ref() {
            IntegerData::Raw(values) => values.values().len(),
            IntegerData::Packed { offsets, .. } => match offsets {
                Offsets::Constant(length) => *length,
                Offsets::Byte(values) => values.values().len(),
                Offsets::Short(values) => values.values().len(),
                Offsets::Word(values) => values.values().len(),
            },
        }
    }
    #[inline]
    pub fn get(&self, slot: usize) -> T {
        match self.0.as_ref() {
            IntegerData::Raw(values) => values.values()[slot],
            IntegerData::Packed { base, offsets } => {
                let offset = match offsets {
                    Offsets::Constant(length) => {
                        assert!(slot < *length);
                        0
                    }
                    Offsets::Byte(values) => values.values()[slot] as u64,
                    Offsets::Short(values) => values.values()[slot] as u64,
                    Offsets::Word(values) => values.values()[slot] as u64,
                };
                T::from_rank(base + offset).expect("Validated integer column")
            }
        }
    }
    pub fn for_each(&self, mut work: impl FnMut(usize, T)) {
        match self.0.as_ref() {
            IntegerData::Raw(values) => {
                for (slot, &value) in values.values().iter().enumerate() {
                    work(slot, value);
                }
            }
            IntegerData::Packed { base, offsets } => {
                let mut visit = |slot, delta| {
                    work(
                        slot,
                        T::from_rank(base + delta).expect("Validated integer column"),
                    )
                };
                match offsets {
                    Offsets::Constant(length) => {
                        for slot in 0..*length {
                            visit(slot, 0);
                        }
                    }
                    Offsets::Byte(values) => {
                        for (slot, &value) in values.values().iter().enumerate() {
                            visit(slot, value as u64);
                        }
                    }
                    Offsets::Short(values) => {
                        for (slot, &value) in values.values().iter().enumerate() {
                            visit(slot, value as u64);
                        }
                    }
                    Offsets::Word(values) => {
                        for (slot, &value) in values.values().iter().enumerate() {
                            visit(slot, value as u64);
                        }
                    }
                }
            }
        }
    }
    pub fn fill_rank_range(&self, output: &mut [u8], lower: u64, upper: u64) {
        fn fill<V: Copy>(values: &[V], output: &mut [u8], predicate: impl Fn(V) -> bool) {
            for (values, byte) in values.chunks(8).zip(output) {
                let mut bits = 0;
                for (bit, &value) in values.iter().enumerate() {
                    bits |= u8::from(predicate(value)) << bit;
                }
                *byte = bits;
            }
        }
        fn relative(base: u64, low: u64, high: u64, maximum: u64) -> Option<(u64, u64)> {
            let upper = high.checked_sub(base)?.min(maximum);
            let lower = low.saturating_sub(base);
            (lower <= upper).then_some((lower, upper))
        }
        match self.0.as_ref() {
            IntegerData::Raw(values) => fill(values.values(), output, |value| {
                let rank = value.rank();
                rank >= lower && rank <= upper
            }),
            IntegerData::Packed { base, offsets } => match offsets {
                Offsets::Constant(_) => output.fill(if *base >= lower && *base <= upper {
                    u8::MAX
                } else {
                    0
                }),
                Offsets::Byte(values) => {
                    if let Some((low, high)) = relative(*base, lower, upper, u8::MAX as u64) {
                        let (low, high) = (low as u8, high as u8);
                        fill(values.values(), output, |value| {
                            value >= low && value <= high
                        });
                    } else {
                        output.fill(0);
                    }
                }
                Offsets::Short(values) => {
                    if let Some((low, high)) = relative(*base, lower, upper, u16::MAX as u64) {
                        let (low, high) = (low as u16, high as u16);
                        fill(values.values(), output, |value| {
                            value >= low && value <= high
                        });
                    } else {
                        output.fill(0);
                    }
                }
                Offsets::Word(values) => {
                    if let Some((low, high)) = relative(*base, lower, upper, u32::MAX as u64) {
                        let (low, high) = (low as u32, high as u32);
                        fill(values.values(), output, |value| {
                            value >= low && value <= high
                        });
                    } else {
                        output.fill(0);
                    }
                }
            },
        }
    }
    pub fn set(&mut self, slot: usize, value: T) {
        if slot < self.len() && self.get(slot) == value {
            return;
        }
        if !matches!(self.0.as_ref(), IntegerData::Raw(_)) {
            let mut values = Vec::with_capacity(self.len() + usize::from(slot == self.len()));
            self.for_each(|_, value| values.push(value));
            self.0 = Arc::new(IntegerData::Raw(Column::owned(values)));
        }
        let IntegerData::Raw(values) = Arc::make_mut(&mut self.0) else {
            unreachable!()
        };
        values.set(slot, value);
    }
    pub fn compact(&mut self) {
        let IntegerData::Raw(values) = self.0.as_ref() else {
            return;
        };
        if values.is_mapped() || values.values().is_empty() {
            return;
        }
        let low = values
            .values()
            .iter()
            .map(|value| value.rank())
            .min()
            .unwrap();
        let high = values
            .values()
            .iter()
            .map(|value| value.rank())
            .max()
            .unwrap();
        let span = high - low;
        let width = if span == 0 {
            0
        } else if span <= u8::MAX as u64 {
            1
        } else if span <= u16::MAX as u64 {
            2
        } else if span <= u32::MAX as u64 {
            4
        } else {
            8
        };
        if width >= std::mem::size_of::<T>() {
            return;
        }
        let deltas = || values.values().iter().map(|value| value.rank() - low);
        let offsets = match width {
            0 => Offsets::Constant(values.values().len()),
            1 => Offsets::Byte(Column::owned(deltas().map(|value| value as u8).collect())),
            2 => Offsets::Short(Column::owned(deltas().map(|value| value as u16).collect())),
            4 => Offsets::Word(Column::owned(deltas().map(|value| value as u32).collect())),
            _ => unreachable!(),
        };
        self.0 = Arc::new(IntegerData::Packed { base: low, offsets });
    }
    pub fn encode(&self, output: &mut Vec<u8>) {
        let (base, width, bytes) = match self.0.as_ref() {
            IntegerData::Raw(values) => (0, std::mem::size_of::<T>(), values.bytes()),
            IntegerData::Packed { base, offsets } => match offsets {
                Offsets::Constant(_) => (*base, 0, &[][..]),
                Offsets::Byte(values) => (*base, 1, values.bytes()),
                Offsets::Short(values) => (*base, 2, values.bytes()),
                Offsets::Word(values) => (*base, 4, values.bytes()),
            },
        };
        output.resize(output.len().next_multiple_of(8), 0);
        output.extend_from_slice(&base.to_le_bytes());
        output.extend_from_slice(&(width as u64).to_le_bytes());
        output.extend_from_slice(bytes);
    }
    pub fn decode(
        mapping: &Arc<memmap2::Mmap>,
        offset: &mut usize,
        count: usize,
        end: usize,
    ) -> Option<Self> {
        *offset = offset.checked_add(7)? & !7;
        let header_end = offset.checked_add(16)?;
        if header_end > end {
            return None;
        }
        let base = u64::from_le_bytes(mapping.get(*offset..*offset + 8)?.try_into().ok()?);
        let width = usize::try_from(u64::from_le_bytes(
            mapping.get(*offset + 8..header_end)?.try_into().ok()?,
        ))
        .ok()?;
        let next = header_end.checked_add(count.checked_mul(width)?)?;
        if next > end {
            return None;
        }
        let range = header_end..next;
        let data = if width == std::mem::size_of::<T>() {
            if base != 0 {
                return None;
            }
            IntegerData::Raw(Column::mapped(mapping.clone(), range)?)
        } else {
            T::from_rank(base)?;
            let offsets = match width {
                0 => Offsets::Constant(count),
                1 => Offsets::Byte(Column::mapped(mapping.clone(), range)?),
                2 => Offsets::Short(Column::mapped(mapping.clone(), range)?),
                4 if width < std::mem::size_of::<T>() => {
                    Offsets::Word(Column::mapped(mapping.clone(), range)?)
                }
                _ => return None,
            };
            let maximum = match &offsets {
                Offsets::Constant(_) => 0,
                Offsets::Byte(values) => values.values().iter().copied().max().unwrap_or(0) as u64,
                Offsets::Short(values) => values.values().iter().copied().max().unwrap_or(0) as u64,
                Offsets::Word(values) => values.values().iter().copied().max().unwrap_or(0) as u64,
            };
            T::from_rank(base.checked_add(maximum)?)?;
            IntegerData::Packed { base, offsets }
        };
        *offset = next;
        Some(Self(Arc::new(data)))
    }
    pub fn account(
        &self,
        owned: &mut usize,
        mappings: &mut std::collections::HashMap<usize, usize>,
        allocations: &mut std::collections::HashSet<usize>,
    ) {
        match self.0.as_ref() {
            IntegerData::Raw(values) => values.account(owned, mappings, allocations),
            IntegerData::Packed { offsets, .. } => match offsets {
                Offsets::Constant(_) => (),
                Offsets::Byte(values) => values.account(owned, mappings, allocations),
                Offsets::Short(values) => values.account(owned, mappings, allocations),
                Offsets::Word(values) => values.account(owned, mappings, allocations),
            },
        }
    }
}
