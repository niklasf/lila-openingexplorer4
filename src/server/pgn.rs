use std::fmt::Write;

use shakmaty::{
    Color, EnPassantMode, KnownOutcome, Position,
    fen::Fen,
    san::SanPlus,
    variant::{Variant, VariantPosition},
};

use crate::packed::{BorrowedLichessGame, GameId, LaxDate, PackedMovesDecoder};

fn pgn_date(date: LaxDate) -> String {
    format!(
        "{:04}.{}.{}",
        date.year(),
        date.month().map_or("??".to_owned(), |m| format!("{m:02}")),
        date.day().map_or("??".to_owned(), |d| format!("{d:02}")),
    )
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn result(outcome: KnownOutcome) -> &'static str {
    match outcome {
        KnownOutcome::Decisive {
            winner: Color::White,
        } => "1-0",
        KnownOutcome::Decisive {
            winner: Color::Black,
        } => "0-1",
        KnownOutcome::Draw => "1/2-1/2",
    }
}

/// Writes space separated tokens, wrapping lines at 80 columns.
#[derive(Default)]
struct Tokens {
    line_len: usize,
    first: bool,
}

impl Tokens {
    fn push(&mut self, pgn: &mut String, s: &str) {
        if self.first {
            if self.line_len + 1 + s.len() > 80 {
                pgn.push('\n');
                self.line_len = 0;
            } else {
                pgn.push(' ');
                self.line_len += 1;
            }
        }
        self.first = true;
        pgn.push_str(s);
        self.line_len += s.len();
    }
}

/// Renders a masters game as PGN, like the previous explorer.
pub fn masters_pgn(id: GameId, game: &BorrowedLichessGame<'_>) -> String {
    let mut pgn = String::new();
    let event = game.event.as_ref();
    let _ = writeln!(
        pgn,
        "[Event \"{}\"]",
        escape(event.map_or("?", |event| event.event))
    );
    let _ = writeln!(
        pgn,
        "[Site \"{}\"]",
        escape(event.map_or("?", |event| event.site))
    );
    let _ = writeln!(pgn, "[Date \"{}\"]", pgn_date(game.date));
    let _ = writeln!(
        pgn,
        "[Round \"{}\"]",
        escape(event.map_or("?", |event| event.round))
    );
    let _ = writeln!(pgn, "[White \"{}\"]", escape(game.players.white.name));
    let _ = writeln!(pgn, "[Black \"{}\"]", escape(game.players.black.name));
    let _ = writeln!(pgn, "[Result \"{}\"]", result(game.outcome));
    for (tag, player) in [
        ("WhiteElo", game.players.white),
        ("BlackElo", game.players.black),
    ] {
        let _ = writeln!(
            pgn,
            "[{tag} \"{}\"]",
            player
                .rating
                .map_or("?".to_owned(), |r| i32::from(r).to_string())
        );
    }
    let _ = writeln!(pgn, "[LichessId \"{id}\"]");

    let mut pos = game.initial_position();
    if pos.variant() != Variant::Chess || pos != VariantPosition::new(Variant::Chess) {
        if pos.variant() != Variant::Chess {
            let _ = writeln!(pgn, "[Variant \"{}\"]", pos.variant());
        }
        let _ = writeln!(
            pgn,
            "[FEN \"{}\"]",
            Fen::from_position(&pos, EnPassantMode::Legal)
        );
        let _ = writeln!(pgn, "[SetUp \"1\"]");
    }
    pgn.push('\n');

    let mut decoder = PackedMovesDecoder::new(game.moves).ok();
    let mut tokens = Tokens::default();
    let mut ply = 0;
    while let Some(m) = decoder
        .as_mut()
        .and_then(|decoder| decoder.decode_next(&pos).ok().flatten())
    {
        if pos.turn() == Color::White {
            tokens.push(&mut pgn, &format!("{}.", pos.fullmoves()));
        } else if ply == 0 {
            tokens.push(&mut pgn, &format!("{}...", pos.fullmoves()));
        }
        let san = SanPlus::from_move_and_play_unchecked(&mut pos, m);
        tokens.push(&mut pgn, &san.to_string());
        ply += 1;
    }
    tokens.push(&mut pgn, result(game.outcome));
    pgn.push('\n');
    pgn
}

#[cfg(test)]
mod tests {
    use shakmaty::{ByColor, Chess, san::San};

    use super::*;
    use crate::packed::{BorrowedEventInfo, BorrowedGamePlayer, PackedMoves, Rating, RatingMode};

    #[test]
    fn test_masters_pgn() {
        let mut pos = VariantPosition::Chess(Chess::default());
        let mut encoder = PackedMoves::encoder();
        for san in "e4 e5 Nf3".split_whitespace() {
            let m = san.parse::<San>().unwrap().to_move(&pos).unwrap();
            encoder.encode_next(&pos, m);
            pos.play_unchecked(m);
        }
        let moves = encoder.finish();
        let game = BorrowedLichessGame {
            players: ByColor {
                white: BorrowedGamePlayer {
                    title: None,
                    name: "Carlsen, Magnus",
                    rating: Some(Rating::try_from(2837).unwrap()),
                },
                black: BorrowedGamePlayer {
                    title: None,
                    name: "Chadaev, Nikolay",
                    rating: Some(Rating::try_from(2580).unwrap()),
                },
            },
            date: LaxDate::from_ymd(2012, Some(7), None).unwrap(),
            outcome: KnownOutcome::Decisive {
                winner: Color::White,
            },
            speed: None,
            rating_mode: RatingMode::Rated,
            setup: None,
            event: Some(BorrowedEventInfo {
                event: "Wch Blitz",
                site: "Astana",
                round: "23",
            }),
            moves: &moves.0,
        };
        let pgn = masters_pgn("aAbqI4ey".parse().unwrap(), &game);
        assert_eq!(
            pgn,
            "[Event \"Wch Blitz\"]\n[Site \"Astana\"]\n[Date \"2012.07.??\"]\n[Round \"23\"]\n[White \"Carlsen, Magnus\"]\n[Black \"Chadaev, Nikolay\"]\n[Result \"1-0\"]\n[WhiteElo \"2837\"]\n[BlackElo \"2580\"]\n[LichessId \"aAbqI4ey\"]\n\n1. e4 e5 2. Nf3 1-0\n"
        );
    }
}
