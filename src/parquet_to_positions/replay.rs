use shakmaty::{
    ByColor, CastlingMode, Color, EnPassantMode, KnownOutcome, Move, Position,
    packed::PackedUciMove,
    variant::{Variant, VariantPosition},
    zobrist::Zobrist128,
};
use thiserror::Error;

use crate::{
    intermediate::{GlobalGameSeq, IntermediateEntry},
    packed::{GameId, Month, PackedMovesDecoder, Rating, Speed, Zobrist96},
    parquet::{GameRow, InvalidGameRow},
};

#[derive(Debug, Error, Copy, Clone, PartialEq, Eq)]
pub enum InvalidGame {
    #[error("{0}")]
    Row(#[from] InvalidGameRow),
    #[error("invalid moves")]
    InvalidMoves,
}

/// A position reached in a game, and the move that was eventually played
/// from it.
#[derive(Copy, Clone)]
struct Visit {
    hash: Zobrist128,
    m: Move,
    turn: Color,
}

/// The metadata shared by all records of one game.
#[derive(Copy, Clone)]
pub struct GameInfo {
    pub game_id: GameId,
    pub seq: GlobalGameSeq,
    pub month: Month,
    pub speed: Speed,
    pub ratings: ByColor<Rating>,
    pub outcome: KnownOutcome,
}

impl GameInfo {
    pub fn from_row(row: &GameRow<'_>, seq: GlobalGameSeq) -> Result<GameInfo, InvalidGameRow> {
        Ok(GameInfo {
            game_id: row.game_id,
            seq,
            month: row.month()?,
            speed: row.speed()?,
            ratings: row.ratings()?,
            outcome: row.known_outcome()?,
        })
    }
}

/// Replays games, with buffers reused across games.
#[derive(Default)]
pub struct Replayer {
    visits: Vec<Visit>,
    /// Low bits of the hashes in `visits`, for a fast scan.
    short_hashes: Vec<u64>,
}

/// Statistics of a replayed game.
#[derive(Debug, Default, Copy, Clone)]
pub struct Replayed {
    pub plies: usize,
    /// Plies that reached a position that was already reached before in the
    /// same game.
    pub repetitions: usize,
}

impl Replayer {
    /// Replays the moves of a game from its initial position. Each position
    /// that a move was played from is recorded once, with the move that was
    /// played from it last: loops within the game are not counted multiple
    /// times. The final position is not recorded.
    pub fn replay(
        &mut self,
        mut pos: VariantPosition,
        moves: &[u8],
    ) -> Result<Replayed, InvalidGame> {
        self.visits.clear();
        self.short_hashes.clear();
        let mut replayed = Replayed::default();

        let mut decoder = PackedMovesDecoder::new(moves).map_err(|_| InvalidGame::InvalidMoves)?;
        let mut hash: Zobrist128 = pos.zobrist_hash(EnPassantMode::Legal);
        while let Some(m) = decoder
            .decode_next(&pos)
            .map_err(|_| InvalidGame::InvalidMoves)?
        {
            replayed.plies += 1;
            let visit = Visit {
                hash,
                m,
                turn: pos.turn(),
            };
            let short = hash.0 as u64;
            match self
                .short_hashes
                .iter()
                .rposition(|&s| s == short)
                .filter(|&i| self.visits[i].hash == hash)
            {
                Some(i) => {
                    replayed.repetitions += 1;
                    self.visits[i] = visit;
                }
                None => {
                    self.visits.push(visit);
                    self.short_hashes.push(short);
                }
            }

            hash = match pos.update_zobrist_hash(hash, m, EnPassantMode::Legal) {
                Some(hash) => {
                    pos.play_unchecked(m);
                    hash
                }
                None => {
                    pos.play_unchecked(m);
                    pos.zobrist_hash(EnPassantMode::Legal)
                }
            };
        }

        Ok(replayed)
    }

    /// Number of positions recorded by the last replay.
    pub fn len(&self) -> usize {
        self.visits.len()
    }

    /// The records of the last replayed game.
    pub fn entries(
        &self,
        variant: Variant,
        info: &GameInfo,
    ) -> impl Iterator<Item = IntermediateEntry> + '_ {
        let info = *info;
        self.visits.iter().map(move |visit| {
            IntermediateEntry::new(
                Zobrist96::new(visit.hash, variant),
                info.month,
                PackedUciMove::pack(visit.m.to_uci(CastlingMode::Chess960)),
                info.speed,
                info.ratings[visit.turn],
                info.outcome,
                info.seq,
                info.game_id,
            )
        })
    }

    /// Recorded hashes of the last replayed game, in order of first
    /// occurrence.
    pub fn hashes(&self) -> impl Iterator<Item = Zobrist128> + '_ {
        self.visits.iter().map(|visit| visit.hash)
    }
}

#[cfg(test)]
mod tests {
    use shakmaty::{Chess, san::San, uci::UciMove};

    use super::*;
    use crate::packed::PackedMoves;

    fn replay_san(sans: &str) -> (Vec<(Zobrist128, String)>, Replayed) {
        let mut pos = VariantPosition::Chess(Chess::default());
        let initial = pos.clone();
        let mut encoder = PackedMoves::encoder();
        for san in sans.split_whitespace() {
            let m = san.parse::<San>().unwrap().to_move(&pos).unwrap();
            encoder.encode_next(&pos, m);
            pos.play_unchecked(m);
        }
        let moves = encoder.finish();

        let mut replayer = Replayer::default();
        let replayed = replayer.replay(initial, &moves.0).unwrap();
        let records = replayer
            .visits
            .iter()
            .map(|visit| {
                (
                    visit.hash,
                    visit.m.to_uci(CastlingMode::Chess960).to_string(),
                )
            })
            .collect();
        (records, replayed)
    }

    fn hash_after(sans: &str) -> Zobrist128 {
        let mut pos = Chess::default();
        for san in sans.split_whitespace() {
            let m = san.parse::<San>().unwrap().to_move(&pos).unwrap();
            pos.play_unchecked(m);
        }
        pos.zobrist_hash(EnPassantMode::Legal)
    }

    #[test]
    fn test_no_loops() {
        let (records, replayed) = replay_san("e4 e5 Nf3 Nc6");
        assert_eq!(replayed.plies, 4);
        assert_eq!(replayed.repetitions, 0);
        let moves: Vec<&str> = records.iter().map(|(_, m)| m.as_str()).collect();
        assert_eq!(moves, ["e2e4", "e7e5", "g1f3", "b8c6"]);
        assert_eq!(records[0].0, hash_after(""));
        assert_eq!(records[3].0, hash_after("e4 e5 Nf3"));
    }

    #[test]
    fn test_loop_keeps_last_move() {
        // P -Nf3-> Q -Nf6-> . -Ng1-> . -Ng8-> P -e4-> ...
        let (records, replayed) = replay_san("Nf3 Nf6 Ng1 Ng8 e4 e5");
        assert_eq!(replayed.plies, 6);
        assert_eq!(replayed.repetitions, 1);
        let moves: Vec<&str> = records.iter().map(|(_, m)| m.as_str()).collect();
        // The starting position keeps the move eventually played (e4), the
        // positions inside the loop are still recorded.
        assert_eq!(moves, ["e2e4", "g8f6", "f3g1", "f6g8", "e7e5"]);
        assert_eq!(records[0].0, hash_after(""));
        assert_eq!(records[4].0, hash_after("e4"));
    }

    #[test]
    fn test_repetition_at_end() {
        // Threefold repetition: the final position is never recorded, but
        // the positions of the cycle keep their last moves.
        let (records, replayed) = replay_san("Nf3 Nf6 Ng1 Ng8 Nf3 Nf6 Ng1 Ng8");
        assert_eq!(replayed.plies, 8);
        assert_eq!(replayed.repetitions, 4);
        let moves: Vec<&str> = records.iter().map(|(_, m)| m.as_str()).collect();
        assert_eq!(moves, ["g1f3", "g8f6", "f3g1", "f6g8"]);
    }

    #[test]
    fn test_nested_loops() {
        // The inner loop returns to the position after Nf3 Nf6, the outer
        // loop to the starting position.
        let (records, replayed) = replay_san("Nf3 Nf6 Nc3 Nc6 Nb1 Nb8 Ng1 Ng8 d4");
        assert_eq!(replayed.repetitions, 2);
        let moves: Vec<&str> = records.iter().map(|(_, m)| m.as_str()).collect();
        assert_eq!(
            moves,
            ["d2d4", "g8f6", "f3g1", "b8c6", "c3b1", "c6b8", "f6g8"]
        );
    }

    #[test]
    fn test_castling_uci() {
        let (records, _) = replay_san("e4 e5 Nf3 Nc6 Bc4 Bc5 O-O");
        assert_eq!(
            records.last().unwrap().1.parse::<UciMove>().unwrap(),
            "e1h1".parse::<UciMove>().unwrap()
        );
    }
}
