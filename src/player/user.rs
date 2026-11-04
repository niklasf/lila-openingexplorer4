use std::{fmt, str::FromStr};

use sha2::{Digest, Sha256};
use thiserror::Error;

/// A lichess user id: the lowercase user name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserId(Box<str>);

#[derive(Debug, Error)]
#[error("invalid user name")]
pub struct InvalidUserName;

impl UserId {
    /// Longest valid user name.
    pub const MAX_LEN: usize = 30;

    /// The id of a user name (case-insensitive): 1 to 30 ASCII letters,
    /// digits, `_` and `-`.
    pub fn from_name(name: &str) -> Result<UserId, InvalidUserName> {
        if name.is_empty()
            || name.len() > UserId::MAX_LEN
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            return Err(InvalidUserName);
        }
        Ok(UserId(name.to_ascii_lowercase().into_boxed_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the name (as in game records) is the name of this user.
    pub fn is_name(&self, name: &str) -> bool {
        self.0.eq_ignore_ascii_case(name)
    }

    pub fn hash(&self) -> UserHash {
        UserHash::new(self)
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for UserId {
    type Err = InvalidUserName;

    fn from_str(s: &str) -> Result<UserId, InvalidUserName> {
        UserId::from_name(s)
    }
}

/// Stable 64-bit hash of a user id, the first part of every key of the
/// player's data, so that a player is one contiguous key range.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserHash([u8; UserHash::SIZE]);

impl UserHash {
    pub const SIZE: usize = 8;

    /// The first 8 bytes of the SHA-256 digest of the user id.
    pub fn new(user: &UserId) -> UserHash {
        let digest = Sha256::digest(user.as_str().as_bytes());
        UserHash(digest[..UserHash::SIZE].try_into().expect("8 bytes"))
    }

    pub fn from_bytes(bytes: [u8; UserHash::SIZE]) -> UserHash {
        UserHash(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; UserHash::SIZE] {
        &self.0
    }

    /// The hash as a number, for deterministic pseudo-random decisions.
    pub fn to_u64(self) -> u64 {
        u64::from_be_bytes(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_id() {
        let id = UserId::from_name("Foo_Bar-1").unwrap();
        assert_eq!(id.as_str(), "foo_bar-1");
        assert!(id.is_name("FOO_bar-1"));
        assert!(UserId::from_name("").is_err());
        assert!(UserId::from_name("a b").is_err());
        assert!(UserId::from_name(&"a".repeat(31)).is_err());
        assert_eq!(id.hash(), UserId::from_name("foo_BAR-1").unwrap().hash());
        assert_ne!(id.hash(), UserId::from_name("foo_bar-2").unwrap().hash());
    }

    #[test]
    fn test_user_hash_is_stable() {
        // Part of the database format: must never change.
        assert_eq!(
            UserId::from_name("revoof").unwrap().hash().as_bytes(),
            &Sha256::digest(b"revoof")[..8]
        );
    }
}
