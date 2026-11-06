use std::{fmt, fs, io, path::Path, str::FromStr};

use hmac::{Hmac, Mac};
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

    /// A stable, public hash of the user id, for deterministic
    /// pseudo-random decisions (like those of the mock lila) that must not
    /// depend on the secret of a database. Not for keys (see `UserHasher`).
    pub fn fingerprint(&self) -> u64 {
        let digest = Sha256::digest(self.as_str().as_bytes());
        u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"))
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

/// Keyed 64-bit hash of a user id, the first part of every key of the
/// player's data, so that a player is one contiguous key range. From the
/// `UserHasher` of the database.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserHash([u8; UserHash::SIZE]);

impl UserHash {
    pub const SIZE: usize = 8;

    pub fn from_bytes(bytes: [u8; UserHash::SIZE]) -> UserHash {
        UserHash(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; UserHash::SIZE] {
        &self.0
    }

    /// The hash as a number, for partitioning.
    pub fn to_u64(self) -> u64 {
        u64::from_be_bytes(self.0)
    }
}

/// Hashes user ids with a secret key (HMAC-SHA-256, truncated), so that
/// nobody can pick a user name that collides with another player's (and
/// would be served or overwrite their data). Each database has its own key,
/// stored with it.
#[derive(Clone)]
pub struct UserHasher {
    key: [u8; UserHasher::KEY_SIZE],
    mac: Hmac<Sha256>,
}

impl UserHasher {
    pub const KEY_SIZE: usize = 32;

    pub fn from_key(key: [u8; UserHasher::KEY_SIZE]) -> UserHasher {
        UserHasher {
            key,
            mac: Hmac::new_from_slice(&key).expect("any key size"),
        }
    }

    /// A hasher with a new random key.
    pub fn generate() -> UserHasher {
        let mut key = [0; UserHasher::KEY_SIZE];
        getrandom::fill(&mut key).expect("random key");
        UserHasher::from_key(key)
    }

    pub fn key(&self) -> &[u8; UserHasher::KEY_SIZE] {
        &self.key
    }

    pub fn from_key_bytes(bytes: &[u8]) -> Option<UserHasher> {
        bytes.try_into().ok().map(UserHasher::from_key)
    }

    /// Reads a key file (written by `save`).
    pub fn load(path: &Path) -> io::Result<UserHasher> {
        UserHasher::from_key_bytes(&fs::read(path)?).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid user hash key in {}", path.display()),
            )
        })
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        fs::write(path, self.key)
    }

    pub fn hash(&self, user: &UserId) -> UserHash {
        let mut mac = self.mac.clone();
        mac.update(user.as_str().as_bytes());
        let digest = mac.finalize().into_bytes();
        UserHash(digest[..UserHash::SIZE].try_into().expect("8 bytes"))
    }
}

impl fmt::Debug for UserHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UserHasher { .. }")
    }
}

impl PartialEq for UserHasher {
    fn eq(&self, other: &UserHasher) -> bool {
        self.key == other.key
    }
}

impl Eq for UserHasher {}

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
    }

    #[test]
    fn test_user_hash() {
        let hasher = UserHasher::from_key([7; UserHasher::KEY_SIZE]);
        let id = UserId::from_name("foo_bar-1").unwrap();
        assert_eq!(
            hasher.hash(&id),
            hasher.hash(&UserId::from_name("FOO_bar-1").unwrap())
        );
        assert_ne!(
            hasher.hash(&id),
            hasher.hash(&UserId::from_name("foo_bar-2").unwrap())
        );
        assert_ne!(hasher.hash(&id), UserHasher::generate().hash(&id));
    }

    #[test]
    fn test_user_hash_is_stable() {
        // Part of the database format: must never change. Python:
        // hmac.new(bytes([7] * 32), b"revoof", hashlib.sha256).
        let hasher = UserHasher::from_key([7; UserHasher::KEY_SIZE]);
        assert_eq!(
            hasher.hash(&UserId::from_name("revoof").unwrap()).to_u64(),
            0x2d4b_f6b0_e9f5_e360
        );
    }

    #[test]
    fn test_fingerprint_is_stable() {
        assert_eq!(
            UserId::from_name("revoof")
                .unwrap()
                .fingerprint()
                .to_be_bytes(),
            Sha256::digest(b"revoof")[..8]
        );
    }
}
