use std::num::NonZero;

use bytes::Buf;
use bytes::BufMut;
use shakmaty::Bitboard;
use shakmaty::Move;
use shakmaty::Position;
use shakmaty::Rank;
use shakmaty::Role;
use shakmaty::Square;
use shakmaty::attacks;
use shakmaty::variant::VariantPosition;
use thiserror::Error;

pub struct PackedMoves(pub Vec<u8>);

impl PackedMoves {
    pub fn pack_entire<B: BufMut>(&self, buf: &mut B) {
        buf.put_slice(&self.0);
    }

    pub fn unpack_entire<B: Buf>(buf: &mut B) -> Self {
        let mut bytes = Vec::new();
        bytes.put(buf);
        PackedMoves(bytes)
    }

    pub fn encode(pos: &mut VariantPosition, moves: impl IntoIterator<Item = Move>) -> Self {
        let mut writer = BitWriter::default();
        for m in moves {
            encode_move(&mut writer, pos, m);
            pos.play_unchecked(m);
        }
        PackedMoves(writer.finish())
    }

    pub fn decode(&self) -> Result<PackedMovesDecoder<'_>, InvalidPackedMoves> {
        let reader = BitReader::new(&self.0)?;
        Ok(PackedMovesDecoder { reader })
    }
}

pub struct PackedMovesDecoder<'a> {
    reader: BitReader<'a>,
}

impl PackedMovesDecoder<'_> {
    pub fn decode_next(
        &mut self,
        pos: &VariantPosition,
    ) -> Result<Option<Move>, InvalidPackedMoves> {
        if self.reader.has_remaining() {
            Ok(Some(decode_move(&mut self.reader, pos)?))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Error)]
#[error("invalid packed moves")]
pub struct InvalidPackedMoves;

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    pending: u64,
    pending_bits: u32,
}

impl BitWriter {
    fn write_bits(&mut self, value: u32, bits: u32) {
        debug_assert!(bits <= 32 && u64::from(value) < (1 << bits));

        self.pending |= u64::from(value) << self.pending_bits;
        self.pending_bits += bits;

        while self.pending_bits >= 8 {
            self.bytes.push(self.pending as u8);
            self.pending >>= 8;
            self.pending_bits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.write_bits(1, 1);
        if self.pending_bits > 0 {
            self.bytes.push(self.pending as u8);
        }
        self.bytes
    }
}

struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    len: usize,
}

impl BitReader<'_> {
    fn new(bytes: &[u8]) -> Result<BitReader<'_>, InvalidPackedMoves> {
        Ok(BitReader {
            bytes,
            pos: 0,
            len: bytes.len() * 8
                - bytes
                    .last()
                    .copied()
                    .and_then(NonZero::new)
                    .ok_or(InvalidPackedMoves)?
                    .leading_zeros() as usize
                - 1,
        })
    }

    fn has_remaining(&self) -> bool {
        self.pos < self.len
    }

    fn read_bits(&mut self, bits: u32) -> Result<u32, InvalidPackedMoves> {
        debug_assert!(bits <= 32);

        if self.pos + bits as usize > self.len {
            return Err(InvalidPackedMoves);
        }

        let tail = &self.bytes[self.pos / 8..];
        let n = tail.len().min(8);
        let mut buf = [0; 8];
        buf[..n].copy_from_slice(&tail[..n]);

        let word = u64::from_le_bytes(buf) >> (self.pos % 8);

        self.pos += bits as usize;

        Ok((word & ((1 << bits) - 1)) as u32)
    }
}

fn bits_for(slots: usize) -> u32 {
    slots.next_power_of_two().ilog2()
}

fn role_index(role: Role) -> u32 {
    u32::from(role) - 1
}

fn square_index(sq: Square, mask: Bitboard) -> u32 {
    let below = mask & Bitboard((1 << u32::from(sq)) - 1);
    below.count() as u32
}

fn pseudo_legal_targets(pos: &VariantPosition, role: Role, from: Square) -> Bitboard {
    match role {
        Role::Pawn => {
            let forward = pos.turn().fold_wb(8, -8);
            let one_step = Bitboard::from_square(from).shift(forward) & !pos.board().occupied();
            let two_step = one_step.shift(forward)
                & !pos.board().occupied()
                & pos.turn().fold_wb(Bitboard::SOUTH, Bitboard::NORTH);
            let captures = attacks::pawn_attacks(pos.turn(), from)
                & (pos.them() | Bitboard::from_iter(pos.maybe_ep_square()));
            one_step | two_step | captures
        }
        Role::Knight => attacks::knight_attacks(from) & !pos.us(),
        Role::Bishop => attacks::bishop_attacks(from, pos.board().occupied()) & !pos.us(),
        Role::Rook => attacks::rook_attacks(from, pos.board().occupied()) & !pos.us(),
        Role::Queen => attacks::queen_attacks(from, pos.board().occupied()) & !pos.us(),
        Role::King => {
            let mut targets = attacks::king_attacks(from) & !pos.us();
            if from.rank() == pos.turn().backrank() {
                targets.add(pos.castles().castling_rights() & pos.turn().backrank());
            }
            targets
        }
    }
}

fn encode_move(writer: &mut BitWriter, pos: &VariantPosition, m: Move) {
    let piece_bits = bits_for(pos.us().count() + usize::from(pos.pockets().is_some()));
    if let Some(from) = m.from() {
        let targets = pseudo_legal_targets(pos, m.role(), from);
        let target_bits = bits_for(targets.count());
        writer.write_bits(square_index(from, pos.us()), piece_bits);
        writer.write_bits(square_index(m.to(), targets), target_bits);
        if let Some(promotion) = m.promotion() {
            writer.write_bits(role_index(promotion), bits_for(6));
        } else if piece_bits == 0 && target_bits == 0 {
            writer.write_bits(0, 1); // Force at least one bit per move
        }
    } else {
        // Drop move
        writer.write_bits(pos.us().count() as u32, piece_bits);
        writer.write_bits(role_index(m.role()), bits_for(6));
        writer.write_bits(u32::from(m.to()), bits_for(64));
    }
}

fn decode_move(reader: &mut BitReader, pos: &VariantPosition) -> Result<Move, InvalidPackedMoves> {
    let piece_bits = bits_for(pos.us().count() + usize::from(pos.pockets().is_some()));
    let piece_index = reader.read_bits(piece_bits)? as usize;
    if piece_index < pos.us().count() {
        let from = pos
            .us()
            .into_iter()
            .nth(piece_index)
            .ok_or(InvalidPackedMoves)?;
        let role = pos.board().role_at(from).ok_or(InvalidPackedMoves)?;

        let targets = pseudo_legal_targets(pos, role, from);
        let target_bits = bits_for(targets.count());
        let to = targets
            .into_iter()
            .nth(reader.read_bits(target_bits)? as usize)
            .ok_or(InvalidPackedMoves)?;

        Ok(if role == Role::King && pos.us().contains(to) {
            Move::Castle {
                king: from,
                rook: to,
            }
        } else if role == Role::Pawn
            && from.file() != to.file()
            && !pos.board().occupied().contains(to)
        {
            Move::EnPassant { from, to }
        } else {
            let promotion =
                if role == Role::Pawn && to.rank() == pos.turn().relative_rank(Rank::Eighth) {
                    Some(
                        Role::try_from(reader.read_bits(bits_for(6))? + 1)
                            .map_err(|_| InvalidPackedMoves)?,
                    )
                } else {
                    if piece_bits == 0 && target_bits == 0 && reader.read_bits(1)? != 0 {
                        return Err(InvalidPackedMoves);
                    }
                    None
                };
            Move::Normal {
                role,
                from,
                capture: pos.board().role_at(to),
                to,
                promotion,
            }
        })
    } else {
        Ok(Move::Put {
            role: Role::try_from(reader.read_bits(bits_for(6))? + 1)
                .map_err(|_| InvalidPackedMoves)?,
            to: Square::new(reader.read_bits(bits_for(64))?),
        })
    }
}

#[cfg(test)]
mod tests {
    use arbitrary::Arbitrary;
    use arbtest::arbtest;
    use shakmaty::{CastlingMode, fen::Fen, san::San};

    use super::*;

    #[test]
    fn test_bits_roundtrip() {
        struct Group {
            value: u32,
            len: u32,
        }

        impl Arbitrary<'_> for Group {
            fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
                let len = u.int_in_range(0..=32)?;
                let value = u.int_in_range(0..=(1u64 << len) - 1)? as u32;
                Ok(Group { value, len })
            }
        }

        arbtest(|u| {
            let groups: Vec<Group> = u.arbitrary()?;

            let mut writer = BitWriter::default();
            for group in &groups {
                writer.write_bits(group.value, group.len);
            }
            let bytes = writer.finish();

            let mut reader = BitReader::new(&bytes).unwrap();
            for group in &groups {
                assert!(reader.has_remaining() || group.len == 0);
                assert_eq!(reader.read_bits(group.len).unwrap(), group.value);
            }
            assert!(!reader.has_remaining());

            Ok(())
        });
    }

    #[test]
    fn test_moves_roundtrip() {
        struct Game {
            pos: VariantPosition,
            moves: Vec<Move>,
        }

        impl Arbitrary<'_> for Game {
            fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
                let len: usize = u.arbitrary_len::<usize>()?;
                let mut game = Game {
                    pos: u.arbitrary()?,
                    moves: Vec::with_capacity(len),
                };

                let mut pos = game.pos.clone();
                for _ in 0..len {
                    let legals = pos.legal_moves();
                    if legals.is_empty() {
                        break;
                    }
                    let m = *u.choose(&legals)?;
                    pos.play_unchecked(m);
                    game.moves.push(m);
                }

                Ok(game)
            }
        }

        arbtest(|u| {
            let game: Game = u.arbitrary()?;

            let packed_moves =
                PackedMoves::encode(&mut game.pos.clone(), game.moves.iter().copied());

            let mut decoder = packed_moves.decode().unwrap();
            let mut pos = game.pos.clone();
            for m in game.moves {
                let decoded = decoder.decode_next(&pos).unwrap().unwrap();
                assert_eq!(decoded, m);
                pos.play_unchecked(decoded);
            }
            assert!(decoder.decode_next(&pos).unwrap().is_none());

            Ok(())
        });
    }

    #[test]
    fn test_only_moves() {
        // shoopi, www.chess.com 6/11/2013, Mate In 8
        let mut pos = VariantPosition::Chess(
            "rk1KB3/b1q1QQQR/bnqQ1q2/1Rq1Q1q1/2q1rQ1q/6B1/6QQ/8 w - - 0 1"
                .parse::<Fen>()
                .unwrap()
                .into_position(CastlingMode::Chess960)
                .unwrap(),
        );

        let moves: Vec<Move> = [
            "Qxc7+", "Qxc7+", "Qxc7+", "Qxc7+", "Qxc7+", "Qxc7+", "Bxc7+", "Kb7+", "Bb8+",
        ]
        .iter()
        .scan(pos.clone(), |pos, san| {
            let m = san.parse::<San>().unwrap().to_move(pos).unwrap();
            pos.play_unchecked(m);
            Some(m)
        })
        .collect();

        let packed_moves = PackedMoves::encode(&mut pos.clone(), moves.iter().copied());

        let mut decoder = packed_moves.decode().unwrap();
        for m in moves {
            let decoded = decoder.decode_next(&pos).unwrap().unwrap();
            assert_eq!(decoded, m);
            pos.play_unchecked(decoded);
        }

        assert!(decoder.decode_next(&pos).unwrap().is_none());
    }
}
