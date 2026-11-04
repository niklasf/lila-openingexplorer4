//! Replay of player games and aggregation of their visits into warm entries
//! and cold groups.

use std::collections::HashMap;

use nohash_hasher::IntMap;
use shakmaty::{
    ByColor, CastlingMode, Color, EnPassantMode, KnownOutcome, Position,
    packed::PackedUciMove,
    variant::{Variant, WithPositionMut},
    zobrist::Zobrist128,
};

use crate::{
    packed::{
        CellKey, GameIndex, Month, PackedMovesDecoder, PlayerEntry, PlayerGroupHeader, PositionKey,
        Rating,
    },
    player::{
        cold::PlayerColdGroup,
        keys::{ColdBucket, GameSeq, MAX_COLD_BITS, color_byte},
        record::BorrowedPlayerGame,
    },
};

/// The positions of the first `max_ply` plies of a game, each once, with
/// the move played from it the last time (removing loops). The final
/// position is not a visit.
pub fn replay(
    game: &BorrowedPlayerGame<'_>,
    max_ply: usize,
    scratch: &mut IntMap<Zobrist128, PackedUciMove>,
    out: &mut Vec<(PositionKey, PackedUciMove)>,
) {
    struct Replay<'a> {
        decoder: PackedMovesDecoder<'a>,
        max_ply: usize,
        visits: &'a mut IntMap<Zobrist128, PackedUciMove>,
    }

    impl WithPositionMut for Replay<'_> {
        type Output = ();

        fn call<P: Position>(mut self, pos: &mut P) {
            let mut hash: Zobrist128 = pos.zobrist_hash(EnPassantMode::Legal);
            let mut ply = 0;
            while ply < self.max_ply
                && let Ok(Some(m)) = self.decoder.decode_next(pos)
            {
                self.visits
                    .insert(hash, PackedUciMove::pack(m.to_uci(CastlingMode::Chess960)));
                ply += 1;
                let updated = pos.update_zobrist_hash(hash, m, EnPassantMode::Legal);
                pos.play_unchecked(m);
                hash = updated.unwrap_or_else(|| pos.zobrist_hash(EnPassantMode::Legal));
            }
        }
    }

    let Ok(decoder) = PackedMovesDecoder::new(game.moves) else {
        return;
    };
    let mut pos = game.initial_position();
    let variant: Variant = pos.variant();
    scratch.clear();
    pos.with_position_mut(Replay {
        decoder,
        max_ply,
        visits: scratch,
    });
    out.extend(
        scratch
            .drain()
            .map(|(hash, m)| (PositionKey::from_zobrist(hash, variant), m)),
    );
}

/// The move played from the position in the game, if the game reached it
/// (see `replay`).
pub fn find_visit(
    game: &BorrowedPlayerGame<'_>,
    target: &PositionKey,
    max_ply: usize,
) -> Option<PackedUciMove> {
    let mut scratch = IntMap::default();
    let mut visits = Vec::new();
    replay(game, max_ply, &mut scratch, &mut visits);
    visits
        .into_iter()
        .find(|(key, _)| key == target)
        .map(|(_, m)| m)
}

/// Decides which positions go into warm entries and which into cold groups.
/// Any decision gives correct results (reads take the union), so this only
/// trades space against validation work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Positions with at least this many games are warm.
    Threshold(usize),
    /// Random decisions, for tests.
    Random(u64),
}

impl Default for Placement {
    fn default() -> Placement {
        Placement::Threshold(2)
    }
}

impl Placement {
    fn is_warm(&self, key: &PositionKey, games: usize) -> bool {
        match *self {
            Placement::Threshold(threshold) => games >= threshold,
            Placement::Random(seed) => {
                let mut x = key.prefix() ^ seed ^ (games as u64).wrapping_mul(0x9e37_79b9);
                x = (x ^ (x >> 31)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                (x ^ (x >> 29)) & 1 == 0
            }
        }
    }
}

/// Bucket bits for about 64 cold items per group.
pub fn cold_bits_for(items: u64) -> u8 {
    let mut bits = 0;
    while bits < MAX_COLD_BITS && items > 64 << bits {
        bits += 1;
    }
    bits
}

#[derive(Debug, Clone, Copy)]
struct Visit {
    color: Color,
    key: PositionKey,
    seq: GameSeq,
    m: PackedUciMove,
    header: PlayerGroupHeader,
    month: Month,
    outcome: KnownOutcome,
    opponent_rating: Rating,
}

/// Visits of games, in memory.
#[derive(Default)]
pub struct Aggregation {
    visits: Vec<Visit>,
    scratch: IntMap<Zobrist128, PackedUciMove>,
    replayed: Vec<(PositionKey, PackedUciMove)>,
}

/// Items `(fingerprint, seq)` of the touched cold buckets of a color.
pub type ColdItems = Vec<(Color, ColdBucket, Vec<(u16, u64)>)>;

/// Warm entries and cold groups of an aggregation, in key order.
pub struct Placed {
    pub warm: Vec<(Color, PositionKey, PlayerEntry)>,
    /// Items `(fingerprint, seq)` of each touched bucket.
    pub cold: ColdItems,
    pub cold_bits: ByColor<u8>,
    pub warm_visits: u64,
    pub cold_visits: u64,
}

impl Aggregation {
    pub fn new() -> Aggregation {
        Aggregation::default()
    }

    pub fn visits(&self) -> usize {
        self.visits.len()
    }

    pub fn add_game(&mut self, seq: GameSeq, game: &BorrowedPlayerGame<'_>, max_ply: usize) {
        self.replayed.clear();
        replay(game, max_ply, &mut self.scratch, &mut self.replayed);
        let header = PlayerGroupHeader {
            speed: game.speed,
            mode: game.mode,
        };
        self.visits
            .extend(self.replayed.iter().map(|&(key, m)| Visit {
                color: game.color,
                key,
                seq,
                m,
                header,
                month: game.month,
                outcome: game.outcome,
                opponent_rating: game.opponent_rating,
            }));
    }

    /// Places the visits. `cold_bits` gives the bucket bits of a color from
    /// the number of its cold items.
    ///
    /// Positions are warm or cold as the placement decides, with one
    /// exception that keeps warm and cold disjoint for every read: all
    /// visits of a game with the same cold fingerprint (color, bucket and
    /// fingerprint bits) are either cold or warm. Otherwise a read of a
    /// warm position would also find the game through the cold item of
    /// another of its positions with the same fingerprint, and validating
    /// the game by replay would count it a second time.
    pub fn place(mut self, placement: Placement, cold_bits: impl Fn(Color, u64) -> u8) -> Placed {
        // In key order (color byte, then position key).
        self.visits
            .sort_unstable_by_key(|v| (color_byte(v.color), v.key, v.seq));
        let mut is_warm = vec![false; self.visits.len()];
        let mut start = 0;
        for group in self
            .visits
            .chunk_by(|a, b| a.color == b.color && a.key == b.key)
        {
            if placement.is_warm(&group[0].key, group.len()) {
                is_warm[start..start + group.len()].fill(true);
            }
            start += group.len();
        }

        let mut counts = ByColor::new_with(|_| 0u64);
        for (v, &warm) in self.visits.iter().zip(&is_warm) {
            if !warm {
                *counts.get_mut(v.color) += 1;
            }
        }
        let bits =
            ByColor::new_with(|color| cold_bits(color, *counts.get(color)).min(MAX_COLD_BITS));

        // Visits of a game sharing a fingerprint go where any of them is
        // warm.
        let mut fingerprints: Vec<(GameSeq, u8, ColdBucket, u16, usize)> = self
            .visits
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let (bucket, fingerprint) = ColdBucket::of(&v.key, *bits.get(v.color));
                (v.seq, color_byte(v.color), bucket, fingerprint, i)
            })
            .collect();
        fingerprints.sort_unstable();
        for group in
            fingerprints.chunk_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2 && a.3 == b.3)
        {
            if group.len() > 1 && group.iter().any(|f| is_warm[f.4]) {
                for f in group {
                    is_warm[f.4] = true;
                }
            }
        }

        let mut warm = Vec::new();
        let mut warm_visits = 0;
        let mut by_bucket: HashMap<(Color, ColdBucket), Vec<(u16, u64)>> = HashMap::new();
        let mut cold_visits = 0;
        let mut start = 0;
        for group in self
            .visits
            .chunk_by(|a, b| a.color == b.color && a.key == b.key)
        {
            let first = group[0];
            let flags = &is_warm[start..start + group.len()];
            start += group.len();
            let mut entry = PlayerEntry::new();
            for (v, &warm) in group.iter().zip(flags) {
                if warm {
                    entry.add_game(
                        CellKey::new(v.m, v.header, v.month),
                        v.opponent_rating,
                        v.outcome,
                        GameIndex::new(u64::from(v.seq.0)).expect("game index"),
                    );
                    warm_visits += 1;
                } else {
                    let (bucket, fingerprint) = ColdBucket::of(&v.key, *bits.get(v.color));
                    by_bucket
                        .entry((v.color, bucket))
                        .or_default()
                        .push((fingerprint, u64::from(v.seq.0)));
                    cold_visits += 1;
                }
            }
            if !entry.is_empty() {
                warm.push((first.color, first.key, entry));
            }
        }
        let mut cold: ColdItems = by_bucket
            .into_iter()
            .map(|((color, bucket), items)| (color, bucket, items))
            .collect();
        cold.sort_unstable_by_key(|(color, bucket, _)| (color_byte(*color), *bucket));

        Placed {
            warm,
            cold,
            cold_bits: bits,
            warm_visits,
            cold_visits,
        }
    }
}

impl Placed {
    /// Packs a cold group's items.
    pub fn pack_cold(items: &mut Vec<(u16, u64)>) -> Vec<u8> {
        let mut buf = Vec::new();
        PlayerColdGroup::pack(items, &mut buf);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cold_bits_for() {
        assert_eq!(cold_bits_for(0), 0);
        assert_eq!(cold_bits_for(64), 0);
        assert_eq!(cold_bits_for(65), 1);
        assert_eq!(cold_bits_for(64 * 1024), 10);
        assert_eq!(cold_bits_for(20_000_000), 19);
        assert_eq!(cold_bits_for(u64::MAX), MAX_COLD_BITS);
    }
}
