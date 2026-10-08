use std::num::NonZero;

use shakmaty::Bitboard;
use shakmaty::Move;
use shakmaty::Position;
use shakmaty::Rank;
use shakmaty::Role;
use shakmaty::Square;
use shakmaty::attacks;
use shakmaty::variant::VariantPosition;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("invalid packed moves")]
struct InvalidPackedMoves;

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    pending: u64,
    pending_len: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter::default()
    }

    fn write_bits(&mut self, value: u32, len: u32) {
        debug_assert!(len <= 32 && u64::from(value) < (1u64 << len));

        self.pending |= u64::from(value) << self.pending_len;
        self.pending_len += len;

        while self.pending_len >= 8 {
            self.bytes.push(self.pending as u8);
            self.pending >>= 8;
            self.pending_len -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.write_bits(1, 1);
        if self.pending_len > 0 {
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

    fn read_bits(&mut self, len: u32) -> Result<u32, InvalidPackedMoves> {
        debug_assert!(len <= 32);

        if self.pos + len as usize > self.len {
            return Err(InvalidPackedMoves);
        }

        let tail = &self.bytes[self.pos / 8..];
        let n = tail.len().min(8);
        let mut buf = [0; 8];
        buf[..n].copy_from_slice(&tail[..n]);

        let word = u64::from_le_bytes(buf) >> (self.pos % 8);

        self.pos += len as usize;

        Ok((word & ((1u64 << len) - 1)) as u32)
    }
}

fn len_for(slots: usize) -> u32 {
    slots.next_power_of_two().ilog2()
}

fn role_index(role: Role) -> u32 {
    u32::from(role) - 1
}

fn square_index(sq: Square, mask: Bitboard) -> u32 {
    let below = mask & Bitboard((1 << u32::from(sq)) - 1);
    below.count() as u32
}

fn pseudo_legal(pos: &VariantPosition, role: Role, from: Square) -> Bitboard {
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
    let piece_bits = len_for(pos.us().count() + usize::from(pos.pockets().is_some()));
    if let Some(from) = m.from() {
        let targets = pseudo_legal(pos, m.role(), from);
        let target_bits = len_for(targets.count());
        writer.write_bits(square_index(from, pos.us()), piece_bits);
        writer.write_bits(square_index(m.to(), targets), target_bits);
        if let Some(promotion) = m.promotion() {
            writer.write_bits(role_index(promotion), len_for(6));
        } else if piece_bits == 0 && target_bits == 0 {
            writer.write_bits(0, 1); // Force at least one bit per move
        }
    } else {
        // Drop move
        writer.write_bits(pos.us().count() as u32, piece_bits);
        writer.write_bits(role_index(m.role()), len_for(6));
        writer.write_bits(u32::from(m.to()), len_for(64));
    }
}

fn decode_move(reader: &mut BitReader, pos: &VariantPosition) -> Result<Move, InvalidPackedMoves> {
    let piece_bits = len_for(pos.us().count() + usize::from(pos.pockets().is_some()));
    let piece_index = reader.read_bits(piece_bits)? as usize;
    if piece_index < pos.us().count() {
        let from = pos
            .us()
            .into_iter()
            .nth(piece_index)
            .ok_or(InvalidPackedMoves)?;
        let role = pos.board().role_at(from).ok_or(InvalidPackedMoves)?;

        let targets = pseudo_legal(pos, role, from);
        let target_bits = len_for(targets.count());
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
                        Role::try_from(reader.read_bits(len_for(6))? + 1)
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
            role: Role::try_from(reader.read_bits(len_for(6))? + 1)
                .map_err(|_| InvalidPackedMoves)?,
            to: Square::new(reader.read_bits(len_for(64))?),
        })
    }
}

#[cfg(test)]
mod tests {
    use arbitrary::Arbitrary;
    use arbtest::arbtest;

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

            let mut writer = BitWriter::new();
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

            let mut bit_writer = BitWriter::new();
            let mut pos = game.pos.clone();
            for &m in &game.moves {
                encode_move(&mut bit_writer, &pos, m);
                pos.play_unchecked(m);
            }
            let bytes = bit_writer.finish();

            let mut bit_reader = BitReader::new(&bytes).unwrap();
            let mut pos = game.pos.clone();
            for m in game.moves {
                let decoded = decode_move(&mut bit_reader, &pos).unwrap();
                assert_eq!(decoded, m);
                pos.play_unchecked(decoded);
            }
            assert!(!bit_reader.has_remaining());

            Ok(())
        });
    }
}
