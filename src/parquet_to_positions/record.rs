//! Fixed size records of one game reaching one position, and the layout of
//! chunks of records in scratch files.

use shakmaty::{Color, KnownOutcome, packed::PackedUciMove};

use crate::packed::{
    GameId, GroupHeader, Month, Rating, RatingGroup, Speed, Zobrist96, outcome_bits,
    outcome_from_bits,
};

/// Bytes of a record.
pub const RECORD_SIZE: usize = 25;

/// Bytes of the key of the positions column family: hash and month.
pub const KEY_SIZE: usize = Zobrist96::SIZE + 2;

/// One game reaching one position and playing a move there.
///
/// Layout, big endian, so that the byte order is the key order of the
/// positions column family, followed by the canonical order of games within
/// an entry (move, speed, rating, game id):
///
/// * 0..12: `Zobrist96` hash
/// * 12..14: month
/// * 14..16: move
/// * 16..18: speed (3 bits) and rating offset plus `RATING_BIAS` (13 bits)
/// * 18..24: game id
/// * 24: outcome
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Record(pub [u8; RECORD_SIZE]);

/// Added to the rating offset so that it is stored unsigned.
const RATING_BIAS: i32 = 1 << 12;

impl Record {
    /// Fails if the rating offset does not fit the 13 bits.
    pub fn new(
        hash: Zobrist96,
        month: Month,
        m: PackedUciMove,
        speed: Speed,
        rating: Rating,
        outcome: KnownOutcome,
        game_id: GameId,
    ) -> Option<Record> {
        let biased = i32::from(rating.offset()) + RATING_BIAS;
        if !(0..1 << 13).contains(&biased) {
            return None;
        }
        let mut bytes = [0; RECORD_SIZE];
        bytes[..12].copy_from_slice(hash.as_bytes());
        bytes[12..14].copy_from_slice(&month.to_bytes());
        bytes[14..16].copy_from_slice(&m.to_bytes());
        bytes[16..18]
            .copy_from_slice(&(u16::from(speed.bits()) << 13 | biased as u16).to_be_bytes());
        bytes[18..24].copy_from_slice(&game_id.to_bytes());
        bytes[24] = outcome_bits(shakmaty::Outcome::Known(outcome));
        Some(Record(bytes))
    }

    #[inline]
    pub fn hash(&self) -> Zobrist96 {
        Zobrist96::from_bytes(self.0[..12].try_into().expect("12 bytes"))
    }

    /// The top 64 bits of the hash.
    #[inline]
    pub fn hash_prefix(&self) -> u64 {
        u64::from_be_bytes(self.0[..8].try_into().expect("8 bytes"))
    }

    /// Hash and month: the key of the positions column family.
    #[inline]
    pub fn key(&self) -> &[u8; KEY_SIZE] {
        self.0[..KEY_SIZE].try_into().expect("key")
    }

    #[inline]
    pub fn month(&self) -> Month {
        Month::unpack(&mut &self.0[12..14])
    }

    #[inline]
    pub fn uci_move(&self) -> PackedUciMove {
        PackedUciMove::from_bytes(self.0[14..16].try_into().expect("2 bytes"))
    }

    #[inline]
    fn speed_and_rating(&self) -> u16 {
        u16::from_be_bytes(self.0[16..18].try_into().expect("2 bytes"))
    }

    #[inline]
    pub fn speed(&self) -> Speed {
        Speed::from_bits((self.speed_and_rating() >> 13) as u8).expect("valid speed")
    }

    #[inline]
    pub fn rating(&self) -> Rating {
        Rating::from_offset((i32::from(self.speed_and_rating() & 0x1fff) - RATING_BIAS) as i16)
    }

    #[inline]
    pub fn group_header(&self) -> GroupHeader {
        GroupHeader {
            speed: self.speed(),
            rating_group: RatingGroup::select(self.rating()),
        }
    }

    /// Move, speed and rating group: identifies the sub-entry within an
    /// entry, in canonical order.
    #[inline]
    pub fn sub_key(&self) -> SubKey {
        SubKey {
            m: self.0[14..16].try_into().expect("2 bytes"),
            speed: self.speed().bits(),
            rating_group: RatingGroup::select(self.rating()) as u8,
        }
    }

    #[inline]
    pub fn game_id(&self) -> GameId {
        GameId::unpack(&mut &self.0[18..24])
    }

    #[inline]
    pub fn outcome(&self) -> KnownOutcome {
        match outcome_from_bits(self.0[24]).expect("valid outcome") {
            shakmaty::Outcome::Known(outcome) => outcome,
            shakmaty::Outcome::Unknown => KnownOutcome::Decisive {
                // Unreachable: records are only created for known outcomes.
                winner: Color::White,
            },
        }
    }
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("hash", &self.hash())
            .field("month", &self.month())
            .field("move", &self.uci_move())
            .field("speed", &self.speed())
            .field("rating", &self.rating())
            .field("game_id", &self.game_id())
            .field("outcome", &self.outcome())
            .finish()
    }
}

/// Identifies a sub-entry within an entry. The derived order is the
/// canonical order of sub-entries.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubKey {
    pub m: [u8; 2],
    pub speed: u8,
    pub rating_group: u8,
}

impl SubKey {
    pub fn uci_move(&self) -> PackedUciMove {
        PackedUciMove::from_bytes(self.m)
    }

    pub fn group_header(&self) -> GroupHeader {
        GroupHeader::from_byte(self.speed | self.rating_group << 3).expect("valid group header")
    }
}

/// Records per scratch chunk.
pub const CHUNK_RECORDS: usize = (4 << 20) / RECORD_SIZE;

/// Bytes of a chunk of records.
pub const CHUNK_BYTES: usize = CHUNK_RECORDS * RECORD_SIZE;

/// Bits of the hash used to bucket the records of a chunk, after the
/// partition bits.
const BUCKET_BITS: u32 = 16;

/// Serializes a chunk of records for compression: records are bucketed by
/// the hash bits following the `partition_bits` and stored column major, so
/// that the leading hash bytes and the low entropy fields compress well.
/// Uses `records` as scratch space.
pub fn encode_chunk(records: &mut [Record], partition_bits: u32, out: &mut Vec<u8>) {
    let n = records.len();
    out.clear();
    out.resize(n * RECORD_SIZE, 0);
    if n == 0 {
        return;
    }

    // Counting sort by the bucket bits.
    let shift = 64 - partition_bits - BUCKET_BITS;
    let bucket = |r: &Record| (r.hash_prefix() >> shift) as usize & ((1 << BUCKET_BITS) - 1);
    let mut offsets = vec![0u32; 1 << BUCKET_BITS];
    for r in records.iter() {
        offsets[bucket(r)] += 1;
    }
    let mut sum = 0;
    for count in &mut offsets {
        let c = *count;
        *count = sum;
        sum += c;
    }
    let mut sorted: Vec<Record> = Vec::with_capacity(n);
    // SAFETY: every index in 0..n is written exactly once below, because
    // the offsets are the exclusive prefix sums of the bucket counts.
    unsafe {
        sorted.set_len(n);
    }
    for r in records.iter() {
        let b = bucket(r);
        sorted[offsets[b] as usize] = *r;
        offsets[b] += 1;
    }

    // Transpose.
    for (i, column) in out.chunks_exact_mut(n).enumerate() {
        for (dst, r) in column.iter_mut().zip(&sorted) {
            *dst = r.0[i];
        }
    }
    records.copy_from_slice(&sorted);
}

/// Appends the records of an encoded chunk.
pub fn decode_chunk(bytes: &[u8], out: &mut Vec<Record>) {
    assert_eq!(bytes.len() % RECORD_SIZE, 0, "chunk length");
    let n = bytes.len() / RECORD_SIZE;
    if n == 0 {
        return;
    }
    let start = out.len();
    out.resize(start + n, Record([0; RECORD_SIZE]));
    let records = &mut out[start..];
    for (i, column) in bytes.chunks_exact(n).enumerate() {
        for (r, src) in records.iter_mut().zip(column) {
            r.0[i] = *src;
        }
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_record_roundtrip_and_order() {
        arbtest(|u| {
            let hash: Zobrist96 = u.arbitrary()?;
            let month: Month = u.arbitrary()?;
            let m: shakmaty::uci::UciMove = u.arbitrary()?;
            let m = PackedUciMove::pack(m);
            let speed: Speed = u.arbitrary()?;
            let rating = Rating::from_offset(u.int_in_range(-4096..=4095)?);
            let outcome: KnownOutcome = u.arbitrary()?;
            let game_id: GameId = u.arbitrary()?;
            let record = Record::new(hash, month, m, speed, rating, outcome, game_id).unwrap();
            assert_eq!(record.hash(), hash);
            assert_eq!(record.month(), month);
            assert_eq!(record.uci_move(), m);
            assert_eq!(record.speed(), speed);
            assert_eq!(record.rating(), rating);
            assert_eq!(record.outcome(), outcome);
            assert_eq!(record.game_id(), game_id);
            assert_eq!(record.sub_key().uci_move(), m);
            assert_eq!(record.sub_key().group_header(), record.group_header());

            let other = Record::new(
                u.arbitrary()?,
                u.arbitrary()?,
                m,
                speed,
                rating,
                outcome,
                game_id,
            )
            .unwrap();
            assert_eq!(
                record.cmp(&other),
                (record.hash(), record.month()).cmp(&(other.hash(), other.month()))
            );
            Ok(())
        });

        assert!(
            Record::new(
                Zobrist96::from_bytes([0; 12]),
                Month::from_months_since_jan_1900(0),
                PackedUciMove::from_bytes([0; 2]),
                Speed::Blitz,
                Rating::from_offset(4096),
                KnownOutcome::Draw,
                GameId::from_ascii(b"abcdefgh").unwrap(),
            )
            .is_none()
        );
    }

    #[test]
    fn test_chunk_roundtrip() {
        arbtest(|u| {
            let mut records: Vec<Record> = Vec::new();
            for r in u.arbitrary_iter::<[u8; RECORD_SIZE]>()? {
                let mut r = Record(r?);
                r.0[24] &= 0b11;
                records.push(r);
            }
            let partition_bits = u.int_in_range(0..=20)?;
            let mut encoded = Vec::new();
            encode_chunk(&mut records, partition_bits, &mut encoded);
            assert_eq!(encoded.len(), records.len() * RECORD_SIZE);
            let mut decoded = Vec::new();
            decode_chunk(&encoded, &mut decoded);
            assert_eq!(decoded, records);
            let mut sorted = records.clone();
            sorted.sort();
            decoded.sort();
            assert_eq!(decoded, sorted);
            Ok(())
        });
    }
}
