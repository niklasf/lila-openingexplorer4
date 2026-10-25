use rustc_hash::FxHashMap;

use crate::{
    intermediate::{GlobalGameSeq, IntermediateEntry, IntermediateSubKey},
    packed::{GameId, LichessEntry, SubStats},
};

pub const GAMES_PER_SUB_ENTRY: usize = 8;

/// Reusable buffers for packing entries.
#[derive(Default)]
pub struct EntryPacker {
    /// Sub-entries with the range of their kept games.
    subs: Vec<(IntermediateSubKey, SubStats, usize, usize)>,
    games: Vec<(GlobalGameSeq, GameId)>,
}

impl EntryPacker {
    /// Packs the entry of the sorted intermediate entries of one key.
    pub fn pack_sorted(&mut self, records: &[IntermediateEntry], buf: &mut Vec<u8>) {
        self.subs.clear();
        self.games.clear();
        for run in records.chunk_by(|a, b| a.sub_key() == b.sub_key()) {
            let mut stats = SubStats::default();
            for record in run {
                stats.add(record.rating(), record.outcome());
            }
            let start = self.games.len();
            self.games.extend(
                run[run.len().saturating_sub(GAMES_PER_SUB_ENTRY)..]
                    .iter()
                    .map(|record| (record.seq(), record.game_id())),
            );
            self.subs
                .push((run[0].sub_key(), stats, start, self.games.len()));
        }

        self.pack(buf);
    }

    fn pack(&self, buf: &mut Vec<u8>) {
        buf.clear();
        let games = &self.games;
        LichessEntry::pack_entire_external(
            buf,
            self.subs.iter().map(|(key, stats, start, end)| {
                (
                    key.uci_move(),
                    key.group_header(),
                    stats,
                    &games[*start..*end],
                )
            }),
        );
    }
}

#[derive(Default, Clone)]
struct SubBuilder {
    stats: SubStats,
    /// Kept games, truncated to the last `GAMES_PER_SUB_ENTRY` by sequence
    /// number once in a while.
    games: Vec<(GlobalGameSeq, GameId)>,
}

impl SubBuilder {
    fn add(&mut self, record: &IntermediateEntry) {
        self.stats.add(record.rating(), record.outcome());
        self.games.push((record.seq(), record.game_id()));
        if self.games.len() >= 4 * GAMES_PER_SUB_ENTRY {
            self.gc();
        }
    }

    fn gc(&mut self) {
        self.games.sort_unstable_by_key(|(seq, _)| *seq);
        self.games
            .drain(..self.games.len().saturating_sub(GAMES_PER_SUB_ENTRY));
    }

    fn merge(&mut self, other: SubBuilder) {
        self.stats.merge(&other.stats);
        self.games.extend(other.games);
        self.gc();
    }
}

#[derive(Default)]
pub struct EntryBuilder {
    subs: FxHashMap<IntermediateSubKey, SubBuilder>,
}

impl EntryBuilder {
    pub fn add(&mut self, record: &IntermediateEntry) {
        self.subs.entry(record.sub_key()).or_default().add(record);
    }

    pub fn merge(&mut self, other: EntryBuilder) {
        for (key, sub) in other.subs {
            self.subs.entry(key).or_default().merge(sub);
        }
    }

    pub fn games(&self) -> u64 {
        self.subs.values().map(|sub| sub.stats.games()).sum()
    }

    pub fn pack(self, packer: &mut EntryPacker, buf: &mut Vec<u8>) {
        let mut subs: Vec<(IntermediateSubKey, SubBuilder)> = self.subs.into_iter().collect();
        subs.sort_unstable_by_key(|(key, _)| *key);
        packer.subs.clear();
        packer.games.clear();
        for (key, mut sub) in subs {
            sub.gc();
            let start = packer.games.len();
            packer.games.extend(sub.games);
            packer
                .subs
                .push((key, sub.stats, start, packer.games.len()));
        }
        packer.pack(buf);
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;
    use shakmaty::{KnownOutcome, Square, packed::PackedUciMove, uci::UciMove};

    use super::*;
    use crate::packed::{Month, Rating, Speed, Zobrist96};

    #[test]
    fn test_packers_agree() {
        arbtest(|u| {
            let hash: Zobrist96 = u.arbitrary()?;
            let month: Month = u.arbitrary()?;
            let moves = [
                UciMove::Normal {
                    from: Square::E2,
                    to: Square::E4,
                    promotion: None,
                },
                UciMove::Normal {
                    from: Square::D2,
                    to: Square::D4,
                    promotion: None,
                },
            ];

            let mut records = Vec::new();
            let mut lichess_entry = LichessEntry::new();
            let mut seq = 0;
            for _ in 0..u.int_in_range(1..=200)? {
                seq += u.int_in_range(1..=3)?;
                let m = PackedUciMove::pack(*u.choose(&moves)?);
                let speed = *u.choose(&[Speed::Blitz, Speed::Rapid])?;
                let rating = Rating::try_from(u.int_in_range(900..=1300)?).unwrap();
                let outcome: KnownOutcome = u.arbitrary()?;
                let game_id: GameId = u.arbitrary()?;
                let seq = GlobalGameSeq::new(seq).unwrap();
                records.push(IntermediateEntry::new(
                    hash, month, m, speed, rating, outcome, seq, game_id,
                ));
                lichess_entry.add_game(m, speed, rating, outcome, game_id);
            }
            lichess_entry.truncate_games(GAMES_PER_SUB_ENTRY);
            let mut expected = Vec::new();
            lichess_entry.pack_entire(&mut expected);
            let unpack = |buf: &[u8]| {
                let mut entry = LichessEntry::new();
                entry.merge_unpack_entire(&mut &buf[..]);
                entry
            };

            // Builder, in any order, split into merged parts.
            let mut shuffled = records.clone();
            for i in (1..shuffled.len()).rev() {
                shuffled.swap(i, u.int_in_range(0..=i)?);
            }
            let split = u.int_in_range(0..=shuffled.len())?;
            let mut builder = EntryBuilder::default();
            let mut other = EntryBuilder::default();
            for record in &shuffled[..split] {
                builder.add(record);
            }
            for record in &shuffled[split..] {
                other.add(record);
            }
            builder.merge(other);
            assert_eq!(builder.games(), records.len() as u64);
            let mut packer = EntryPacker::default();
            let mut buf = Vec::new();
            builder.pack(&mut packer, &mut buf);
            assert_eq!(unpack(&buf), unpack(&expected));

            // Packer, from sorted records.
            shuffled.sort_unstable_by_key(IntermediateEntry::sort_key);
            packer.pack_sorted(&shuffled, &mut buf);
            assert_eq!(unpack(&buf), unpack(&expected));
            Ok(())
        });
    }
}
