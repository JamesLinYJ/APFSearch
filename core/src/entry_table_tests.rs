use super::*;
use serde_json::json;

fn file(id: i64) -> IndexedFile {
    let mut file: IndexedFile = serde_json::from_value(json!({
        "id":id,"path":format!("/fixture/目录/Café-{id}.txt"),"name":format!("Café-{id}.txt"),
        "extension":"txt","volume_id":"fixture","size":u64::MAX,"modified":i64::MIN,
        "created":i64::MAX,"changed":0,"modified_ns":i64::MIN,"changed_ns":i64::MAX,
        "file_id":u64::MAX,"parent_id":u64::MAX-1,"flags":u32::MAX,
        "is_dir":false,"is_symlink":false,"content_indexed":false,"properties":{}
    }))
    .unwrap();
    file.prepare();
    file
}

#[test]
fn metadata_updates_copy_only_changed_columns_and_preserve_other_blocks() {
    let original =
        EntryTable::from_rows((0..CHUNK_LENGTH * 2 + 1).map(|id| Ok(file(id as i64)))).unwrap();
    let mut updated = original.clone();
    let mut row = updated.at(CHUNK_LENGTH).to_owned_file();
    row.size = 7;
    updated.set(CHUNK_LENGTH, row);
    updated.finish_update();
    assert!(original.shares_block(&updated, 0));
    assert!(original.shares_block(&updated, CHUNK_LENGTH * 2));
    assert!(!original.shares_block(&updated, CHUNK_LENGTH));
    let before = &original.chunks[1];
    let after = &updated.chunks[1];
    assert!(!Arc::ptr_eq(&before.size.0, &after.size.0));
    assert!(Arc::ptr_eq(&before.id.0, &after.id.0));
    assert!(Arc::ptr_eq(&before.modified.0, &after.modified.0));
    assert!(Arc::ptr_eq(&before.path.0, &after.path.0));
    assert!(Arc::ptr_eq(&before.text, &after.text));
    assert_eq!(original.at(CHUNK_LENGTH).size(), u64::MAX);
    assert_eq!(updated.at(CHUNK_LENGTH).size(), 7);
}

#[test]
fn integer_extremes_and_sparse_json_roundtrip_without_precision_loss() {
    let mut rows: Vec<_> = (0..4).map(file).collect();
    rows[1].properties = Value::Null;
    rows[2].properties = json!({"width":9007199254740993u64,"tags":["中文",null]});
    rows[3].is_dir = true;
    rows[3].is_symlink = true;
    rows[3].content_indexed = true;
    let expected = serde_json::to_value(&rows).unwrap();
    let table = EntryTable::from_rows(rows.into_iter().map(Ok)).unwrap();
    assert_eq!(table.chunks[0].properties.len(), 1);
    assert_eq!(serde_json::to_value(&table).unwrap(), expected);
    assert_eq!(
        Value::Array(table.iter().map(|row| row.to_json()).collect()),
        expected
    );
    assert_eq!(table.at(2).modified_ns(), i64::MIN);
    assert_eq!(table.at(2).file_id(), u64::MAX);
}

#[test]
fn repeated_renames_reclaim_unreferenced_text_without_mutating_readers() {
    let original = EntryTable::from_rows([Ok(file(1))]).unwrap();
    let mut updated = original.clone();
    for iteration in 0..100 {
        let mut row = file(1);
        row.path = format!("/fixture/{iteration}/{}", row.name);
        row.prepare();
        updated.set(0, row);
        updated.finish_update();
        assert!(updated.chunks[0].text.text().len() < 1024);
    }
    assert_eq!(original.at(0).path(), "/fixture/目录/Café-1.txt");
    assert_eq!(updated.at(0).path(), "/fixture/99/Café-1.txt");
}

#[test]
fn mapped_columns_reject_unaligned_or_incomplete_ranges() {
    let mapping = Arc::new(
        memmap2::MmapMut::map_anon(128)
            .unwrap()
            .make_read_only()
            .unwrap(),
    );
    assert!(Column::<u64>::mapped(mapping.clone(), 1..9).is_none());
    assert!(Column::<u64>::mapped(mapping.clone(), 8..17).is_none());
    assert!(Column::<u64>::mapped(mapping.clone(), 120..136).is_none());
    assert_eq!(
        Column::<u64>::mapped(mapping, 8..24).unwrap().values(),
        [0, 0]
    );
}

#[test]
fn appended_rows_share_directory_prefixes_and_release_update_dictionary() {
    let original = EntryTable::from_rows([Ok(file(0))]).unwrap();
    let mut updated = original.clone();
    for id in 1..256 {
        updated.push(file(id));
    }
    updated.finish_update();
    let chunk = &updated.chunks[0];
    assert!(chunk.pending_text.is_none());
    assert!(
        chunk
            .path
            .values()
            .iter()
            .all(|path| path.prefix == chunk.path.values()[0].prefix)
    );
    assert!(
        chunk
            .search_path
            .values()
            .iter()
            .all(|path| path.prefix == chunk.search_path.values()[0].prefix)
    );
    let reconstructed = EntryTable::from_rows((0..256).map(|id| Ok(file(id)))).unwrap();
    assert_eq!(
        chunk.text.text().len(),
        reconstructed.chunks[0].text.text().len()
    );
    assert_eq!(
        serde_json::to_value(&updated).unwrap(),
        serde_json::to_value(&reconstructed).unwrap()
    );
    assert_eq!(original.len(), 1);
    assert_eq!(original.at(0).path(), "/fixture/目录/Café-0.txt");
}
