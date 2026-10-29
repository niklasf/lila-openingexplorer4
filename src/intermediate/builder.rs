use std::collections::BTreeMap;

use crate::{
    intermediate::{GlobalGameSeq, IntermediateEntry, IntermediateSubKey},
    packed::{GAMES_PER_CELL, GameId, LichessEntry, Month, SubStats},
};

/// Chronological order of games: by month, then in input order.
type GameKey = (Month, GlobalGameSeq);

/// Reusable buffers for packing entries.
#[derive(Default)]
pub struct EntryPacker {
    /// Months of groups, with the start and end index of their kept games.
    cells: Vec<(IntermediateSubKey, Month, SubStats, usize, usize)>,
    games: Vec<(GameKey, GameId)>,
}

impl EntryPacker {
    /// Packs the entry of the sorted intermediate entries of one key.
    pub fn pack_sorted(&mut self, records: &[IntermediateEntry], buf: &mut Vec<u8>) {
        self.cells.clear();
        self.games.clear();
        for run in records.chunk_by(|a, b| a.sub_key() == b.sub_key() && a.month() == b.month()) {
            let mut stats = SubStats::default();
            for record in run {
                stats.add(record.rating(), record.outcome());
            }
            let start = self.games.len();
            self.games.extend(
                run[run.len().saturating_sub(GAMES_PER_CELL)..]
                    .iter()
                    .map(|record| ((record.month(), record.seq()), record.game_id())),
            );
            self.cells.push((
                run[0].sub_key(),
                run[0].month(),
                stats,
                start,
                self.games.len(),
            ));
        }

        self.pack(buf);
    }

    fn pack(&self, buf: &mut Vec<u8>) {
        buf.clear();
        LichessEntry::pack_entire_external(
            buf,
            self.cells.iter().map(|(key, month, stats, start, end)| {
                (
                    key.uci_move(),
                    key.group_header(),
                    *month,
                    stats,
                    &self.games[*start..*end],
                )
            }),
        );
    }
}

#[derive(Default)]
struct CellBuilder {
    stats: SubStats,
    /// Kept games, truncated to the most recent `GAMES_PER_CELL` once in a
    /// while.
    games: Vec<(GameKey, GameId)>,
}

impl CellBuilder {
    fn add(&mut self, record: &IntermediateEntry) {
        self.stats.add(record.rating(), record.outcome());
        self.games
            .push(((record.month(), record.seq()), record.game_id()));
        if self.games.len() >= 4 * GAMES_PER_CELL {
            self.gc();
        }
    }

    fn gc(&mut self) {
        self.games.sort_unstable_by_key(|(key, _)| *key);
        self.games
            .drain(..self.games.len().saturating_sub(GAMES_PER_CELL));
    }
}

#[derive(Default)]
pub struct EntryBuilder {
    cells: BTreeMap<(IntermediateSubKey, Month), CellBuilder>,
}

impl EntryBuilder {
    pub fn add(&mut self, record: &IntermediateEntry) {
        self.cells
            .entry((record.sub_key(), record.month()))
            .or_default()
            .add(record);
    }

    pub fn pack(self, packer: &mut EntryPacker, buf: &mut Vec<u8>) {
        packer.cells.clear();
        packer.games.clear();
        for ((key, month), mut cell) in self.cells {
            cell.gc();
            let start = packer.games.len();
            packer.games.extend(cell.games);
            packer
                .cells
                .push((key, month, cell.stats, start, packer.games.len()));
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
            let mut month = Month::from_months_since_jan_1900(u.int_in_range(1356..=1700)?);
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
                // Input order is chronological by month.
                if u.ratio(1, 8)? {
                    month = Month::from_months_since_jan_1900(
                        month.months_since_jan_1900() + u.int_in_range(1..=30)?,
                    );
                }
                let m = PackedUciMove::pack(*u.choose(&moves)?);
                let speed = *u.choose(&[Speed::Blitz, Speed::Rapid])?;
                let rating = Rating::try_from(u.int_in_range(900..=1300)?).unwrap();
                let outcome: KnownOutcome = u.arbitrary()?;
                let game_id: GameId = u.arbitrary()?;
                let seq = GlobalGameSeq::new(seq).unwrap();
                records.push(IntermediateEntry::new(
                    hash, month, m, speed, rating, outcome, seq, game_id,
                ));
                lichess_entry.add_game(m, speed, rating, outcome, month, game_id);
            }
            let mut expected = Vec::new();
            lichess_entry.pack_entire(&mut expected);
            let unpack = |buf: &[u8]| {
                let mut entry = LichessEntry::new();
                entry.merge_unpack_entire(&mut &buf[..]);
                entry
            };

            // Builder, in any order.
            let mut shuffled = records.clone();
            for i in (1..shuffled.len()).rev() {
                shuffled.swap(i, u.int_in_range(0..=i)?);
            }
            let mut builder = EntryBuilder::default();
            for record in &shuffled {
                builder.add(record);
            }
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
