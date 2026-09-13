//! Optional APFS data-stream evidence, never a digest or reclaimable-byte estimate.
//! ATTR_CMNEXT_CLONEID is documented by Apple's getattrlist(2): equal nonzero
//! IDs on the same device identify pure clones. Different IDs do not prove that
//! no extents are shared. Unsupported/missing attributes remain unknown.
use std::fs::File;

#[cfg(target_os = "macos")]
pub(crate) fn read(file: &File) -> Option<u64> {
    use std::os::fd::AsRawFd;
    let mut attributes = libc::attrlist {
        bitmapcount: 5,
        reserved: 0,
        commonattr: 0x8000_0000, // ATTR_CMN_RETURNED_ATTRS
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0x100, // ATTR_CMNEXT_CLONEID, sys/attr.h
    };
    let mut bytes = [0u8; 32]; // length + five returned masks + u64 clone ID
    // fgetattrlist borrows an already identity-checked, no-follow descriptor.
    // The kernel writes at most the supplied buffer length; parsing below uses
    // checked slices, not references to packed or unaligned integers.
    let result = unsafe {
        libc::fgetattrlist(file.as_raw_fd(), (&mut attributes as *mut libc::attrlist).cast(),
            bytes.as_mut_ptr().cast(), bytes.len(), 0x20) // FSOPT_ATTR_CMN_EXTENDED
    };
    (result == 0).then(|| decode(&bytes)).flatten()
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn read(_file: &File) -> Option<u64> { None }

#[cfg(any(test, target_os = "macos"))]
fn decode(bytes: &[u8]) -> Option<u64> {
    let word = |offset| Some(u32::from_ne_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?));
    let length = word(0)? as usize;
    if length != 32 || bytes.len() < length || word(4)? != 0x8000_0000
        || word(8)? != 0 || word(12)? != 0 || word(16)? != 0 || word(20)? != 0x100 { return None; }
    let clone = u64::from_ne_bytes(bytes.get(24..32)?.try_into().ok()?);
    (clone != 0).then_some(clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn returned_masks_and_all_truncated_buffers_fail_closed() {
        let mut bytes = [0u8; 32];
        bytes[0..4].copy_from_slice(&32u32.to_ne_bytes());
        bytes[4..8].copy_from_slice(&0x8000_0000u32.to_ne_bytes());
        bytes[20..24].copy_from_slice(&0x100u32.to_ne_bytes());
        bytes[24..32].copy_from_slice(&77u64.to_ne_bytes());
        assert_eq!(decode(&bytes), Some(77));
        for length in 0..32 { assert_eq!(decode(&bytes[..length]), None); }
        bytes[20..24].fill(0);
        assert_eq!(decode(&bytes), None, "unreturned data is not evidence");
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_clonefile_shares_data_stream_but_not_inode() {
        use std::{ffi::CString, os::unix::fs::MetadataExt};
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let clone = temporary.path().join("clone");
        std::fs::write(&source, vec![91u8; 16_384]).unwrap();
        let a = CString::new(source.to_str().unwrap()).unwrap();
        let b = CString::new(clone.to_str().unwrap()).unwrap();
        // This macOS/APFS fixture is deliberately required on the target CI.
        assert_eq!(unsafe { libc::clonefile(a.as_ptr(), b.as_ptr(), 0) }, 0,
            "APFS clone fixture: {}", std::io::Error::last_os_error());
        let first = File::open(&source).unwrap();
        let second = File::open(&clone).unwrap();
        assert_ne!(first.metadata().unwrap().ino(), second.metadata().unwrap().ino());
        let identifier = read(&first).expect("APFS must return clone identity");
        assert_eq!(read(&second), Some(identifier));
    }
}
