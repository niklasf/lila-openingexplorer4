//! Replaying games into position records.

use shakmaty::{
    CastlingMode, EnPassantMode, KnownOutcome, Outcome, Position,
    packed::{PackedSetup, PackedUciMove},
    variant::{Variant, VariantPosition},
    zobrist::Zobrist128,
};

use crate::{
    packed::{GameId, Month, PackedMoves, Rating, Speed, Zobrist96},
    parquet::GameRow,
    parquet_to_positions::record::Record,
};

/// Why a game produces no records.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Skip {
    MissingRating,
    RatingOutOfRange,
    MissingMonth,
    UnknownOutcome,
    MissingSpeed,
    InvalidSetup,
    InvalidMoves,
}

impl Skip {
    pub const ALL: [Skip; 7] = [
        Skip::MissingRating,
        Skip::RatingOutOfRange,
        Skip::MissingMonth,
        Skip::UnknownOutcome,
        Skip::MissingSpeed,
        Skip::InvalidSetup,
        Skip::InvalidMoves,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Skip::MissingRating => "missing rating",
            Skip::RatingOutOfRange => "rating out of range",
            Skip::MissingMonth => "missing month",
            Skip::UnknownOutcome => "unknown outcome",
            Skip::MissingSpeed => "missing speed",
            Skip::InvalidSetup => "invalid setup",
            Skip::InvalidMoves => "invalid moves",
        }
    }
}

/// Game level fields of the records of a game.
#[derive(Debug, Copy, Clone)]
pub struct GameFields {
    pub game_id: GameId,
    pub month: Month,
    pub speed: Speed,
    /// Average rating of both players.
    pub rating: Rating,
    pub outcome: KnownOutcome,
}

impl GameFields {
    pub fn from_row(row: &GameRow<'_>) -> Result<GameFields, Skip> {
        let (Some(white), Some(black)) = (row.players.white.rating, row.players.black.rating)
        else {
            return Err(Skip::MissingRating);
        };
        let rating = Rating::try_from((white + black) / 2).map_err(|_| Skip::RatingOutOfRange)?;
        let (Some(year), Some(month)) = (row.date_year, row.date_month) else {
            return Err(Skip::MissingMonth);
        };
        let month = Month::from_year_month(year, month).ok_or(Skip::MissingMonth)?;
        let outcome = match Outcome::from_ascii(row.outcome) {
            Ok(Outcome::Known(outcome)) => outcome,
            _ => return Err(Skip::UnknownOutcome),
        };
        let speed = row
            .speed
            .and_then(|speed| Speed::from_ascii(speed).ok())
            .ok_or(Skip::MissingSpeed)?;
        Ok(GameFields {
            game_id: row.game_id,
            month,
            speed,
            rating,
            outcome,
        })
    }
}

/// Positions of the current game, for detecting repetitions. Open
/// addressing on the hash prefix, cleared by bumping the generation.
struct Seen {
    slots: Vec<(u64, u32, u32)>, // (prefix, generation, index)
    generation: u32,
}

impl Seen {
    fn new(capacity: usize) -> Seen {
        Seen {
            slots: vec![(0, 0, 0); capacity.next_power_of_two()],
            generation: 1,
        }
    }

    fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.slots.fill((0, 0, 0));
            self.generation = 1;
        }
    }

    /// Looks up the index of the position, or inserts it with the given
    /// index.
    #[inline]
    fn get_or_insert(&mut self, prefix: u64, index: u32) -> Option<u32> {
        let mask = self.slots.len() - 1;
        let mut slot = prefix as usize & mask;
        loop {
            let (key, generation, i) = self.slots[slot];
            if generation != self.generation {
                self.slots[slot] = (prefix, self.generation, index);
                return None;
            }
            if key == prefix {
                return Some(i);
            }
            slot = (slot + 1) & mask;
        }
    }
}

/// Replays games into records, reusing buffers.
pub struct Replayer {
    records: Vec<Record>,
    seen: Seen,
    /// Plies whose hash was updated incrementally.
    pub incremental: u64,
    /// Repetitions removed.
    pub loops: u64,
}

impl Default for Replayer {
    fn default() -> Self {
        Self::new()
    }
}

impl Replayer {
    pub fn new() -> Replayer {
        Replayer {
            records: Vec::new(),
            seen: Seen::new(4096),
            incremental: 0,
            loops: 0,
        }
    }

    /// Replays the game and returns one record per position with a move
    /// played, in game order, with repetitions removed: when a position
    /// repeats, the records since its first occurrence are dropped, so that
    /// only the move played eventually remains.
    pub fn replay(&mut self, row: &GameRow<'_>) -> Result<&[Record], Skip> {
        let fields = GameFields::from_row(row)?;
        let (mut pos, variant) = match row.setup {
            None => (VariantPosition::new(Variant::Chess), Variant::Chess),
            Some(bytes) => {
                let (setup, variant) = PackedSetup::try_from_bytes(bytes)
                    .map_err(|_| Skip::InvalidSetup)?
                    .unpack_variant()
                    .map_err(|_| Skip::InvalidSetup)?;
                (
                    VariantPosition::from_setup(variant, setup, CastlingMode::Chess960)
                        .map_err(|_| Skip::InvalidSetup)?,
                    variant,
                )
            }
        };
        let packed = PackedMoves(row.moves.to_vec());
        let mut decoder = packed.decode().map_err(|_| Skip::InvalidMoves)?;

        self.records.clear();
        self.seen.clear();
        let mut hash: Zobrist128 = pos.zobrist_hash(EnPassantMode::Legal);
        while let Some(m) = decoder.decode_next(&pos).map_err(|_| Skip::InvalidMoves)? {
            let key = Zobrist96::new(hash, variant);
            let index = self.records.len() as u32;
            if let Some(first) = self.seen.get_or_insert(key.prefix(), index) {
                self.loops += 1;
                self.records.truncate(first as usize);
                self.seen.clear();
                for (i, r) in self.records.iter().enumerate() {
                    self.seen.get_or_insert(r.hash_prefix(), i as u32);
                }
                self.seen.get_or_insert(key.prefix(), first);
            }
            if self.records.len() >= self.seen.slots.len() / 2 {
                self.grow_seen();
            }
            let uci = PackedUciMove::pack(m.to_uci(CastlingMode::Chess960));
            self.records.push(
                Record::new(
                    key,
                    fields.month,
                    uci,
                    fields.speed,
                    fields.rating,
                    fields.outcome,
                    fields.game_id,
                )
                .ok_or(Skip::RatingOutOfRange)?,
            );
            hash = match pos.update_zobrist_hash(hash, m, EnPassantMode::Legal) {
                Some(hash) => {
                    self.incremental += 1;
                    pos.play_unchecked(m);
                    hash
                }
                None => {
                    pos.play_unchecked(m);
                    pos.zobrist_hash(EnPassantMode::Legal)
                }
            };
        }
        Ok(&self.records)
    }

    fn grow_seen(&mut self) {
        self.seen = Seen::new(self.seen.slots.len() * 2);
        for (i, r) in self.records.iter().enumerate() {
            self.seen.get_or_insert(r.hash_prefix(), i as u32);
        }
    }
}

#[cfg(test)]
mod tests {
    use shakmaty::{ByColor, Chess, san::San};

    use super::*;
    use crate::{
        indexing::{FlatGame, FlatPlayer},
        packed::{LaxDate, RatingMode},
        parquet::GameColumns,
    };

    fn game(sans: &[&str]) -> FlatGame {
        let mut pos = Chess::default();
        let mut moves = Vec::new();
        for san in sans {
            let m = san.parse::<San>().unwrap().to_move(&pos).unwrap();
            pos.play_unchecked(m);
            moves.push(m);
        }
        FlatGame {
            game_id: GameId::from_ascii(b"abcdefgh").unwrap(),
            players: ByColor {
                white: FlatPlayer {
                    title: None,
                    name: "w".to_owned(),
                    rating: Some(Rating::try_from(1600).unwrap()),
                },
                black: FlatPlayer {
                    title: None,
                    name: "b".to_owned(),
                    rating: Some(Rating::try_from(1500).unwrap()),
                },
            },
            date: Some(LaxDate::from_ymd(2024, Some(3), Some(1)).unwrap()),
            outcome: Outcome::Known(KnownOutcome::Draw),
            speed: Some(Speed::Blitz),
            rating_mode: RatingMode::Rated,
            setup: None,
            moves: PackedMoves::encode(
                &mut VariantPosition::new(Variant::Chess),
                moves.iter().copied(),
            ),
        }
    }

    fn replay(sans: &[&str]) -> Vec<Record> {
        let mut columns = GameColumns::default();
        columns.push(game(sans));
        let row = columns.rows().next().unwrap();
        let mut replayer = Replayer::new();
        replayer.replay(&row).unwrap().to_vec()
    }

    #[test]
    fn test_replay_fields() {
        let records = replay(&["e4", "e5", "Nf3"]);
        assert_eq!(records.len(), 3);
        let start: Zobrist128 = Chess::default().zobrist_hash(EnPassantMode::Legal);
        assert_eq!(records[0].hash(), Zobrist96::new(start, Variant::Chess));
        assert_eq!(records[0].uci_move().unpack().to_string(), "e2e4");
        assert_eq!(records[2].uci_move().unpack().to_string(), "g1f3");
        assert_eq!(records[0].rating(), Rating::try_from(1550).unwrap());
        assert_eq!(records[0].month().to_string(), "2024-03");
        assert_eq!(records[0].speed(), Speed::Blitz);
        assert_eq!(records[0].outcome(), KnownOutcome::Draw);
        assert!(records.iter().all(|r| r.game_id() == records[0].game_id()));
    }

    #[test]
    fn test_loop_removal() {
        // Nf3 Nf6 Ng1 Ng8 returns to the start, then e4.
        let looped = replay(&["Nf3", "Nf6", "Ng1", "Ng8", "e4", "e5"]);
        let direct = replay(&["e4", "e5"]);
        assert_eq!(looped, direct);

        // Repeating three times keeps the last cycle only.
        let triple = replay(&["Nf3", "Nf6", "Ng1", "Ng8", "Nf3", "Nf6", "Ng1", "Ng8"]);
        assert_eq!(triple.len(), 4);
        assert_eq!(
            triple
                .iter()
                .map(|r| r.uci_move().unpack().to_string())
                .collect::<Vec<_>>(),
            ["g1f3", "g8f6", "f3g1", "f6g8"]
        );
    }
}
