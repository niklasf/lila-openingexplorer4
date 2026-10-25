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
