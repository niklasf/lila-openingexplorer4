//! Status of a player: indexing progress and the current generation of the
//! warm and cold index.

use bytes::{Buf, BufMut};
use shakmaty::ByColor;

use crate::{
    packed::{pack_string, pack_uint, unpack_string, unpack_uint},
    player::{
        UserId,
        keys::{GameSeq, Generation, MAX_COLD_BITS},
    },
};

/// Version of the formats of the personal explorer. A player indexed with
/// another version is rebuilt.
pub const FORMAT_VERSION: u8 = 1;

/// ```text
/// status := u8(version) uint(generation) u8(cold bits white) u8(cold bits black)
///           uint(games) uint(games at last rebuild) uint(max ply)
///           uint(latest createdAt) uint(revisit createdAt + 1, or 0)
///           uint(indexed at) uint(revisited at) string(user id) string(name)
/// ```
///
/// Times in milliseconds since the epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerStatus {
    pub version: u8,
    pub user: UserId,
    /// The name as last seen in a game (with capitalization).
    pub name: String,
    /// Current generation of the warm and cold index.
    pub generation: Generation,
    /// Bits of the cold buckets of the current generation, per color of
    /// the player.
    pub cold_bits: ByColor<u8>,
    /// Number of indexed games, which is also the next `GameSeq`.
    pub games: u32,
    /// Games at the last rebuild.
    pub games_at_rebuild: u32,
    /// Plies indexed per game in the current generation.
    pub max_ply: u32,
    /// Latest `createdAt` of games received from lila (indexed or not).
    pub latest_created_at: u64,
    /// `createdAt` of the earliest game that was ongoing when last seen.
    pub revisit_ongoing: Option<u64>,
    /// When the last index run finished.
    pub indexed_at: u64,
    /// When ongoing games were last revisited.
    pub revisited_at: u64,
}

impl PlayerStatus {
    pub fn new(user: UserId, max_ply: u32) -> PlayerStatus {
        PlayerStatus {
            version: FORMAT_VERSION,
            name: user.as_str().to_owned(),
            user,
            generation: Generation(0),
            cold_bits: ByColor::new_with(|_| 0),
            games: 0,
            games_at_rebuild: 0,
            max_ply,
            latest_created_at: 0,
            revisit_ongoing: None,
            indexed_at: 0,
            revisited_at: 0,
        }
    }

    pub fn next_seq(&self) -> GameSeq {
        GameSeq(self.games)
    }

    pub fn pack(&self, buf: &mut Vec<u8>) {
        buf.put_u8(self.version);
        pack_uint(buf, u64::from(self.generation.0));
        buf.put_u8(self.cold_bits.white);
        buf.put_u8(self.cold_bits.black);
        pack_uint(buf, u64::from(self.games));
        pack_uint(buf, u64::from(self.games_at_rebuild));
        pack_uint(buf, u64::from(self.max_ply));
        pack_uint(buf, self.latest_created_at);
        pack_uint(buf, self.revisit_ongoing.map_or(0, |t| t + 1));
        pack_uint(buf, self.indexed_at);
        pack_uint(buf, self.revisited_at);
        pack_string(buf, self.user.as_str());
        pack_string(buf, &self.name);
    }

    pub fn unpack(mut buf: &[u8]) -> PlayerStatus {
        let buf = &mut buf;
        let version = buf.get_u8();
        let generation = Generation(unpack_uint(buf) as u32);
        let cold_bits = ByColor {
            white: buf.get_u8().min(MAX_COLD_BITS),
            black: buf.get_u8().min(MAX_COLD_BITS),
        };
        let games = unpack_uint(buf) as u32;
        let games_at_rebuild = unpack_uint(buf) as u32;
        let max_ply = unpack_uint(buf) as u32;
        let latest_created_at = unpack_uint(buf);
        let revisit_ongoing = unpack_uint(buf).checked_sub(1);
        let indexed_at = unpack_uint(buf);
        let revisited_at = unpack_uint(buf);
        let user = UserId::from_name(unpack_string(buf)).expect("valid user id");
        let name = unpack_string(buf).to_owned();
        PlayerStatus {
            version,
            user,
            name,
            generation,
            cold_bits,
            games,
            games_at_rebuild,
            max_ply,
            latest_created_at,
            revisit_ongoing,
            indexed_at,
            revisited_at,
        }
    }
}

/// When to index a player.
#[derive(Debug, Clone, Copy)]
pub struct RunPolicy {
    /// No new index run within this time after the last one (ms).
    pub cooldown: u64,
    /// Ongoing games are revisited at most this often (ms).
    pub revisit_interval: u64,
}

impl Default for RunPolicy {
    fn default() -> RunPolicy {
        RunPolicy {
            cooldown: 2 * 60 * 1000,
            revisit_interval: 24 * 60 * 60 * 1000,
        }
    }
}

/// An index run: request games created since a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRun {
    /// Request games with `createdAt >= since`.
    pub since: u64,
    /// Revisits ongoing games (and also gets all newer games).
    pub revisit: bool,
}

impl RunPolicy {
    /// The run to do now, if any.
    pub fn next_run(&self, status: Option<&PlayerStatus>, now: u64) -> Option<IndexRun> {
        let Some(status) = status else {
            return Some(IndexRun {
                since: 0,
                revisit: false,
            });
        };
        if now.saturating_sub(status.indexed_at) < self.cooldown {
            return None;
        }
        if let Some(ongoing) = status.revisit_ongoing
            && now.saturating_sub(status.revisited_at) >= self.revisit_interval
        {
            return Some(IndexRun {
                since: ongoing,
                revisit: true,
            });
        }
        Some(IndexRun {
            // Plus 1 ms to avoid the overlap. Might miss a game created in
            // the same millisecond after the last run, as before.
            since: if status.games == 0 && status.latest_created_at == 0 {
                0
            } else {
                status.latest_created_at + 1
            },
            revisit: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let mut status = PlayerStatus::new(UserId::from_name("alice").unwrap(), 50);
        status.name = "Alice".to_owned();
        status.generation = Generation(3);
        status.cold_bits.black = 7;
        status.games = 1234;
        status.revisit_ongoing = Some(0);
        status.indexed_at = 1_700_000_000_000;
        let mut buf = Vec::new();
        status.pack(&mut buf);
        assert_eq!(PlayerStatus::unpack(&buf), status);
    }

    #[test]
    fn test_next_run() {
        let policy = RunPolicy::default();
        let alice = UserId::from_name("alice").unwrap();
        assert_eq!(
            policy.next_run(None, 0),
            Some(IndexRun {
                since: 0,
                revisit: false
            })
        );
        let mut status = PlayerStatus::new(alice, 50);
        status.games = 10;
        status.latest_created_at = 1000;
        let t = 100_000_000_000;
        status.indexed_at = t;
        assert_eq!(policy.next_run(Some(&status), t + 60_000), None);
        assert_eq!(
            policy.next_run(Some(&status), t + 120_000),
            Some(IndexRun {
                since: 1001,
                revisit: false
            })
        );
        status.revisit_ongoing = Some(500);
        assert_eq!(
            policy.next_run(Some(&status), t + 120_000),
            Some(IndexRun {
                since: 500,
                revisit: true
            })
        );
        status.revisited_at = t;
        assert_eq!(
            policy.next_run(Some(&status), t + 120_000),
            Some(IndexRun {
                since: 1001,
                revisit: false
            })
        );
    }
}
