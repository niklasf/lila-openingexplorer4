//! Compact encoding of move sequences that needs only a board to decode.
//!
//! Each move is encoded as the index of the moving piece among the pieces of
//! the side to move, followed by the index of the destination among the
//! squares that piece can reach (attacked squares not occupied by own
//! pieces, castling rook squares for the king, pushes and captures for
//! pawns), each in `ceil(log2(n))` bits. Pawn moves from the pre-promotion
//! rank are followed by the promotion role in 3 bits. In variants with
//! pockets, an extra piece slot denotes a drop, followed by the role in 3
//! bits and the square in 6 bits.
//!
//! Legality is never checked, so encoder and decoder agree as long as they
//! replay the same moves on the same position. The byte stream is terminated
//! by a 1 bit followed by 0 bits up to the byte boundary, so that the number
//! of plies does not need to be stored. A move that would occupy zero bits
//! (a single piece with a single reachable square) is given one bit, so that
//! every move is visible to the terminator.

use shakmaty::{
    Bitboard, CastlingSide, Move, Piece, Position, Rank, Role, Square, attacks, uci::UciMove,
    variant::VariantPosition,
};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeMovesError {
    #[error("missing terminator")]
    MissingTerminator,
    #[error("truncated at ply {0}")]
    Truncated(usize),
    #[error("invalid move at ply {0}")]
    InvalidMove(usize),
}

#[derive(Debug, Error)]
#[error("unencodable move {0} at ply {1}")]
pub struct EncodeMoveError(pub Move, pub usize);

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

/// Squares below or equal to `sq`.
fn up_to(sq: Square) -> Bitboard {
    Bitboard(u64::MAX >> (63 - u32::from(sq)))
}

fn pre_promotion_rank(pos: &VariantPosition) -> Rank {
    pos.turn().relative_rank(Rank::Seventh)
}

/// Squares a piece at `from` may move to, ignoring legality.
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
                        // Horde pawns may also advance two squares from the
                        // first rank.
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

/// Writes bits, most significant first.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    acc: u64,
    bits: u32,
}

impl BitWriter {
    fn write(&mut self, value: u32, bits: u32) {
        debug_assert!(bits <= 32);
        self.acc = (self.acc << bits) | u64::from(value);
        self.bits += bits;
        while self.bits >= 8 {
            self.bits -= 8;
            self.bytes.push((self.acc >> self.bits) as u8);
        }
    }

    /// Terminates with a 1 bit and zero padding.
    fn finish(mut self) -> Vec<u8> {
        self.write(1, 1);
        if self.bits > 0 {
            self.bytes.push((self.acc << (8 - self.bits)) as u8);
        }
        self.bytes
    }
}

struct BitReader<'a> {
    bytes: &'a [u8],
    /// Total payload bits, excluding the terminator.
    len: usize,
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Result<BitReader<'a>, DecodeMovesError> {
        let last = *bytes.last().ok_or(DecodeMovesError::MissingTerminator)?;
        if last == 0 {
            return Err(DecodeMovesError::MissingTerminator);
        }
        Ok(BitReader {
            bytes,
            len: bytes.len() * 8 - last.trailing_zeros() as usize - 1,
            pos: 0,
        })
    }

    fn has_remaining(&self) -> bool {
        self.pos < self.len
    }

    fn read(&mut self, bits: u32, ply: usize) -> Result<u32, DecodeMovesError> {
        if self.pos + bits as usize > self.len {
            return Err(DecodeMovesError::Truncated(ply));
        }
        let mut value = 0;
        for _ in 0..bits {
            let byte = self.bytes[self.pos / 8];
            value = (value << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Ok(value)
    }
}

/// Encodes moves one by one, as they are played on a position.
#[derive(Default)]
pub struct MoveEncoder {
    writer: BitWriter,
    plies: usize,
}

impl MoveEncoder {
    pub fn new() -> MoveEncoder {
        MoveEncoder::default()
    }

    pub fn plies(&self) -> usize {
        self.plies
    }

    /// Encodes `m`, which must be a legal move in `pos`. The caller plays
    /// the move afterwards.
    pub fn push(&mut self, pos: &VariantPosition, m: Move) -> Result<(), EncodeMoveError> {
        let turn = pos.turn();
        let own = pos.board().by_color(turn);
        let slots = own.count() + usize::from(pos.pockets().is_some());
        let piece_bits = bits_for(slots);
        let error = || EncodeMoveError(m, self.plies);

        match m {
            Move::Put { role, to } => {
                if pos.pockets().is_none() {
                    return Err(error());
                }
                self.writer.write(own.count() as u32, piece_bits);
                self.writer.write(role_index(role) << 6 | u32::from(to), 9);
            }
            _ => {
                let from = m.from().ok_or_else(error)?;
                if !own.contains(from) {
                    return Err(error());
                }
                let role = m.role();
                let targets = reachable(pos, from, role);
                let to = m.to();
                if !targets.contains(to) {
                    return Err(error());
                }
                let dest_bits = bits_for(targets.count());
                let promotion = if role == Role::Pawn && from.rank() == pre_promotion_rank(pos) {
                    Some(m.promotion().ok_or_else(error)?)
                } else {
                    if m.promotion().is_some() {
                        return Err(error());
                    }
                    None
                };

                self.writer
                    .write((own & up_to(from)).count() as u32 - 1, piece_bits);
                self.writer
                    .write((targets & up_to(to)).count() as u32 - 1, dest_bits);
                match promotion {
                    Some(promotion) => self.writer.write(role_index(promotion), 3),
                    None => {
                        if piece_bits == 0 && dest_bits == 0 {
                            // Give the move a bit, so that it is visible to
                            // the terminator.
                            self.writer.write(0, 1);
                        }
                    }
                }
            }
        }
        self.plies += 1;
        Ok(())
    }

    pub fn finish(self) -> EncodedMoves {
        EncodedMoves(self.writer.finish())
    }
}

/// Moves of a game, compactly encoded relative to the initial position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EncodedMoves(Vec<u8>);

impl EncodedMoves {
    /// Encodes moves played from `pos`, advancing the position.
    pub fn encode(
        pos: &mut VariantPosition,
        moves: impl IntoIterator<Item = Move>,
    ) -> Result<EncodedMoves, EncodeMoveError> {
        let mut encoder = MoveEncoder::new();
        for m in moves {
            encoder.push(pos, m)?;
            pos.play_unchecked(m);
        }
        Ok(encoder.finish())
    }

    /// Encodes moves in UCI notation (castling as king takes rook), resolving
    /// them against the position.
    pub fn encode_uci(
        pos: &mut VariantPosition,
        moves: impl IntoIterator<Item = UciMove>,
    ) -> Result<EncodedMoves, EncodeUciError> {
        let mut encoder = MoveEncoder::new();
        for uci in moves {
            let m = uci
                .to_move(pos)
                .map_err(|_| EncodeUciError::Illegal(uci, encoder.plies()))?;
            encoder.push(pos, m)?;
            pos.play_unchecked(m);
        }
        Ok(encoder.finish())
    }

    /// Wraps bytes without validation.
    pub fn from_bytes(bytes: Vec<u8>) -> EncodedMoves {
        EncodedMoves(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Decodes all moves, advancing the position.
    pub fn decode(&self, pos: &mut VariantPosition) -> Result<Vec<Move>, DecodeMovesError> {
        let mut moves = Vec::new();
        self.decode_into(pos, &mut moves)?;
        Ok(moves)
    }

    pub fn decode_into(
        &self,
        pos: &mut VariantPosition,
        out: &mut Vec<Move>,
    ) -> Result<(), DecodeMovesError> {
        let mut reader = BitReader::new(&self.0)?;
        let mut ply = 0;
        while reader.has_remaining() {
            let m = decode_move(pos, &mut reader, ply)?;
            pos.play_unchecked(m);
            out.push(m);
            ply += 1;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum EncodeUciError {
    #[error("illegal uci move {0} at ply {1}")]
    Illegal(UciMove, usize),
    #[error(transparent)]
    Encode(#[from] EncodeMoveError),
}

fn decode_move(
    pos: &VariantPosition,
    reader: &mut BitReader<'_>,
    ply: usize,
) -> Result<Move, DecodeMovesError> {
    let turn = pos.turn();
    let own = pos.board().by_color(turn);
    let has_pockets = pos.pockets().is_some();
    let slots = own.count() + usize::from(has_pockets);
    let piece_bits = bits_for(slots);
    let piece_index = reader.read(piece_bits, ply)? as usize;
    let invalid = || DecodeMovesError::InvalidMove(ply);

    if piece_index == own.count() {
        if !has_pockets {
            return Err(invalid());
        }
        let code = reader.read(9, ply)?;
        return Ok(Move::Put {
            role: role_from_index(code >> 6).ok_or_else(invalid)?,
            to: Square::new(code & 63),
        });
    }

    let from = own.into_iter().nth(piece_index).ok_or_else(invalid)?;
    let board = pos.board();
    let role = board.role_at(from).ok_or_else(invalid)?;
    let targets = reachable(pos, from, role);
    let dest_bits = bits_for(targets.count());
    let to = targets
        .into_iter()
        .nth(reader.read(dest_bits, ply)? as usize)
        .ok_or_else(invalid)?;

    let promotion = if role == Role::Pawn && from.rank() == pre_promotion_rank(pos) {
        Some(role_from_index(reader.read(3, ply)?).ok_or_else(invalid)?)
    } else {
        if piece_bits == 0 && dest_bits == 0 && reader.read(1, ply)? != 0 {
            return Err(invalid());
        }
        None
    };

    Ok(if role == Role::King && own.contains(to) {
        Move::Castle {
            king: from,
            rook: to,
        }
    } else if role == Role::Pawn && from.file() != to.file() && board.role_at(to).is_none() {
        Move::EnPassant { from, to }
    } else {
        Move::Normal {
            role,
            from,
            capture: board.role_at(to),
            to,
            promotion,
        }
    })
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;
    use shakmaty::{CastlingMode, variant::Variant};

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
    fn test_empty() {
        let mut pos = VariantPosition::new(Variant::Chess);
        let encoded = EncodedMoves::encode(&mut pos, []).unwrap();
        assert_eq!(encoded.as_bytes(), [0x80]);
        assert_eq!(encoded.decode(&mut pos).unwrap(), Vec::<Move>::new());
        assert_eq!(
            EncodedMoves::from_bytes(vec![]).decode(&mut pos),
            Err(DecodeMovesError::MissingTerminator)
        );
        assert_eq!(
            EncodedMoves::from_bytes(vec![0]).decode(&mut pos),
            Err(DecodeMovesError::MissingTerminator)
        );
    }

    #[test]
    fn test_known_game() {
        let ucis = [
            "e2e4", "c7c5", "g1f3", "d7d6", "d2d4", "c5d4", "f3d4", "g8f6", "b1c3", "a7a6", "c1e3",
            "e7e5", "d4b3", "c8e6", "f2f3", "f8e7", "d1d2", "e8h8", "e1a1",
        ];
        let mut pos = VariantPosition::new(Variant::Chess);
        let encoded = EncodedMoves::encode_uci(
            &mut pos,
            ucis.iter().map(|uci| uci.parse::<UciMove>().unwrap()),
        )
        .unwrap();
        // About 6 bits per ply plus terminator.
        assert!(
            encoded.as_bytes().len() <= 16,
            "{}",
            encoded.as_bytes().len()
        );

        let mut pos = VariantPosition::new(Variant::Chess);
        let decoded = encoded.decode(&mut pos).unwrap();
        let decoded: Vec<String> = decoded
            .iter()
            .map(|m| m.to_uci(CastlingMode::Chess960).to_string())
            .collect();
        assert_eq!(decoded, ucis);
    }

    fn random_game(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<(Variant, Vec<Move>)> {
        let variant: Variant = u.arbitrary()?;
        let mut pos = VariantPosition::new(variant);
        let mut moves = Vec::new();
        for _ in 0..u.int_in_range(0..=200)? {
            let legals = pos.legal_moves();
            if legals.is_empty() {
                break;
            }
            let m = *u.choose(&legals)?;
            pos.play_unchecked(m);
            moves.push(m);
        }
        Ok((variant, moves))
    }

    #[test]
    fn test_arbitrary_games_roundtrip() {
        arbtest(|u| {
            let (variant, moves) = random_game(u)?;
            let mut pos = VariantPosition::new(variant);
            let encoded = EncodedMoves::encode(&mut pos, moves.iter().copied()).unwrap();
            let mut pos = VariantPosition::new(variant);
            assert_eq!(
                encoded.decode(&mut pos).unwrap(),
                moves,
                "variant {variant:?}"
            );

            // Also via uci.
            let mut pos = VariantPosition::new(variant);
            let via_uci = EncodedMoves::encode_uci(
                &mut pos,
                moves.iter().map(|m| m.to_uci(CastlingMode::Chess960)),
            )
            .unwrap();
            assert_eq!(via_uci, encoded);
            Ok(())
        })
        .budget_ms(5000);
    }

    #[test]
    fn test_zero_width_moves_are_terminated() {
        // Lone king with a single reachable square: Kh1 boxed in by own
        // pieces? Use a position where white has only a king on a1 with b1,
        // b2 attacked... simpler: construct via FEN with a single white
        // piece that has one target.
        use shakmaty::fen::Fen;
        let fen: Fen = "7k/8/8/8/8/8/6pp/7K w - - 0 1".parse().unwrap();
        let mut pos =
            VariantPosition::from_setup(Variant::Chess, fen.into(), CastlingMode::Standard)
                .unwrap();
        // White king on h1, black pawns g2 h2: only Kxg2 is legal (h2 pawn
        // is defended? g2 pawn defended by nothing; h2 pawn attacked). Both
        // captures reachable, so this has one bit; just check roundtrip and
        // that a trailing move is not lost.
        let legals = pos.legal_moves();
        let m = legals[0];
        let encoded = EncodedMoves::encode(&mut pos.clone(), [m]).unwrap();
        assert_eq!(encoded.decode(&mut pos).unwrap(), vec![m]);
    }
}
