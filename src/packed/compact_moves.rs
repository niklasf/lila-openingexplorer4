//! Compact fixed-width move encoding, after the "medium" level of
//! thomas-daniels/aix, generalized to all variants.
//!
//! Each move is encoded as the index of the moving piece among the pieces of
//! the side to move (`ceil(log2(n))` bits, with an extra slot for drops when
//! pockets exist), followed by a destination code whose width depends on the
//! role. Decoding only needs to replay moves on a board, not to generate
//! legal moves.

use shakmaty::{
    Bitboard, CastlingSide, Color, Move, Piece, Position, Rank, Role, Square, attacks,
    uci::UciMove, variant::VariantPosition,
};
use thiserror::Error;

use crate::packed::{BitReader, BitWriter, DecodeError};

#[derive(Debug, Error)]
pub enum CompactEncodeError {
    #[error("illegal uci move {0} at ply {1}")]
    IllegalUci(UciMove, usize),
    #[error("unsupported move {0} at ply {1}")]
    Unsupported(Move, usize),
}

/// Symbols produced for one move, for entropy analysis.
#[derive(Debug, Clone, Copy)]
pub struct CompactSymbols {
    pub piece_bits: u32,
    pub piece_index: u32,
    /// Role of the moved piece, or `None` for drops.
    pub role: Option<Role>,
    pub dest_bits: u32,
    pub dest_code: u32,
}

fn bits_for(slots: usize) -> u32 {
    if slots <= 1 {
        0
    } else {
        usize::BITS - (slots - 1).leading_zeros()
    }
}

fn role_index(role: Role) -> u32 {
    match role {
        Role::Pawn => 0,
        Role::Knight => 1,
        Role::Bishop => 2,
        Role::Rook => 3,
        Role::Queen => 4,
        Role::King => 5,
    }
}

fn role_from_index(index: u32) -> Option<Role> {
    Some(match index {
        0 => Role::Pawn,
        1 => Role::Knight,
        2 => Role::Bishop,
        3 => Role::Rook,
        4 => Role::Queen,
        5 => Role::King,
        _ => return None,
    })
}

const KNIGHT_OFFSETS: [i32; 8] = [6, 10, 15, 17, -6, -10, -15, -17];
const KING_OFFSETS: [i32; 8] = [1, 9, 8, 7, -1, -9, -8, -7];

/// Castling codes reuse the king directions that lead off the board from
/// the back rank.
fn castle_code(turn: Color, side: CastlingSide) -> u32 {
    match (turn, side) {
        (Color::White, CastlingSide::KingSide) => 5,
        (Color::White, CastlingSide::QueenSide) => 6,
        (Color::Black, CastlingSide::KingSide) => 1,
        (Color::Black, CastlingSide::QueenSide) => 2,
    }
}

fn pre_promotion_rank(turn: Color) -> Rank {
    turn.relative_rank(Rank::Seventh)
}

fn diff(from: Square, to: Square) -> i32 {
    i32::from(to) - i32::from(from)
}

/// Destination code and width for a move with the given role.
fn dest_code(turn: Color, role: Role, from: Square, m: Move) -> Option<(u32, u32)> {
    let to = m.to();
    Some(match m {
        Move::Castle { rook, .. } => {
            let side = if rook > from {
                CastlingSide::KingSide
            } else {
                CastlingSide::QueenSide
            };
            (castle_code(turn, side), 3)
        }
        Move::Normal { .. } | Move::EnPassant { .. } => match role {
            Role::Knight => (
                KNIGHT_OFFSETS.iter().position(|&o| o == diff(from, to))? as u32,
                3,
            ),
            Role::King => (
                KING_OFFSETS.iter().position(|&o| o == diff(from, to))? as u32,
                3,
            ),
            Role::Rook => (rook_code(from, to)?, 4),
            Role::Bishop => (bishop_code(from, to)?, 4),
            Role::Queen => {
                if from.file() == to.file() || from.rank() == to.rank() {
                    (rook_code(from, to)?, 5)
                } else {
                    (16 + bishop_code(from, to)?, 5)
                }
            }
            Role::Pawn => {
                let forward = if turn.is_white() { 1 } else { -1 };
                let file_diff = i32::from(to.file()) - i32::from(from.file());
                let rank_diff = (i32::from(to.rank()) - i32::from(from.rank())) * forward;
                let direction = match (file_diff, rank_diff) {
                    (0, 1) => 0,
                    (-1, 1) => 1,
                    (1, 1) => 2,
                    (0, 2) => 3,
                    _ => return None,
                };
                if from.rank() == pre_promotion_rank(turn) {
                    (direction << 3 | role_index(m.promotion()?), 5)
                } else {
                    if m.promotion().is_some() {
                        return None;
                    }
                    (direction, 2)
                }
            }
        },
        Move::Put { .. } => return None,
    })
}

fn rook_code(from: Square, to: Square) -> Option<u32> {
    if from.file() == to.file() {
        Some(u32::from(to.rank()))
    } else if from.rank() == to.rank() {
        Some(8 | u32::from(to.file()))
    } else {
        None
    }
}

fn bishop_code(from: Square, to: Square) -> Option<u32> {
    let d = diff(from, to);
    if d == 0 {
        None
    } else if d % 9 == 0 {
        Some(8 | u32::from(to.rank()))
    } else if d % 7 == 0 {
        Some(u32::from(to.rank()))
    } else {
        None
    }
}

fn square_offset(from: Square, offset: i32) -> Result<Square, DecodeError> {
    let index = i32::from(from) + offset;
    if (0..64).contains(&index) {
        Ok(Square::new(index as u32))
    } else {
        Err(DecodeError::RankOutOfRange(0))
    }
}

fn rook_dest(from: Square, code: u32) -> Square {
    if code & 8 == 0 {
        Square::from_coords(from.file(), Rank::new(code & 7))
    } else {
        Square::from_coords(shakmaty::File::new(code & 7), from.rank())
    }
}

fn bishop_dest(from: Square, code: u32) -> Result<Square, DecodeError> {
    let rank = Rank::new(code & 7);
    let rank_diff = i32::from(rank) - i32::from(from.rank());
    let file_diff = if code & 8 != 0 { rank_diff } else { -rank_diff };
    let file = i32::from(from.file()) + file_diff;
    if rank_diff != 0 && (0..8).contains(&file) {
        Ok(Square::from_coords(shakmaty::File::new(file as u32), rank))
    } else {
        Err(DecodeError::RankOutOfRange(0))
    }
}

/// Encodes moves in UCI notation (castling as king takes rook), advancing
/// the position. `observe` is called with the symbols of each move.
pub fn encode_compact(
    pos: &mut VariantPosition,
    moves: impl IntoIterator<Item = UciMove>,
    writer: &mut BitWriter,
    mut observe: impl FnMut(CompactSymbols),
) -> Result<usize, CompactEncodeError> {
    let mut plies = 0;
    for uci in moves {
        let m = uci
            .to_move(pos)
            .map_err(|_| CompactEncodeError::IllegalUci(uci, plies))?;
        let turn = pos.turn();
        let own = pos.board().by_color(turn);
        let slots = own.count() + usize::from(pos.pockets().is_some());
        let piece_bits = bits_for(slots);

        let symbols = match m {
            Move::Put { role, to } => {
                let piece_index = own.count() as u32;
                writer.write(piece_index, piece_bits);
                let code = role_index(role) << 6 | u32::from(to);
                writer.write(code, 9);
                CompactSymbols {
                    piece_bits,
                    piece_index,
                    role: None,
                    dest_bits: 9,
                    dest_code: code,
                }
            }
            _ => {
                let from = m.from().expect("from square");
                // Rank of `from` among own pieces in square order.
                let piece_index =
                    (own & Bitboard(u64::MAX >> (63 - u32::from(from)))).count() as u32 - 1;
                writer.write(piece_index, piece_bits);
                let role = m.role();
                let (code, bits) = dest_code(turn, role, from, m)
                    .ok_or(CompactEncodeError::Unsupported(m, plies))?;
                writer.write(code, bits);
                CompactSymbols {
                    piece_bits,
                    piece_index,
                    role: Some(role),
                    dest_bits: bits,
                    dest_code: code,
                }
            }
        };
        observe(symbols);
        pos.play_unchecked(m);
        plies += 1;
    }
    Ok(plies)
}

/// Decodes `plies` moves, advancing the position.
pub fn decode_compact(
    pos: &mut VariantPosition,
    reader: &mut BitReader<'_>,
    plies: usize,
    out: &mut Vec<Move>,
) -> Result<(), DecodeError> {
    for ply in 0..plies {
        let turn = pos.turn();
        let own = pos.board().by_color(turn);
        let has_pockets = pos.pockets().is_some();
        let slots = own.count() + usize::from(has_pockets);
        let piece_index = reader.read(bits_for(slots))? as usize;

        let m = if piece_index == own.count() {
            if !has_pockets {
                return Err(DecodeError::RankOutOfRange(ply));
            }
            let code = reader.read(9)?;
            Move::Put {
                role: role_from_index(code >> 6).ok_or(DecodeError::RankOutOfRange(ply))?,
                to: Square::new(code & 63),
            }
        } else {
            let from = own
                .into_iter()
                .nth(piece_index)
                .ok_or(DecodeError::RankOutOfRange(ply))?;
            let role = pos
                .board()
                .role_at(from)
                .ok_or(DecodeError::RankOutOfRange(ply))?;
            let board = pos.board();
            let normal = |to: Square, promotion: Option<Role>| Move::Normal {
                role,
                from,
                capture: board.role_at(to),
                to,
                promotion,
            };
            match role {
                Role::Knight => normal(
                    square_offset(from, KNIGHT_OFFSETS[reader.read(3)? as usize])?,
                    None,
                ),
                Role::King => {
                    let code = reader.read(3)?;
                    let castle = if from.rank() == turn.backrank() {
                        [CastlingSide::KingSide, CastlingSide::QueenSide]
                            .into_iter()
                            .find(|&side| castle_code(turn, side) == code)
                    } else {
                        None
                    };
                    match castle {
                        Some(side) => Move::Castle {
                            king: from,
                            rook: pos
                                .castles()
                                .rook(turn, side)
                                .ok_or(DecodeError::RankOutOfRange(ply))?,
                        },
                        None => normal(square_offset(from, KING_OFFSETS[code as usize])?, None),
                    }
                }
                Role::Rook => normal(rook_dest(from, reader.read(4)?), None),
                Role::Bishop => normal(bishop_dest(from, reader.read(4)?)?, None),
                Role::Queen => {
                    let code = reader.read(5)?;
                    if code & 16 == 0 {
                        normal(rook_dest(from, code), None)
                    } else {
                        normal(bishop_dest(from, code & 15)?, None)
                    }
                }
                Role::Pawn => {
                    let promoting = from.rank() == pre_promotion_rank(turn);
                    let (direction, promotion) = if promoting {
                        let code = reader.read(5)?;
                        (
                            code >> 3,
                            Some(
                                role_from_index(code & 7)
                                    .ok_or(DecodeError::RankOutOfRange(ply))?,
                            ),
                        )
                    } else {
                        (reader.read(2)?, None)
                    };
                    let forward = if turn.is_white() { 8 } else { -8 };
                    let offset = match direction {
                        0 => forward,
                        1 => forward - 1,
                        2 => forward + 1,
                        3 => 2 * forward,
                        _ => return Err(DecodeError::RankOutOfRange(ply)),
                    };
                    let to = square_offset(from, offset)?;
                    if direction == 1 || direction == 2 {
                        if board.role_at(to).is_none() {
                            Move::EnPassant { from, to }
                        } else {
                            normal(to, promotion)
                        }
                    } else {
                        normal(to, promotion)
                    }
                }
            }
        };
        pos.play_unchecked(m);
        out.push(m);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;
    use shakmaty::variant::Variant;

    use super::*;

    #[test]
    fn test_bits_for() {
        assert_eq!(bits_for(1), 0);
        assert_eq!(bits_for(2), 1);
        assert_eq!(bits_for(16), 4);
        assert_eq!(bits_for(17), 5);
        assert_eq!(bits_for(36), 6);
    }

    #[test]
    fn test_arbitrary_games_roundtrip() {
        arbtest(|u| {
            let variant: Variant = u.arbitrary()?;
            let mut pos = VariantPosition::new(variant);
            let mut moves = Vec::new();
            for _ in 0..u.int_in_range(0..=150)? {
                let legals = pos.legal_moves();
                if legals.is_empty() {
                    break;
                }
                let m = *u.choose(&legals)?;
                pos.play_unchecked(m);
                moves.push(m);
            }

            let mut writer = BitWriter::default();
            let mut pos = VariantPosition::new(variant);
            let ucis = moves
                .iter()
                .map(|m| m.to_uci(shakmaty::CastlingMode::Chess960));
            let plies = encode_compact(&mut pos, ucis, &mut writer, |_| {}).unwrap();
            assert_eq!(plies, moves.len());
            let bytes = writer.finish();

            let mut pos = VariantPosition::new(variant);
            let mut decoded = Vec::new();
            decode_compact(&mut pos, &mut BitReader::new(&bytes), plies, &mut decoded).unwrap();
            assert_eq!(decoded, moves, "variant {variant:?}");
            Ok(())
        })
        .budget_ms(3000);
    }
}

/// Squares a piece at `from` may move to, ignoring legality: attacked
/// squares not occupied by own pieces, castling rook squares for the king,
/// and pushes and captures (including en passant) for pawns. Only needs the
/// board, so encoder and decoder agree without legal move generation.
fn reachable(pos: &VariantPosition, from: Square, role: Role) -> Bitboard {
    let turn = pos.turn();
    let board = pos.board();
    let own = board.by_color(turn);
    match role {
        Role::Pawn => {
            let forward = if turn.is_white() { 8 } else { -8 };
            let mut targets = Bitboard::EMPTY;
            if let Some(single) = from.offset(forward) {
                if !board.occupied().contains(single) {
                    targets.add(single);
                    if let Some(double) = single.offset(forward) {
                        if !board.occupied().contains(double)
                            && (from.rank() == turn.relative_rank(Rank::Second)
                                || from.rank() == turn.relative_rank(Rank::First))
                        {
                            targets.add(double);
                        }
                    }
                }
            }
            let mut capturable = board.by_color(!turn);
            if let Some(ep) = pos.maybe_ep_square() {
                capturable.add(ep);
            }
            targets | (attacks::pawn_attacks(turn, from) & capturable)
        }
        Role::King => {
            let mut targets = attacks::attacks(
                from,
                Piece {
                    color: turn,
                    role: Role::King,
                },
                board.occupied(),
            ) & !own;
            if from.rank() == turn.backrank() {
                for side in [CastlingSide::KingSide, CastlingSide::QueenSide] {
                    if let Some(rook) = pos.castles().rook(turn, side) {
                        targets.add(rook);
                    }
                }
            }
            targets
        }
        _ => attacks::attacks(from, Piece { color: turn, role }, board.occupied()) & !own,
    }
}

/// Like `encode_compact`, but destinations are indexed among the squares the
/// piece can reach.
pub fn encode_reachable(
    pos: &mut VariantPosition,
    moves: impl IntoIterator<Item = UciMove>,
    writer: &mut BitWriter,
    mut observe: impl FnMut(CompactSymbols),
) -> Result<usize, CompactEncodeError> {
    let mut plies = 0;
    for uci in moves {
        let m = uci
            .to_move(pos)
            .map_err(|_| CompactEncodeError::IllegalUci(uci, plies))?;
        let turn = pos.turn();
        let own = pos.board().by_color(turn);
        let slots = own.count() + usize::from(pos.pockets().is_some());
        let piece_bits = bits_for(slots);

        let symbols = match m {
            Move::Put { role, to } => {
                let piece_index = own.count() as u32;
                writer.write(piece_index, piece_bits);
                let code = role_index(role) << 6 | u32::from(to);
                writer.write(code, 9);
                CompactSymbols {
                    piece_bits,
                    piece_index,
                    role: None,
                    dest_bits: 9,
                    dest_code: code,
                }
            }
            _ => {
                let from = m.from().expect("from square");
                let piece_index =
                    (own & Bitboard(u64::MAX >> (63 - u32::from(from)))).count() as u32 - 1;
                writer.write(piece_index, piece_bits);
                let role = m.role();
                let targets = reachable(pos, from, role);
                let to = m.to();
                if !targets.contains(to) {
                    return Err(CompactEncodeError::Unsupported(m, plies));
                }
                let dest_bits = bits_for(targets.count());
                let mut code =
                    (targets & Bitboard(u64::MAX >> (63 - u32::from(to)))).count() as u32 - 1;
                writer.write(code, dest_bits);
                let mut bits = dest_bits;
                if role == Role::Pawn && from.rank() == pre_promotion_rank(turn) {
                    let promotion = role_index(
                        m.promotion()
                            .ok_or(CompactEncodeError::Unsupported(m, plies))?,
                    );
                    writer.write(promotion, 3);
                    code = code << 3 | promotion;
                    bits += 3;
                }
                CompactSymbols {
                    piece_bits,
                    piece_index,
                    role: Some(role),
                    dest_bits: bits,
                    dest_code: code,
                }
            }
        };
        observe(symbols);
        pos.play_unchecked(m);
        plies += 1;
    }
    Ok(plies)
}

fn nth_square(bb: Bitboard, n: usize) -> Option<Square> {
    bb.into_iter().nth(n)
}

/// Decodes moves written by `encode_reachable`.
pub fn decode_reachable(
    pos: &mut VariantPosition,
    reader: &mut BitReader<'_>,
    plies: usize,
    out: &mut Vec<Move>,
) -> Result<(), DecodeError> {
    for ply in 0..plies {
        let turn = pos.turn();
        let own = pos.board().by_color(turn);
        let has_pockets = pos.pockets().is_some();
        let slots = own.count() + usize::from(has_pockets);
        let piece_index = reader.read(bits_for(slots))? as usize;

        let m = if piece_index == own.count() {
            if !has_pockets {
                return Err(DecodeError::RankOutOfRange(ply));
            }
            let code = reader.read(9)?;
            Move::Put {
                role: role_from_index(code >> 6).ok_or(DecodeError::RankOutOfRange(ply))?,
                to: Square::new(code & 63),
            }
        } else {
            let from = nth_square(own, piece_index).ok_or(DecodeError::RankOutOfRange(ply))?;
            let board = pos.board();
            let role = board
                .role_at(from)
                .ok_or(DecodeError::RankOutOfRange(ply))?;
            let targets = reachable(pos, from, role);
            let index = reader.read(bits_for(targets.count()))? as usize;
            let to = nth_square(targets, index).ok_or(DecodeError::RankOutOfRange(ply))?;
            let promotion = if role == Role::Pawn && from.rank() == pre_promotion_rank(turn) {
                Some(role_from_index(reader.read(3)?).ok_or(DecodeError::RankOutOfRange(ply))?)
            } else {
                None
            };
            if role == Role::King && own.contains(to) {
                Move::Castle {
                    king: from,
                    rook: to,
                }
            } else if role == Role::Pawn && from.file() != to.file() && board.role_at(to).is_none()
            {
                Move::EnPassant { from, to }
            } else {
                Move::Normal {
                    role,
                    from,
                    capture: board.role_at(to),
                    to,
                    promotion,
                }
            }
        };
        pos.play_unchecked(m);
        out.push(m);
    }
    Ok(())
}

#[cfg(test)]
mod reachable_tests {
    use arbtest::arbtest;
    use shakmaty::variant::Variant;

    use super::*;

    #[test]
    fn test_arbitrary_games_roundtrip() {
        arbtest(|u| {
            let variant: Variant = u.arbitrary()?;
            let mut pos = VariantPosition::new(variant);
            let mut moves = Vec::new();
            for _ in 0..u.int_in_range(0..=150)? {
                let legals = pos.legal_moves();
                if legals.is_empty() {
                    break;
                }
                let m = *u.choose(&legals)?;
                pos.play_unchecked(m);
                moves.push(m);
            }

            let mut writer = BitWriter::default();
            let mut pos = VariantPosition::new(variant);
            let ucis = moves
                .iter()
                .map(|m| m.to_uci(shakmaty::CastlingMode::Chess960));
            let plies = encode_reachable(&mut pos, ucis, &mut writer, |_| {}).unwrap();
            let bytes = writer.finish();

            let mut pos = VariantPosition::new(variant);
            let mut decoded = Vec::new();
            decode_reachable(&mut pos, &mut BitReader::new(&bytes), plies, &mut decoded).unwrap();
            assert_eq!(decoded, moves, "variant {variant:?}");
            Ok(())
        })
        .budget_ms(3000);
    }
}
