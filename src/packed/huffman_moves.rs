//! Compression of move sequences as Huffman coded ranks in a heuristically
//! ordered list of legal moves, following lichess-org/compression.
//!
//! Unlike the reference implementation, this supports all variants (including
//! drops), and ranks of 255 and above are escaped as the symbol 255 followed
//! by 16 raw bits.

use std::sync::OnceLock;

use shakmaty::{
    Bitboard, Color, Move, Position, Role, Square, attacks, uci::UciMove, variant::VariantPosition,
};
use thiserror::Error;

/// Piece square tables indexed by role (pawn, knight, bishop, rook, queen,
/// king) and square in a8..h1 order, from lichess-org/compression.
const PSQT: [[i32; 64]; 6] = [
    [
        0, 0, 0, 0, 0, 0, 0, 0, 50, 50, 50, 50, 50, 50, 50, 50, 10, 10, 20, 30, 30, 20, 10, 10, 5,
        5, 10, 25, 25, 10, 5, 5, 0, 0, 0, 20, 21, 0, 0, 0, 5, -5, -10, 0, 0, -10, -5, 5, 5, 10, 10,
        -31, -31, 10, 10, 5, 0, 0, 0, 0, 0, 0, 0, 0,
    ],
    [
        -50, -40, -30, -30, -30, -30, -40, -50, -40, -20, 0, 0, 0, 0, -20, -40, -30, 0, 10, 15, 15,
        10, 0, -30, -30, 5, 15, 20, 20, 15, 5, -30, -30, 0, 15, 20, 20, 15, 0, -30, -30, 5, 10, 15,
        15, 11, 5, -30, -40, -20, 0, 5, 5, 0, -20, -40, -50, -40, -30, -30, -30, -30, -40, -50,
    ],
    [
        -20, -10, -10, -10, -10, -10, -10, -20, -10, 0, 0, 0, 0, 0, 0, -10, -10, 0, 5, 10, 10, 5,
        0, -10, -10, 5, 5, 10, 10, 5, 5, -10, -10, 0, 10, 10, 10, 10, 0, -10, -10, 10, 10, 10, 10,
        10, 10, -10, -10, 5, 0, 0, 0, 0, 5, -10, -20, -10, -10, -10, -10, -10, -10, -20,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 5, 10, 10, 10, 10, 10, 10, 5, -5, 0, 0, 0, 0, 0, 0, -5, -5, 0, 0,
        0, 0, 0, 0, -5, -5, 0, 0, 0, 0, 0, 0, -5, -5, 0, 0, 0, 0, 0, 0, -5, -5, 0, 0, 0, 0, 0, 0,
        -5, 0, 0, 0, 5, 5, 0, 0, 0,
    ],
    [
        -20, -10, -10, -5, -5, -10, -10, -20, -10, 0, 0, 0, 0, 0, 0, -10, -10, 0, 5, 5, 5, 5, 0,
        -10, -5, 0, 5, 5, 5, 5, 0, -5, 0, 0, 5, 5, 5, 5, 0, -5, -10, 5, 5, 5, 5, 5, 0, -10, -10, 0,
        5, 0, 0, 0, 0, -10, -20, -10, -10, -5, -5, -10, -10, -20,
    ],
    [
        -30, -40, -40, -50, -50, -40, -40, -30, -30, -40, -40, -50, -50, -40, -40, -30, -30, -40,
        -40, -50, -50, -40, -40, -30, -30, -40, -40, -50, -50, -40, -40, -30, -20, -30, -30, -40,
        -40, -30, -30, -20, -10, -20, -20, -20, -20, -20, -20, -10, 20, 20, 0, 0, 0, 0, 20, 20, 0,
        30, 10, 0, 0, 10, 30, 0,
    ],
];

/// Huffman codes for move ranks, as (code, bits), from lichess-org/compression.
const CODES: [(u32, u8); 256] = [
    (0b00, 2),
    (0b100, 3),
    (0b1101, 4),
    (0b1010, 4),
    (0b0101, 4),
    (0b11101, 5),
    (0b10111, 5),
    (0b01110, 5),
    (0b01100, 5),
    (0b01000, 5),
    (0b111101, 6),
    (0b111001, 6),
    (0b111100, 6),
    (0b110011, 6),
    (0b110010, 6),
    (0b110000, 6),
    (0b101101, 6),
    (0b101100, 6),
    (0b011111, 6),
    (0b011011, 6),
    (0b010011, 6),
    (0b011010, 6),
    (0b1111111, 7),
    (0b1111101, 7),
    (0b1111110, 7),
    (0b1111100, 7),
    (0b1110000, 7),
    (0b1100011, 7),
    (0b0111101, 7),
    (0b0100101, 7),
    (0b0100100, 7),
    (0b11100010, 8),
    (0b11000101, 8),
    (0b01111001, 8),
    (0b111000111, 9),
    (0b110001001, 9),
    (0b011110001, 9),
    (0b011110000, 9),
    (0b1110001100, 10),
    (0b1100010000, 10),
    (0b11100011010, 11),
    (0b11000100010, 11),
    (0b111000110110, 12),
    (0b110001000110, 12),
    (0b1110001101110, 13),
    (0b1100010001110, 13),
    (0b11100011011110, 14),
    (0b11000100011110, 14),
    (0b111000110111110, 15),
    (0b110001000111110, 15),
    (0b1110001101111110, 16),
    (0b1100010001111110, 16),
    (0b11000100011111111, 17),
    (0b111000110111111111, 18),
    (0b111000110111111101, 18),
    (0b110001000111111100, 18),
    (0b1110001101111111100, 19),
    (0b1100010001111111011, 19),
    (0b11100011011111111011, 20),
    (0b11100011011111110010, 20),
    (0b11100011011111110000, 20),
    (0b111000110111111110101, 21),
    (0b111000110111111100110, 21),
    (0b111000110111111100010, 21),
    (0b110001000111111101001, 21),
    (0b110001000111111101000, 21),
    (0b1110001101111111101000, 22),
    (0b1110001101111111000110, 22),
    (0b1100010001111111010111, 22),
    (0b1100010001111111010101, 22),
    (0b11100011011111111010011, 23),
    (0b11100011011111110011110, 23),
    (0b11100011011111110001110, 23),
    (0b11100011011111110001111, 23),
    (0b11000100011111110101100, 23),
    (0b111000110111111100111011, 24),
    (0b111000110111111110100100, 24),
    (0b111000110111111100111111, 24),
    (0b111000110111111100111010, 24),
    (0b110001000111111101011011, 24),
    (0b110001000111111101010011, 24),
    (0b110001000111111101010001, 24),
    (0b1110001101111111001110011, 25),
    (0b1110001101111111001110001, 25),
    (0b1110001101111111001110010, 25),
    (0b1100010001111111010100101, 25),
    (0b1100010001111111010110100, 25),
    (0b1100010001111111010100001, 25),
    (0b11100011011111110011111011, 26),
    (0b11100011011111110011111001, 26),
    (0b11100011011111110011111010, 26),
    (0b11100011011111110011111000, 26),
    (0b11000100011111110101101011, 26),
    (0b111000110111111110100101111, 27),
    (0b110001000111111101011010100, 27),
    (0b110001000111111101011010101, 27),
    (0b111000110111111100111000010, 27),
    (0b111000110111111100111000011, 27),
    (0b110001000111111101010010011, 27),
    (0b1110001101111111101001010011, 28),
    (0b1100010001111111010100100101, 28),
    (0b1110001101111111001110000011, 28),
    (0b1110001101111111001110000010, 28),
    (0b1110001101111111001110000000, 28),
    (0b11100011011111110011100000010, 29),
    (0b11000100011111110101000001001, 29),
    (0b11100011011111110011100000011, 29),
    (0b11000100011111110101000001000, 29),
    (0b11000100011111110101000000011, 29),
    (0b110001000111111101010000011110, 30),
    (0b111000110111111110100101100110, 30),
    (0b111000110111111110100101010111, 30),
    (0b110001000111111101010000001101, 30),
    (0b111000110111111110100101100010, 30),
    (0b110001000111111101010000001000, 30),
    (0b110001000111111101010000000101, 30),
    (0b110001000111111101010000000000, 30),
    (0b110001000111111101010000001010, 30),
    (0b110001000111111101010010001101, 30),
    (0b110001000111111101010010010011, 30),
    (0b110001000111111101010010010010, 30),
    (0b110001000111111101010010010001, 30),
    (0b110001000111111101010010010000, 30),
    (0b110001000111111101010010001011, 30),
    (0b110001000111111101010010001010, 30),
    (0b110001000111111101010010001001, 30),
    (0b110001000111111101010010001000, 30),
    (0b110001000111111101010010000111, 30),
    (0b110001000111111101010010000110, 30),
    (0b110001000111111101010010000011, 30),
    (0b110001000111111101010010000010, 30),
    (0b110001000111111101010000011011, 30),
    (0b110001000111111101010000011010, 30),
    (0b110001000111111101010000011001, 30),
    (0b110001000111111101010000011000, 30),
    (0b110001000111111101010000010101, 30),
    (0b110001000111111101010000010100, 30),
    (0b110001000111111101010010000101, 30),
    (0b110001000111111101010010000100, 30),
    (0b110001000111111101010000011111, 30),
    (0b110001000111111101010000011101, 30),
    (0b110001000111111101010000011100, 30),
    (0b110001000111111101010010000001, 30),
    (0b110001000111111101010010000000, 30),
    (0b110001000111111101010000001111, 30),
    (0b110001000111111101010000001110, 30),
    (0b110001000111111101010000001100, 30),
    (0b110001000111111101010000010111, 30),
    (0b110001000111111101010000010110, 30),
    (0b110001000111111101010000001001, 30),
    (0b110001000111111101010000000100, 30),
    (0b110001000111111101010000000011, 30),
    (0b110001000111111101010000000010, 30),
    (0b110001000111111101010000000001, 30),
    (0b110001000111111101010000001011, 30),
    (0b110001000111111101010010001111, 30),
    (0b110001000111111101010010001110, 30),
    (0b110001000111111101010010001100, 30),
    (0b1110001101111111101001010111101, 31),
    (0b1110001101111111101001010111111, 31),
    (0b1110001101111111101001010100010, 31),
    (0b1110001101111111101001011011111, 31),
    (0b1110001101111111101001010100100, 31),
    (0b1110001101111111101001010111001, 31),
    (0b1110001101111111101001011011010, 31),
    (0b1110001101111111101001011010010, 31),
    (0b1110001101111111101001011010000, 31),
    (0b1110001101111111101001010111010, 31),
    (0b1110001101111111101001010001011, 31),
    (0b1110001101111111101001010001010, 31),
    (0b1110001101111111101001010001001, 31),
    (0b1110001101111111101001010001000, 31),
    (0b1110001101111111101001010000111, 31),
    (0b1110001101111111101001010000110, 31),
    (0b1110001101111111101001010000101, 31),
    (0b1110001101111111101001010000100, 31),
    (0b1110001101111111101001011010111, 31),
    (0b1110001101111111101001011010110, 31),
    (0b1110001101111111101001011010101, 31),
    (0b1110001101111111101001011010100, 31),
    (0b1110001101111111101001010110111, 31),
    (0b1110001101111111101001010110110, 31),
    (0b1110001101111111101001010010101, 31),
    (0b1110001101111111101001010010100, 31),
    (0b1110001101111111101001010110101, 31),
    (0b1110001101111111101001010110100, 31),
    (0b1110001101111111101001010010111, 31),
    (0b1110001101111111101001010010110, 31),
    (0b1110001101111111101001010110001, 31),
    (0b1110001101111111101001010110000, 31),
    (0b1110001101111111101001010010011, 31),
    (0b1110001101111111101001010010010, 31),
    (0b1110001101111111101001011101101, 31),
    (0b1110001101111111101001011101100, 31),
    (0b1110001101111111101001011101011, 31),
    (0b1110001101111111101001011101010, 31),
    (0b1110001101111111101001011100111, 31),
    (0b1110001101111111101001011100110, 31),
    (0b1110001101111111101001010010001, 31),
    (0b1110001101111111101001010010000, 31),
    (0b1110001101111111101001011100011, 31),
    (0b1110001101111111101001011100010, 31),
    (0b1110001101111111101001011100001, 31),
    (0b1110001101111111101001011100000, 31),
    (0b1110001101111111101001011101001, 31),
    (0b1110001101111111101001011101000, 31),
    (0b1110001101111111101001010001111, 31),
    (0b1110001101111111101001010001110, 31),
    (0b1110001101111111101001010000011, 31),
    (0b1110001101111111101001010000010, 31),
    (0b1110001101111111101001010001101, 31),
    (0b1110001101111111101001010001100, 31),
    (0b1110001101111111101001011001111, 31),
    (0b1110001101111111101001011001110, 31),
    (0b1110001101111111101001010000001, 31),
    (0b1110001101111111101001010000000, 31),
    (0b1110001101111111101001011011001, 31),
    (0b1110001101111111101001011011000, 31),
    (0b1110001101111111101001011100101, 31),
    (0b1110001101111111101001011100100, 31),
    (0b1110001101111111101001010101101, 31),
    (0b1110001101111111101001010101100, 31),
    (0b1110001101111111101001010110011, 31),
    (0b1110001101111111101001010110010, 31),
    (0b1110001101111111101001010101001, 31),
    (0b1110001101111111101001010101000, 31),
    (0b1110001101111111101001011101111, 31),
    (0b1110001101111111101001011101110, 31),
    (0b1110001101111111101001011001011, 31),
    (0b1110001101111111101001011001010, 31),
    (0b1110001101111111101001011000011, 31),
    (0b1110001101111111101001011000010, 31),
    (0b1110001101111111101001010101011, 31),
    (0b1110001101111111101001010101010, 31),
    (0b1110001101111111101001011001001, 31),
    (0b1110001101111111101001011001000, 31),
    (0b1110001101111111101001011000111, 31),
    (0b1110001101111111101001011000110, 31),
    (0b1110001101111111101001011000001, 31),
    (0b1110001101111111101001011000000, 31),
    (0b1110001101111111101001010111100, 31),
    (0b1110001101111111101001010100111, 31),
    (0b1110001101111111101001010100110, 31),
    (0b1110001101111111101001010111110, 31),
    (0b1110001101111111101001010100011, 31),
    (0b1110001101111111101001010100001, 31),
    (0b1110001101111111101001010100000, 31),
    (0b1110001101111111101001011011110, 31),
    (0b1110001101111111101001010100101, 31),
    (0b1110001101111111101001011011101, 31),
    (0b1110001101111111101001011011100, 31),
    (0b1110001101111111101001010111000, 31),
    (0b1110001101111111101001011011011, 31),
    (0b1110001101111111101001011010001, 31),
    (0b1110001101111111101001011010011, 31),
    (0b1110001101111111101001010111011, 31),
];

/// Symbol used to escape ranks that do not fit the code table.
const ESCAPE: u32 = 255;
const ESCAPE_BITS: u32 = 16;

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

fn psqt(role: Role, square: Square, turn: Color) -> i32 {
    let index = usize::from(square) ^ if turn.is_white() { 56 } else { 0 };
    PSQT[role_index(role) as usize][index]
}

/// Per position context for scoring moves.
struct Scorer {
    turn: Color,
    /// Squares attacked by enemy pawns.
    defended: Bitboard,
}

impl Scorer {
    fn new<P: Position>(pos: &P) -> Scorer {
        let turn = pos.turn();
        let enemy_pawns = pos.board().pawns() & pos.them();
        let mut defended = Bitboard::EMPTY;
        for sq in enemy_pawns {
            defended |= attacks::pawn_attacks(!turn, sq);
        }
        Scorer { turn, defended }
    }

    /// Heuristic score of a move. Higher scores come first.
    ///
    /// The role index is used as a tie breaker, which only matters for drops
    /// of different roles to the same square.
    fn key(&self, m: Move) -> (u32, u32) {
        let role = m.role();
        let to = m.to();
        let from = m.from().unwrap_or(to);
        let move_value = psqt(role, to, self.turn) - psqt(role, from, self.turn);
        let score = m.promotion().map_or(0, |p| role_index(p) << 26)
            + if m.is_capture() { 1 << 25 } else { 0 }
            + (if self.defended.contains(to) {
                5 - role_index(role)
            } else {
                6
            } << 22)
            + (((512 + move_value) as u32) << 12)
            + (u32::from(to) << 6)
            + u32::from(from);
        (score, role_index(role))
    }
}

/// Heuristic score of a move. Higher scores come first.
pub fn move_key<P: Position>(pos: &P, m: Move) -> (u32, u32) {
    Scorer::new(pos).key(m)
}

#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    acc: u64,
    bits: u32,
}

impl BitWriter {
    pub fn write(&mut self, value: u32, bits: u32) {
        debug_assert!(bits <= 32);
        self.acc = (self.acc << bits) | u64::from(value);
        self.bits += bits;
        while self.bits >= 8 {
            self.bits -= 8;
            self.bytes.push((self.acc >> self.bits) as u8);
        }
    }

    /// Pads with zero bits and returns the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.bytes.push((self.acc << (8 - self.bits)) as u8);
            self.bits = 0;
        }
        self.bytes
    }
}

pub struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
    acc: u64,
    bits: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> BitReader<'a> {
        BitReader {
            bytes,
            pos: 0,
            acc: 0,
            bits: 0,
        }
    }

    pub fn read_bit(&mut self) -> Result<u32, DecodeError> {
        if self.bits == 0 {
            let byte = *self.bytes.get(self.pos).ok_or(DecodeError::Truncated)?;
            self.pos += 1;
            self.acc = u64::from(byte);
            self.bits = 8;
        }
        self.bits -= 1;
        Ok(((self.acc >> self.bits) & 1) as u32)
    }

    pub fn read(&mut self, bits: u32) -> Result<u32, DecodeError> {
        let mut value = 0;
        for _ in 0..bits {
            value = (value << 1) | self.read_bit()?;
        }
        Ok(value)
    }
}

/// Binary tree for decoding: nodes are (zero child, one child) indices, or a
/// leaf symbol.
enum Node {
    Branch(u16, u16),
    Leaf(u8),
}

fn tree() -> &'static [Node] {
    static TREE: OnceLock<Vec<Node>> = OnceLock::new();
    TREE.get_or_init(|| {
        let mut nodes = vec![Node::Branch(0, 0)];
        for (symbol, &(code, bits)) in CODES.iter().enumerate() {
            let mut node = 0;
            for i in (0..bits).rev() {
                let bit = (code >> i) & 1;
                let next = match &nodes[node] {
                    Node::Branch(zero, one) => {
                        if bit == 0 {
                            *zero
                        } else {
                            *one
                        }
                    }
                    Node::Leaf(_) => panic!("code table is not prefix free"),
                };
                node = if next == 0 {
                    let index = nodes.len() as u16;
                    nodes.push(Node::Branch(0, 0));
                    if let Node::Branch(zero, one) = &mut nodes[node] {
                        if bit == 0 {
                            *zero = index;
                        } else {
                            *one = index;
                        }
                    }
                    index as usize
                } else {
                    next as usize
                };
            }
            nodes[node] = Node::Leaf(symbol as u8);
        }
        nodes
    })
}

fn write_rank(writer: &mut BitWriter, rank: usize) {
    if rank < ESCAPE as usize {
        let (code, bits) = CODES[rank];
        writer.write(code, u32::from(bits));
    } else {
        let (code, bits) = CODES[ESCAPE as usize];
        writer.write(code, u32::from(bits));
        writer.write(rank as u32, ESCAPE_BITS);
    }
}

fn read_rank(reader: &mut BitReader<'_>) -> Result<usize, DecodeError> {
    let nodes = tree();
    let mut node = 0;
    loop {
        match &nodes[node] {
            Node::Leaf(symbol) => {
                return Ok(if u32::from(*symbol) == ESCAPE {
                    reader.read(ESCAPE_BITS)? as usize
                } else {
                    usize::from(*symbol)
                });
            }
            Node::Branch(zero, one) => {
                node = if reader.read_bit()? == 0 { *zero } else { *one } as usize;
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum EncodeError {
    #[error("illegal move {0} at ply {1}")]
    IllegalMove(Move, usize),
}

#[derive(Debug, Error)]
#[error("illegal uci move {0} at ply {1}")]
pub struct EncodeUciError(pub UciMove, pub usize);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("truncated")]
    Truncated,
    #[error("rank out of range at ply {0}")]
    RankOutOfRange(usize),
}

/// Encodes moves played from `pos`, advancing the position.
pub fn encode_moves(
    pos: &mut VariantPosition,
    moves: impl IntoIterator<Item = Move>,
    writer: &mut BitWriter,
) -> Result<usize, EncodeError> {
    let mut plies = 0;
    for m in moves {
        let legals = pos.legal_moves();
        if !legals.contains(&m) {
            return Err(EncodeError::IllegalMove(m, plies));
        }
        encode_legal(pos, &legals, m, writer);
        pos.play_unchecked(m);
        plies += 1;
    }
    Ok(plies)
}

/// Encodes moves in UCI notation (castling as king takes rook, as produced
/// with `CastlingMode::Chess960`), advancing the position.
pub fn encode_uci_moves(
    pos: &mut VariantPosition,
    moves: impl IntoIterator<Item = UciMove>,
    writer: &mut BitWriter,
) -> Result<usize, EncodeUciError> {
    let mut plies = 0;
    for uci in moves {
        let legals = pos.legal_moves();
        let m = *legals
            .iter()
            .find(|&&legal| matches_uci(legal, uci))
            .ok_or(EncodeUciError(uci, plies))?;
        encode_legal(pos, &legals, m, writer);
        pos.play_unchecked(m);
        plies += 1;
    }
    Ok(plies)
}

fn matches_uci(m: Move, uci: UciMove) -> bool {
    match (m, uci) {
        (
            Move::Normal {
                from,
                to,
                promotion,
                ..
            },
            UciMove::Normal {
                from: uci_from,
                to: uci_to,
                promotion: uci_promotion,
            },
        ) => from == uci_from && to == uci_to && promotion == uci_promotion,
        (
            Move::EnPassant { from, to },
            UciMove::Normal {
                from: uci_from,
                to: uci_to,
                promotion: None,
            },
        ) => from == uci_from && to == uci_to,
        (
            Move::Castle { king, rook },
            UciMove::Normal {
                from,
                to,
                promotion: None,
            },
        ) => king == from && rook == to,
        (
            Move::Put { role, to },
            UciMove::Put {
                role: uci_role,
                to: uci_to,
            },
        ) => role == uci_role && to == uci_to,
        _ => false,
    }
}

fn encode_legal(pos: &VariantPosition, legals: &[Move], m: Move, writer: &mut BitWriter) {
    let scorer = Scorer::new(pos);
    let key = scorer.key(m);
    let rank = legals
        .iter()
        .filter(|&&legal| scorer.key(legal) > key)
        .count();
    write_rank(writer, rank);
}

/// Decodes `plies` moves, advancing the position.
pub fn decode_moves(
    pos: &mut VariantPosition,
    reader: &mut BitReader<'_>,
    plies: usize,
    out: &mut Vec<Move>,
) -> Result<(), DecodeError> {
    let mut keyed: Vec<((u32, u32), Move)> = Vec::new();
    for ply in 0..plies {
        let rank = read_rank(reader)?;
        let scorer = Scorer::new(pos);
        keyed.clear();
        keyed.extend(pos.legal_moves().into_iter().map(|m| (scorer.key(m), m)));
        if rank >= keyed.len() {
            return Err(DecodeError::RankOutOfRange(ply));
        }
        // Descending by key.
        let (_, m) = *keyed.select_nth_unstable_by(rank, |a, b| b.0.cmp(&a.0)).1;
        pos.play_unchecked(m);
        out.push(m);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;
    use shakmaty::{CastlingMode, uci::UciMove, variant::Variant};

    use super::*;

    #[test]
    fn test_huffman_roundtrip() {
        let mut writer = BitWriter::default();
        let ranks = [0usize, 1, 2, 5, 17, 100, 254, 255, 256, 1000, 0, 3];
        for &rank in &ranks {
            write_rank(&mut writer, rank);
        }
        let bytes = writer.finish();
        let mut reader = BitReader::new(&bytes);
        for &rank in &ranks {
            assert_eq!(read_rank(&mut reader).unwrap(), rank);
        }
    }

    #[test]
    fn test_game_roundtrip() {
        let ucis = [
            "e2e4", "c7c5", "g1f3", "d7d6", "d2d4", "c5d4", "f3d4", "g8f6", "b1c3", "a7a6", "c1e3",
            "e7e5", "d4b3", "c8e6", "f2f3", "f8e7", "d1d2", "e8h8", "e1a1",
        ];
        let mut pos = VariantPosition::new(Variant::Chess);
        let mut moves = Vec::new();
        for uci in ucis {
            let m = uci.parse::<UciMove>().unwrap().to_move(&pos).unwrap();
            pos.play_unchecked(m);
            moves.push(m);
        }

        let mut writer = BitWriter::default();
        let mut pos = VariantPosition::new(Variant::Chess);
        let plies = encode_moves(&mut pos, moves.iter().copied(), &mut writer).unwrap();
        assert_eq!(plies, moves.len());
        let bytes = writer.finish();
        assert!(bytes.len() < moves.len() * 2);

        let mut pos = VariantPosition::new(Variant::Chess);
        let mut decoded = Vec::new();
        decode_moves(&mut pos, &mut BitReader::new(&bytes), plies, &mut decoded).unwrap();
        assert_eq!(decoded, moves);
        assert_eq!(
            decoded
                .last()
                .unwrap()
                .to_uci(CastlingMode::Standard)
                .to_string(),
            "e1c1"
        );
    }

    #[test]
    fn test_arbitrary_games_roundtrip() {
        arbtest(|u| {
            let variant: Variant = u.arbitrary()?;
            let mut pos = VariantPosition::new(variant);
            let mut moves = Vec::new();
            for _ in 0..u.int_in_range(0..=120)? {
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
            let plies = encode_moves(&mut pos, moves.iter().copied(), &mut writer).unwrap();
            let bytes = writer.finish();

            let mut pos = VariantPosition::new(variant);
            let mut decoded = Vec::new();
            decode_moves(&mut pos, &mut BitReader::new(&bytes), plies, &mut decoded).unwrap();
            assert_eq!(decoded, moves);
            Ok(())
        });
    }
}
