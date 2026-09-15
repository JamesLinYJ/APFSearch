use super::*;
#[test]
fn unchanged_mount_tables_share_materialized_context_without_sharing_mutations() {
    let filesystems = mounted_filesystems().unwrap();
    let first = MountScope::from_current_filesystems(&filesystems).unwrap();
    let second = MountScope::from_current_filesystems(&filesystems).unwrap();
    assert_eq!(first.identity(), second.identity());
    let second = first.clone();
    assert!(Arc::ptr_eq(&first.mounts, &second.mounts));
    let original = first.mounts.clone();
    let mapped = first.with_aliases(std::iter::once((
        PathBuf::from("/"),
        PathBuf::from("/test-alias"),
    )));
    assert!(Arc::ptr_eq(&original, &second.mounts));
    assert!(!Arc::ptr_eq(&mapped.mounts, &second.mounts));
    assert!(second.aliases.is_empty());
}

#[test]
fn changed_mount_identity_invalidates_materialized_policy() {
    let mut filesystems = mounted_filesystems().unwrap();
    let before = MountScope::from_current_filesystems(&filesystems).unwrap();
    let mount = c_array_string(&filesystems[0].f_mntonname)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    filesystems[0].f_flags &= !(libc::MNT_LOCAL as u32);
    let after = MountScope::from_current_filesystems(&filesystems).unwrap();
    assert_ne!(before.identity(), after.identity());
    assert!(!after.allows(Path::new(&mount)));
}
#[test]
fn excluded_mounts_are_rejected_before_any_path_io() {
    let scope = MountScope::from_mounts(vec![
        (PathBuf::from("/"), true),
        (PathBuf::from("/does-not-exist/remote"), false),
    ]);
    let path = Path::new("/does-not-exist/remote/missing");
    // ENOTSUP proves scope refusal happened before lstat/open (which would
    // return ENOENT). No remote or privileged fixture is needed.
    assert_eq!(
        stat_entry(path, &scope).unwrap_err().raw_os_error(),
        Some(libc::ENOTSUP)
    );
    assert_eq!(
        scan_directory(path, &scope, |_, _| panic!("Excluded callback"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOTSUP)
    );
}
#[test]
fn physical_and_logical_mount_aliases_share_one_scope_boundary() {
    let scope = MountScope::from_mounts(vec![
        (PathBuf::from("/"), true),
        (PathBuf::from("/Volumes/Network"), false),
        (PathBuf::from("/System/Volumes/Data/home"), false),
    ])
    .with_aliases(
        [
            (
                PathBuf::from("/System/Volumes/Data/Volumes"),
                PathBuf::from("/Volumes"),
            ),
            (
                PathBuf::from("/System/Volumes/Data/home"),
                PathBuf::from("/home"),
            ),
        ]
        .into_iter(),
    );
    for path in [
        "/Volumes/Network/file",
        "/System/Volumes/Data/Volumes/Network/file",
        "/home/user",
        "/System/Volumes/Data/home/user",
    ] {
        assert!(
            !scope.allows(Path::new(path)),
            "Alias escaped mount scope: {path}"
        );
    }
    assert!(scope.allows(Path::new("/Volumes/Network-copy/file")));
    assert!(scope.allows(Path::new("/Users/local/file")));
}
fn record(name: &[u8], kind: u32, size: i64) -> Vec<u8> {
    let mut bytes = vec![0u8; 4];
    let common = libc::ATTR_CMN_FILEID | libc::ATTR_CMN_NAME;
    bytes.extend(common.to_ne_bytes());
    bytes.extend([0u8; 16]);
    bytes.extend(0u32.to_ne_bytes());
    let reference = bytes.len();
    bytes.extend([0u8; 8]);
    bytes.extend(3u32.to_ne_bytes());
    bytes.extend(kind.to_ne_bytes());
    for seconds in [100i64, 200, 300] {
        bytes.extend(seconds.to_ne_bytes());
        bytes.extend(123i64.to_ne_bytes());
    }
    bytes.extend(0u32.to_ne_bytes());
    bytes.extend(42u64.to_ne_bytes());
    bytes.extend(7u64.to_ne_bytes());
    if kind == 2 {
        bytes.extend(0u32.to_ne_bytes());
    } else {
        // PACK_INVAL_ATTRS reserves requested LINKCOUNT even with its mask unset.
        bytes.extend(0u32.to_ne_bytes());
        bytes.extend(size.to_ne_bytes());
    }
    let name_start = bytes.len();
    bytes.extend(name);
    bytes.push(0);
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
    let length = bytes.len() as u32;
    bytes[..4].copy_from_slice(&length.to_ne_bytes());
    bytes[reference..reference + 4]
        .copy_from_slice(&((name_start - reference) as i32).to_ne_bytes());
    bytes[reference + 4..reference + 8].copy_from_slice(&((name.len() + 1) as u32).to_ne_bytes());
    bytes
}
#[test]
fn regular_zero_size_and_directory_records_use_different_packed_layouts() {
    for (kind, size) in [(1, 0), (1, 1024), (2, 0), (5, 12)] {
        let bytes = record("中文.txt".as_bytes(), kind, size);
        let parsed = parse_record(&bytes, "volume").unwrap();
        assert_eq!(parsed.name, "中文.txt".as_bytes());
        assert_eq!(parsed.entry.kind, FileKind::from(kind));
        assert_eq!(parsed.entry.size, size as u64);
        assert_eq!(parsed.entry.file_id, 42);
        assert_eq!(parsed.entry.modified_ns, 200_000_000_123);
    }
}
#[test]
fn every_truncated_prefix_and_oversized_record_is_rejected() {
    let bytes = record(b"file", 1, 0);
    for end in 0..bytes.len() {
        assert!(
            parse_record(&bytes[..end], "volume").is_err(),
            "prefix {end}"
        );
    }
    let mut oversized = bytes.clone();
    oversized[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
    assert!(parse_batch(&oversized, 1, "volume", |_| Ok(())).is_err());
    assert!(parse_batch(&bytes, 2, "volume", |_| Ok(())).is_err());
}
#[test]
fn relative_names_are_bounded_and_must_be_one_terminated_string() {
    let original = record(b"file", 1, 0);
    let reference = 28;
    for offset in [i32::MIN, -1, 0, 1, i32::MAX] {
        let mut bytes = original.clone();
        bytes[reference..reference + 4].copy_from_slice(&offset.to_ne_bytes());
        assert!(parse_record(&bytes, "volume").is_err());
    }
    for length in [0, u32::MAX] {
        let mut bytes = original.clone();
        bytes[reference + 4..reference + 8].copy_from_slice(&length.to_ne_bytes());
        assert!(parse_record(&bytes, "volume").is_err());
    }
    let name_start = reference
        + i32::from_ne_bytes(original[reference..reference + 4].try_into().unwrap()) as usize;
    let mut bytes = original.clone();
    bytes[name_start + 4] = b'x';
    assert!(parse_record(&bytes, "volume").is_err());
    let mut bytes = original.clone();
    bytes[name_start + 1] = 0;
    assert!(parse_record(&bytes, "volume").is_err());
}
#[test]
fn batches_do_not_cross_record_boundaries_and_preserve_non_utf8_for_reporting() {
    let mut bytes = record(b"one", 1, 1);
    bytes.extend(record(b"two", 2, 0));
    let mut names = vec![];
    parse_batch(&bytes, 2, "volume", |r| {
        names.push(r.name.to_vec());
        Ok(())
    })
    .unwrap();
    assert_eq!(names, vec![b"one".to_vec(), b"two".to_vec()]);
    let bytes = record(&[0xff, b'a'], 1, 0);
    assert_eq!(parse_record(&bytes, "volume").unwrap().name, &[0xff, b'a']);
}
#[test]
fn malformed_kernel_bytes_never_panic() {
    let mut seed = 0x12345678u64;
    for length in 0..512 {
        let mut bytes = vec![0u8; length];
        for byte in &mut bytes {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *byte = seed as u8;
        }
        if length >= 4 {
            bytes[..4].copy_from_slice(&(length as u32).to_ne_bytes());
        }
        let _ = parse_record(&bytes, "volume");
        let _ = parse_batch(&bytes, 3, "volume", |_| Ok(()));
    }
}

#[test]
fn metadata_stat_rejects_intermediate_symlinks_but_preserves_the_final_link() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let target = root.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("private.txt"), "private").unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let scope = MountScope::load().unwrap();
    assert!(stat_entry(&alias.join("private.txt"), &scope).is_err());
    assert_eq!(stat_entry(&alias, &scope).unwrap().kind, FileKind::Symlink);
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink("missing", &alias).unwrap();
    assert_eq!(stat_entry(&alias, &scope).unwrap().kind, FileKind::Symlink);
}

#[test]
fn descriptor_metadata_preserves_parent_ids_for_distinct_hardlink_entries() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let one = root.join("one");
    let two = root.join("two");
    std::fs::create_dir(&one).unwrap();
    std::fs::create_dir(&two).unwrap();
    std::fs::write(one.join("file"), "payload").unwrap();
    std::fs::hard_link(one.join("file"), two.join("file")).unwrap();
    let scope = MountScope::load().unwrap();
    let mut first = MetadataReader::new(&scope);
    let mut second = MetadataReader::new(&scope);
    let left = first.stat(&one.join("file")).unwrap();
    let right = second.stat(&two.join("file")).unwrap();
    assert_eq!(left.parent_id, std::fs::metadata(&one).unwrap().ino());
    assert_eq!(right.parent_id, std::fs::metadata(&two).unwrap().ino());
    assert_eq!(
        first.stat(&one.join("file")).unwrap().parent_id,
        left.parent_id
    );
    assert_eq!(left.file_id, right.file_id);
    assert_eq!(left.volume_id, right.volume_id);
    assert_ne!(left.parent_id, right.parent_id);
    assert_eq!(left.link_count, Some(2));
}

#[test]
fn metadata_identity_does_not_require_access_to_the_leaf_contents() {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("denied");
    std::fs::create_dir(&path).unwrap();
    let scope = MountScope::load().unwrap();
    let before = metadata(&path, &scope).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let identity = metadata(&path, &scope);
    let entry = stat_entry(&path, &scope);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(identity.unwrap().st_ino, before.st_ino);
    assert_eq!(entry.unwrap().file_id, before.st_ino);
}

#[test]
fn reused_metadata_parent_cannot_be_retargeted_to_an_outside_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let parent = root.join("selected");
    let outside = root.join("outside");
    std::fs::create_dir(&parent).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(parent.join("known.txt"), "selected").unwrap();
    std::fs::write(outside.join("private.txt"), "outside").unwrap();
    let scope = MountScope::load().unwrap();
    let mut reader = MetadataReader::new(&scope);
    let before = reader.stat(&parent.join("known.txt")).unwrap();
    std::fs::rename(&parent, root.join("original-directory")).unwrap();
    std::os::unix::fs::symlink(&outside, &parent).unwrap();
    assert!(reader.stat(&parent.join("private.txt")).is_err());
    assert_eq!(
        reader.stat(&parent.join("known.txt")).unwrap().file_id,
        before.file_id
    );
    assert!(stat_entry(&parent.join("known.txt"), &scope).is_err());
}
