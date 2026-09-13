//! Conservative proof for resuming a local per-host event stream. Auxiliary
//! volumes without the boot volume's history are re-enumerated separately.
use crate::{file_events, filesystem, scanner};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeProof {
    version: u32,
    boot_session: String,
    journal_uuid: String,
    roots: Vec<(String, u64, u64)>,
    mounts: Vec<(String, String, u32)>,
    mount_types: Vec<(String, String)>,
    access: Vec<(String, bool)>,
}
impl ResumeProof {
    pub(crate) fn rejection_reason(&self, other: &Self) -> Option<&'static str> {
        if self.version != other.version {
            Some("proof_version_changed")
        } else if self.boot_session != other.boot_session {
            Some("boot_session_changed")
        } else if self.journal_uuid != other.journal_uuid {
            Some("journal_identity_changed")
        } else if self.roots != other.roots {
            Some("root_configuration_or_identity_changed")
        } else {
            None
        }
    }
    pub(crate) fn same_namespace(&self, other: &Self) -> bool {
        self.rejection_reason(other).is_none()
    }
    /// A completed baseline may consume old mount controls only while the
    /// inspected mount namespace is still the one that was reconciled. Access
    /// probes and incidental mount flags are not mount-identity changes.
    pub(crate) fn same_mount_namespace(&self, other: &Self) -> bool {
        let relevant_flags =
            libc::MNT_LOCAL as u32 | libc::MNT_SNAPSHOT as u32 | libc::MNT_IGNORE_OWNERSHIP as u32;
        self.same_namespace(other)
            && self.mount_types == other.mount_types
            && self.mounts.len() == other.mounts.len()
            && self.mounts.iter().zip(&other.mounts).all(
                |((path, source, flags), (other_path, other_source, other_flags))| {
                    path == other_path
                        && source == other_source
                        && flags & relevant_flags == other_flags & relevant_flags
                },
            )
    }
    pub(crate) fn summary(&self) -> serde_json::Value {
        serde_json::json!({"version":self.version,"boot_session":self.boot_session,
            "journal_uuid":self.journal_uuid,"roots":self.roots,
            "mount_count":self.mounts.len(),"access_probe_count":self.access.len()})
    }
}
pub(crate) struct Inspection {
    pub(crate) proof: ResumeProof,
    pub(crate) recheck_roots: Vec<String>,
}
impl Inspection {
    /// Runtime access and mount changes invalidate their intersecting scopes,
    /// not the per-host journal or every configured root.
    pub(crate) fn recheck_against(&self, previous: &ResumeProof) -> Vec<String> {
        use std::collections::BTreeMap;
        let configured: Vec<_> = self
            .proof
            .roots
            .iter()
            .map(|(root, _, _)| root.clone())
            .collect();
        let selected = scanner::PathScopes::from_paths(&configured);
        let mut recheck = self.recheck_roots.clone();
        let old_access: BTreeMap<_, _> = previous.access.iter().cloned().collect();
        let new_access: BTreeMap<_, _> = self.proof.access.iter().cloned().collect();
        for path in old_access
            .keys()
            .chain(new_access.keys())
            .collect::<BTreeSet<_>>()
        {
            if !selected.covers(Path::new(path)) || !scanner::path_in_namespace(path, &configured) {
                continue;
            }
            // A probe removed from the dynamic uncovered set is not an access
            // revocation. Read its current status before choosing a recheck.
            let current = new_access
                .get(path)
                .copied()
                .unwrap_or_else(|| scanner::scope_accessible(path));
            if old_access
                .get(path)
                .is_none_or(|previous| *previous != current)
            {
                recheck.push(path.clone());
            }
        }
        // These flags affect index eligibility. Read-only, noexec, quota and
        // other mount bookkeeping flags do not invalidate filename coverage.
        let relevant_flags =
            libc::MNT_LOCAL as u32 | libc::MNT_SNAPSHOT as u32 | libc::MNT_IGNORE_OWNERSHIP as u32;
        let mount_map = |proof: &ResumeProof| -> BTreeMap<String, (String, u32)> {
            proof
                .mounts
                .iter()
                .map(|(path, source, flags)| {
                    (path.clone(), (source.clone(), flags & relevant_flags))
                })
                .collect()
        };
        let old_mounts = mount_map(previous);
        let new_mounts = mount_map(&self.proof);
        for path in old_mounts
            .keys()
            .chain(new_mounts.keys())
            .collect::<BTreeSet<_>>()
        {
            if old_mounts.get(path) != new_mounts.get(path) {
                recheck.push(path.clone());
            }
        }
        let old_types: BTreeMap<_, _> = previous.mount_types.iter().cloned().collect();
        let new_types: BTreeMap<_, _> = self.proof.mount_types.iter().cloned().collect();
        for (path, kind) in &new_types {
            if old_types.get(path) != Some(kind) {
                recheck.push(path.clone());
            }
        }
        let mut intersecting = Vec::new();
        for changed in recheck {
            if !scanner::path_in_namespace(&changed, &configured) {
                continue;
            }
            for (root, _, _) in &self.proof.roots {
                if Path::new(&changed).starts_with(root) {
                    intersecting.push(changed.clone());
                } else if Path::new(root).starts_with(&changed) {
                    intersecting.push(root.clone());
                }
            }
        }
        scanner::normalize_scopes(&intersecting)
    }
}
/// Opening a few directory handles checks configured roots, previously denied
/// paths and standard macOS privacy boundaries; this never reads file contents.
pub(crate) fn inspect(roots: &[String], uncovered: &[String]) -> Result<Inspection, String> {
    inspect_inner(roots, uncovered).map_err(|error| error.to_string())
}
fn inspect_inner(roots: &[String], uncovered: &[String]) -> std::io::Result<Inspection> {
    let scope = scanner::mount_scope()?;
    let selected = scanner::PathScopes::from_paths(roots);
    let boot_device = std::fs::symlink_metadata("/")?.dev();
    let journal_uuid = file_events::journal_uuid(Path::new("/"))?
        .ok_or_else(|| std::io::Error::other("Boot-volume event history is unavailable"))?;
    let mut root_ids = Vec::new();
    let mut probes = BTreeSet::new();
    // Cancellation may have committed some newly observed rows before its
    // coverage-finish phase. Retry known gaps so unchanged denied probes still
    // hide any such rows; a denied directory fails promptly without traversal.
    let mut recheck: Vec<_> = uncovered
        .iter()
        .filter(|path| selected.covers(Path::new(path)) && scanner::path_in_namespace(path, roots))
        .cloned()
        .collect();
    for root in roots {
        if !scope.allows(Path::new(root)) {
            return Err(std::io::Error::other(
                "Configured root is not on a supported local mount",
            ));
        }
        let metadata = filesystem::metadata(Path::new(root), &scope)?;
        root_ids.push((root.clone(), metadata.st_dev as u64, metadata.st_ino));
        probes.insert(root.clone());
        if metadata.st_dev as u64 != boot_device {
            recheck.push(root.clone());
        }
    }
    root_ids.sort();
    let mut mounts = Vec::new();
    let mut mount_types = Vec::new();
    for mounted in filesystem::mounted_filesystems()? {
        let raw_path = filesystem::c_array_string(&mounted.f_mntonname)?
            .to_str()
            .map_err(|_| std::io::Error::other("Non-UTF8 mount path"))?;
        let path = scanner::visible_path(raw_path);
        if !scanner::path_in_namespace(&path, roots)
            || !roots.iter().any(|root| {
                Path::new(root).starts_with(&path) || Path::new(&path).starts_with(root)
            })
        {
            continue;
        }
        let source = filesystem::c_array_string(&mounted.f_mntfromname)?
            .to_string_lossy()
            .into_owned();
        mount_types.push((
            path.clone(),
            filesystem::c_array_string(&mounted.f_fstypename)?
                .to_string_lossy()
                .into_owned(),
        ));
        mounts.push((path.clone(), source, mounted.f_flags));
        let mount_device = if scope.allows(Path::new(raw_path)) {
            Some(filesystem::metadata(Path::new(raw_path), &scope)?.st_dev as u64)
        } else {
            None
        };
        if mount_device.is_some_and(|device| device != boot_device) {
            for root in roots {
                if Path::new(&path).starts_with(root) {
                    recheck.push(path.clone());
                } else if Path::new(root).starts_with(&path) {
                    recheck.push(root.clone());
                }
            }
        }
    }
    mounts.sort();
    mounts.dedup();
    mount_types.sort();
    mount_types.dedup();
    probes.extend(
        uncovered
            .iter()
            .filter(|path| {
                selected.covers(Path::new(path)) && scanner::path_in_namespace(path, roots)
            })
            .cloned(),
    );
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for suffix in [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Movies",
            "Music",
            "Library/Safari",
            "Library/Mail",
            "Library/Messages",
            "Library/Calendars",
            "Library/HomeKit",
            "Library/Containers",
            "Library/Group Containers",
            "Library/Application Support/com.apple.TCC",
            "Pictures/Photos Library.photoslibrary",
        ] {
            let path = home.join(suffix);
            if roots
                .iter()
                .any(|root| path.starts_with(root) || Path::new(root).starts_with(&path))
            {
                probes.insert(path.to_string_lossy().into_owned());
            }
        }
    }
    let access = probes
        .into_iter()
        .filter(|path| scope.allows(Path::new(path)))
        .map(|path| {
            let readable = scanner::scope_accessible(&path);
            if readable && uncovered.contains(&path) {
                recheck.push(path.clone());
            }
            (path, readable)
        })
        .collect();
    Ok(Inspection {
        proof: ResumeProof {
            version: 1,
            boot_session: file_events::boot_session()?,
            journal_uuid,
            roots: root_ids,
            mounts,
            mount_types,
            access,
        },
        recheck_roots: scanner::normalize_scopes(&recheck),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn proof() -> ResumeProof {
        ResumeProof {
            version: 1,
            boot_session: "boot".into(),
            journal_uuid: "journal".into(),
            roots: vec![("/fixture".into(), 1, 2)],
            mounts: vec![("/".into(), "/dev/disk1".into(), libc::MNT_LOCAL as u32)],
            mount_types: vec![("/".into(), "apfs".into())],
            access: vec![("/fixture".into(), true)],
        }
    }
    #[test]
    fn resume_scopes_do_not_expand_the_default_boot_namespace() {
        let mut before = proof();
        before.roots = vec![("/".into(), 1, 2)];
        before.access = vec![("/System/Volumes/Preboot/denied".into(), false)];
        before.mounts.push((
            "/System/Volumes/Preboot".into(),
            "/dev/aux".into(),
            libc::MNT_LOCAL as u32,
        ));
        let mut current = before.clone();
        current.access.clear();
        current.mounts.pop();
        let inspection = Inspection {
            proof: current,
            recheck_roots: vec![
                "/System/Volumes/Preboot".into(),
                "/Library/Developer/CoreSimulator/Volumes/runtime".into(),
            ],
        };
        assert!(before.same_namespace(&inspection.proof));
        assert_eq!(
            inspection.recheck_against(&before),
            ["/Library/Developer/CoreSimulator/Volumes/runtime"]
        );
        assert!(inspection.proof.same_mount_namespace(&inspection.proof));
    }
    #[test]
    fn historical_mount_consumption_requires_the_reconciled_mount_namespace() {
        let before = proof();
        assert!(before.same_mount_namespace(&before));
        let mut incidental = before.clone();
        incidental.access[0].1 = false;
        incidental.mounts[0].2 |= libc::MNT_NOEXEC as u32;
        assert!(before.same_mount_namespace(&incidental));
        for change in 0..6 {
            let mut different = before.clone();
            match change {
                0 => different.boot_session = "other-boot".into(),
                1 => different.journal_uuid = "other-journal".into(),
                2 => different.roots[0].2 += 1,
                3 => different.mounts[0].1 = "/dev/disk2".into(),
                4 => different.mounts[0].2 ^= libc::MNT_LOCAL as u32,
                5 => different.mount_types[0].1 = "hfs".into(),
                _ => unreachable!(),
            }
            assert!(!before.same_mount_namespace(&different), "change={change}");
        }
        let mut mounted = before.clone();
        mounted
            .mounts
            .push(("/fixture/covered".into(), "remote".into(), 0));
        assert!(!before.same_mount_namespace(&mounted));
        assert!(!mounted.same_mount_namespace(&before));
    }
    #[test]
    fn runtime_access_and_probe_set_changes_do_not_invalidate_history() {
        let before = proof();
        let mut current = before.clone();
        current.access.push(("/fixture/new-gap".into(), false));
        let inspection = Inspection {
            proof: current,
            recheck_roots: Vec::new(),
        };
        assert!(before.same_namespace(&inspection.proof));
        assert_eq!(inspection.recheck_against(&before), ["/fixture/new-gap"]);
        let mut current = before.clone();
        current.access[0].1 = false;
        let inspection = Inspection {
            proof: current,
            recheck_roots: Vec::new(),
        };
        assert!(before.same_namespace(&inspection.proof));
        assert_eq!(inspection.recheck_against(&before), ["/fixture"]);
    }
    #[test]
    fn mount_add_remove_and_cover_changes_recheck_only_intersections() {
        let before = proof();
        let mut mounted = before.clone();
        mounted
            .mounts
            .push(("/fixture/mount".into(), "smb://server/share".into(), 0));
        let current = Inspection {
            proof: mounted.clone(),
            recheck_roots: Vec::new(),
        };
        assert!(before.same_namespace(&current.proof));
        assert_eq!(current.recheck_against(&before), ["/fixture/mount"]);
        let unmounted = Inspection {
            proof: before,
            recheck_roots: Vec::new(),
        };
        assert_eq!(unmounted.recheck_against(&mounted), ["/fixture/mount"]);
        // Reusing the same device name and flags must not conceal a type change.
        let mut old_type = mounted.clone();
        old_type
            .mount_types
            .push(("/fixture/mount".into(), "apfs".into()));
        let mut new_type = old_type.clone();
        new_type.mount_types.last_mut().unwrap().1 = "hfs".into();
        let changed_type = Inspection {
            proof: new_type,
            recheck_roots: Vec::new(),
        };
        assert_eq!(changed_type.recheck_against(&old_type), ["/fixture/mount"]);
    }
    #[test]
    fn unrelated_mount_flags_do_not_rescan_the_root_namespace() {
        let before = proof();
        let mut current = before.clone();
        current.mounts[0].2 |= libc::MNT_RDONLY as u32 | libc::MNT_NOEXEC as u32;
        let inspection = Inspection {
            proof: current,
            recheck_roots: Vec::new(),
        };
        assert!(before.same_namespace(&inspection.proof));
        assert!(inspection.recheck_against(&before).is_empty());
    }
    #[test]
    fn stable_history_anchor_mismatches_have_explicit_rejection_reasons() {
        let before = proof();
        let mut current = before.clone();
        current.boot_session = "other".into();
        assert_eq!(
            before.rejection_reason(&current),
            Some("boot_session_changed")
        );
        current = before.clone();
        current.journal_uuid = "other".into();
        assert_eq!(
            before.rejection_reason(&current),
            Some("journal_identity_changed")
        );
        current = before.clone();
        current.roots[0].2 += 1;
        assert_eq!(
            before.rejection_reason(&current),
            Some("root_configuration_or_identity_changed")
        );
    }
}
