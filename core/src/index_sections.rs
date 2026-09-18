//! Stable secondary-index partitions. A changed posting does not rewrite all
//! other postings or both complete ordering arrays.
use super::*;
const MAGIC: &[u8; 8] = b"APFSEC04";
const SHARDS: usize = 256;
const ORDER_BLOCK: usize = crate::slot_order::LEAF_LENGTH;
use crate::posting_map::partition;
#[path = "order_validation.rs"]
mod order_validation;
fn descriptor(output: &mut Vec<u8>, section: &Section) {
    output.extend_from_slice(&section.hash);
    output.extend_from_slice(&section.bytes.to_le_bytes());
}
fn publish_bytes(
    directory: &Path,
    publications: &mut Publications,
    bytes: &[u8],
) -> io::Result<Arc<Section>> {
    let hash = *payload_digest(bytes).as_bytes();
    let path = directory.join(format!(
        "{}.segment",
        blake3::Hash::from_bytes(hash).to_hex()
    ));
    // Only reuse an in-process pinned section whose bytes were verified or
    // published here. Unknown on-disk files are replaced atomically.
    if let Some(section) = publications.sections.get(&path).and_then(Weak::upgrade)
        && section.bytes == bytes.len() as u64
        && reusable(&section, directory)
    {
        return Ok(section);
    }
    write_section(directory, publications, |output| output.write_all(bytes))
}
pub(super) fn write(
    directory: &Path,
    publications: &mut Publications,
    snapshot: &SearchSnapshot,
) -> io::Result<Arc<Section>> {
    let mut cpu = crate::cpu_executor::enter_background();
    let mut parts = Vec::new();
    let mut bytes = Vec::new();
    snapshot.live.serialize_into(&mut bytes)?;
    snapshot.path_ties.serialize_into(&mut bytes)?;
    parts.push(publish_bytes(directory, publications, &bytes)?);
    for order in [&snapshot.name_order, &snapshot.path_order] {
        for block in &order.blocks {
            cpu.checkpoint();
            bytes.clear();
            let section = if let Some(section) = block
                .section
                .get()
                .filter(|section| reusable(section, directory))
            {
                section.clone()
            } else {
                bytes.extend_from_slice(block.slots.bytes());
                let section = publish_bytes(directory, publications, &bytes)?;
                let _ = block.section.set(section.clone());
                section
            };
            parts.push(section);
        }
    }
    for shard in 0..SHARDS {
        cpu.checkpoint();
        if let Some(section) = snapshot.posting_sections[shard]
            .get()
            .filter(|section| reusable(section, directory))
        {
            parts.push(section.clone());
            continue;
        }
        let mut group: Vec<(u8, &[u8], &RoaringBitmap)> = snapshot
            .trigrams
            .partition_entries(shard)
            .map(|(key, bitmap)| (0, key.as_slice(), bitmap.as_ref()))
            .collect();
        snapshot
            .metadata_postings
            .visit_partition(shard, |kind, key, bitmap| group.push((kind, key, bitmap)));
        group.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        bytes.clear();
        bytes.extend_from_slice(&(group.len() as u32).to_le_bytes());
        for &(kind, key, bitmap) in group.iter() {
            bytes.push(kind);
            bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
            bytes.extend_from_slice(key);
            bitmap.serialize_into(&mut bytes)?;
        }
        let section = publish_bytes(directory, publications, &bytes)?;
        let _ = snapshot.posting_sections[shard].set(section.clone());
        parts.push(section);
    }
    bytes.clear();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(snapshot.entries.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&snapshot.live.len().to_le_bytes());
    bytes.extend_from_slice(&(snapshot.name_order.blocks.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&(snapshot.path_order.blocks.len() as u64).to_le_bytes());
    for part in &parts {
        descriptor(&mut bytes, part);
    }
    let root = publish_bytes(directory, publications, &bytes)?;
    let _ = root.dependencies.set(parts);
    Ok(root)
}
pub(super) fn read(
    root: &Arc<Section>,
    mapping: &Arc<memmap2::Mmap>,
    publications: &mut Publications,
    entries: crate::entry_table::EntryTable,
    generation: u64,
    content_revision: u64,
) -> Option<SearchSnapshot> {
    if mapping.get(..8)? != MAGIC {
        return None;
    }
    let mut position = 8;
    let count = usize::try_from(get64(mapping, &mut position)?).ok()?;
    let order_count = usize::try_from(get64(mapping, &mut position)?).ok()?;
    if count != entries.len() || order_count > count {
        return None;
    }
    let name_blocks = usize::try_from(get64(mapping, &mut position)?).ok()?;
    let path_blocks = usize::try_from(get64(mapping, &mut position)?).ok()?;
    for count in [name_blocks, path_blocks] {
        if count < order_count.div_ceil(ORDER_BLOCK) || count > order_count {
            return None;
        }
    }
    let part_count = 1usize
        .checked_add(name_blocks)?
        .checked_add(path_blocks)?
        .checked_add(SHARDS)?;
    if mapping.len() != position.checked_add(part_count.checked_mul(DESCRIPTOR_BYTES)?)? {
        return None;
    }
    let mut parts = Vec::with_capacity(part_count);
    for _ in 0..part_count {
        parts.push(read_descriptor(
            mapping,
            &mut position,
            &root.directory,
            publications,
        )?);
    }
    let live_mapping = map_section(&parts[0])?;
    let mut reader = Cursor::new(live_mapping.as_ref());
    let live = read_bitmap(&mut reader, count)?;
    let path_ties = read_bitmap(&mut reader, count)?;
    if reader.position() != live_mapping.len() as u64
        || live.len() as usize != order_count
        || !path_ties.is_subset(&live)
    {
        return None;
    }
    #[cfg(test)]
    crate::compact_acceptance_tests::restore_phase("index_manifest");
    let mut cpu = crate::cpu_executor::enter_background();
    let mut orders = Vec::with_capacity(2);
    for (first, blocks) in [(1, name_blocks), (1 + name_blocks, path_blocks)] {
        let mut order = Vec::with_capacity(blocks);
        let mut remaining = order_validation::RemainingSlots::new(&live);
        let mut total = 0;
        for part in &parts[first..first + blocks] {
            cpu.checkpoint();
            let mapping = map_section(part)?;
            if mapping.is_empty()
                || mapping.len() > ORDER_BLOCK * 4
                || !mapping.len().is_multiple_of(4)
            {
                return None;
            }
            for bytes in mapping.as_chunks::<4>().0 {
                let slot = u32::from_le_bytes(*bytes);
                if !remaining.consume(slot) {
                    return None;
                }
            }
            total += mapping.len() / 4;
            let block = crate::slot_order::OrderBlock {
                slots: crate::entry_table::Column::mapped(mapping.clone(), 0..mapping.len())?,
                section: std::sync::OnceLock::new(),
            };
            let _ = block.section.set(part.clone());
            order.push(Arc::new(block));
        }
        if total != order_count {
            return None;
        }
        orders.push(Arc::new(crate::slot_order::SlotOrder::from_blocks(order)));
    }
    #[cfg(test)]
    crate::compact_acceptance_tests::restore_phase("orders");
    let mut trigrams = crate::posting_map::PostingMap::default();
    let mut metadata_postings = MetadataPostings::default();
    let mut flags = 0u8;
    for (shard, part) in parts[1 + name_blocks + path_blocks..].iter().enumerate() {
        cpu.checkpoint();
        let mapping = map_section(part)?;
        let mut reader = Cursor::new(mapping.as_ref());
        let mut size = [0u8; 4];
        reader.read_exact(&mut size).ok()?;
        let length = u32::from_le_bytes(size) as usize;
        if length > mapping.len() / 5 {
            return None;
        }
        for _ in 0..length {
            let mut kind = [0u8; 1];
            reader.read_exact(&mut kind).ok()?;
            reader.read_exact(&mut size).ok()?;
            let key_length = u32::from_le_bytes(size) as usize;
            let start = reader.position() as usize;
            let end = start.checked_add(key_length)?;
            let key = mapping.get(start..end)?;
            if partition(kind[0], key) != shard {
                return None;
            }
            reader.set_position(end as u64);
            let bitmap = read_bitmap(&mut reader, count)?;
            if kind[0] == 0 {
                let key: [u8; 3] = key.try_into().ok()?;
                if trigrams.insert(key, Arc::new(bitmap)).is_some() {
                    return None;
                }
            } else {
                if matches!(kind[0], 4 | 5) {
                    let bit = 1 << kind[0];
                    if flags & bit != 0 {
                        return None;
                    }
                    flags |= bit;
                }
                metadata_postings.insert_restored(kind[0], key, bitmap)?;
            }
        }
        if reader.position() != mapping.len() as u64 {
            return None;
        }
    }
    if flags != (1 << 4) | (1 << 5) {
        return None;
    }
    #[cfg(test)]
    crate::compact_acceptance_tests::restore_phase("postings");
    let file_slots = FileSlots::from_entries(&entries).ok()?;
    #[cfg(test)]
    crate::compact_acceptance_tests::restore_phase("file_slots");
    let path_order = orders.pop()?;
    let name_order = orders.pop()?;
    let snapshot = SearchSnapshot::from_prepared_cache(PreparedSnapshot {
        entries,
        labels: Arc::default(),
        file_slots,
        live,
        trigrams: Arc::new(trigrams),
        name_order,
        path_order,
        path_ties: Arc::new(path_ties),
        metadata_postings,
        generation,
        content_revision,
    });
    for (shard, part) in parts[1 + name_blocks + path_blocks..].iter().enumerate() {
        let _ = snapshot.posting_sections[shard].set(part.clone());
    }
    let _ = root.dependencies.set(parts);
    Some(snapshot)
}
