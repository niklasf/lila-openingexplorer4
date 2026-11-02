use shakmaty::{
    CastlingMode, Color, EnPassantMode, Position,
    packed::PackedUciMove,
    variant::{VariantPosition, WithPositionMut},
    zobrist::Zobrist128,
};

use crate::packed::{BorrowedLichessGame, PackedMovesDecoder, Zobrist96};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Visit {
    pub m: PackedUciMove,
    pub turn: Color,
}

/// Replays the first `max_ply` plies of a game (all if `None`) and finds
/// the position with the given hash, as `parquet_to_positions` would have
/// recorded it. The final position is never a visit.
pub fn find_visit(
    game: &BorrowedLichessGame<'_>,
    target: &Zobrist96,
    max_ply: Option<usize>,
) -> Option<Visit> {
    let mut pos = game.initial_position();
    let variant = pos.variant();

    struct Find<'a> {
        decoder: PackedMovesDecoder<'a>,
        max_ply: usize,
        target: Zobrist96,
        variant: shakmaty::variant::Variant,
    }

    impl WithPositionMut for Find<'_> {
        type Output = Option<Visit>;

        fn call<P: Position>(mut self, pos: &mut P) -> Option<Visit> {
            let mut hash: Zobrist128 = pos.zobrist_hash(EnPassantMode::Legal);
            let mut found = None;
            let mut ply = 0;
            while ply < self.max_ply
                && let Ok(Some(m)) = self.decoder.decode_next(pos)
            {
                if Zobrist96::new(hash, self.variant) == self.target {
                    found = Some(Visit {
                        m: PackedUciMove::pack(m.to_uci(CastlingMode::Chess960)),
                        turn: pos.turn(),
                    });
                }
                ply += 1;
                let updated = pos.update_zobrist_hash(hash, m, EnPassantMode::Legal);
                pos.play_unchecked(m);
                hash = updated.unwrap_or_else(|| pos.zobrist_hash(EnPassantMode::Legal));
            }
            found
        }
    }

    let decoder = PackedMovesDecoder::new(game.moves).ok()?;
    pos.with_position_mut(Find {
        decoder,
        max_ply: max_ply.unwrap_or(usize::MAX),
        target: *target,
        variant,
    })
}

/// Hash of a position as used for the positions and cold_positions keys.
pub fn position_hash(pos: &VariantPosition) -> Zobrist96 {
    Zobrist96::new(pos.zobrist_hash(EnPassantMode::Legal), pos.variant())
}

#[cfg(test)]
mod tests {
    use shakmaty::{ByColor, Chess, KnownOutcome, san::San, uci::UciMove};

    use super::*;
    use crate::packed::{BorrowedGamePlayer, LaxDate, PackedMoves, RatingMode};

    fn game(sans: &str) -> PackedMoves {
        let mut pos = VariantPosition::Chess(Chess::default());
        let mut encoder = PackedMoves::encoder();
        for san in sans.split_whitespace() {
            let m = san.parse::<San>().unwrap().to_move(&pos).unwrap();
            encoder.encode_next(&pos, m);
            pos.play_unchecked(m);
        }
        encoder.finish()
    }

    fn after(sans: &str) -> VariantPosition {
        let mut pos = VariantPosition::Chess(Chess::default());
        for san in sans.split_whitespace() {
            let m = san.parse::<San>().unwrap().to_move(&pos).unwrap();
            pos.play_unchecked(m);
        }
        pos
    }

    #[test]
    fn test_find_visit() {
        let moves = game("Nf3 Nf6 Ng1 Ng8 e4 e5");
        let game = BorrowedLichessGame {
            players: ByColor {
                white: BorrowedGamePlayer {
                    title: None,
                    name: "w",
                    rating: None,
                },
                black: BorrowedGamePlayer {
                    title: None,
                    name: "b",
                    rating: None,
                },
            },
            date: LaxDate::from_ymd(2020, None, None).unwrap(),
            outcome: KnownOutcome::Draw,
            speed: None,
            rating_mode: RatingMode::Rated,
            setup: None,
            event: None,
            moves: &moves.0,
        };
        let visit = |pos: &VariantPosition, max_ply| {
            find_visit(&game, &position_hash(pos), max_ply)
                .map(|visit| (visit.m.unpack(), visit.turn))
        };
        let uci = |s: &str| s.parse::<UciMove>().unwrap();
        // The start position keeps the move eventually played.
        assert_eq!(visit(&after(""), None), Some((uci("e2e4"), Color::White)));
        assert_eq!(
            visit(&after(""), Some(2)),
            Some((uci("g1f3"), Color::White))
        );
        assert_eq!(
            visit(&after("Nf3"), None),
            Some((uci("g8f6"), Color::Black))
        );
        assert_eq!(visit(&after("e4"), None), Some((uci("e7e5"), Color::Black)));
        // The final position is not a visit.
        assert_eq!(visit(&after("e4 e5"), None), None);
        assert_eq!(visit(&after("e4"), Some(5)), None);
        assert_eq!(visit(&after("d4"), None), None);
    }
}
