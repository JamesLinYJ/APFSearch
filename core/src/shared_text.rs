//! Shared normalized text for owned I/O records. Search snapshots store their
//! text in column blocks and do not retain this per-value ownership wrapper.
use std::{
    borrow::Borrow,
    cmp::Ordering,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::Arc,
};
#[derive(Clone, Default)]
pub struct SharedText(Arc<str>);
impl SharedText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn ptr_eq(left: &Self, right: &Self) -> bool {
        Arc::ptr_eq(&left.0, &right.0)
    }
}
impl Deref for SharedText {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
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
    fn from(value: String) -> Self {
        Self(value.into())
    }
}
impl From<&str> for SharedText {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}
impl serde::Serialize for SharedText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self)
    }
}
impl<'de> serde::Deserialize<'de> for SharedText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <String as serde::Deserialize>::deserialize(deserializer).map(Into::into)
    }
}
impl rusqlite::types::FromSql for SharedText {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value.as_str().map(Into::into)
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
        self.as_ref().hash(state)
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
