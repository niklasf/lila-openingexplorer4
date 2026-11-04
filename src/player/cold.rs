//! Cold groups of the personal explorer: positions reached by a single game
//! (in the chunk or rebuild that wrote them), grouped into buckets by the
//! first bits of their position keys.

use crate::{
    packed::{ColdItemsView, pack_cold_items, pack_uint, unpack_uint},
    player::keys::GameSeq,
};

/// The value of a cold group: the games of the positions of a bucket, with
/// the 16 bits after the bucket bits of their keys as fingerprints.
///
/// ```text
/// value := uint(index bits) items
/// items := pack_cold_items(): (fingerprint, GameSeq) sorted, index bits wide
/// ```
///
/// Merging is the union of the items. Duplicates collapse: two positions
/// of a game with the same fingerprint have one item, and validating the
/// game by replay finds either position.
pub struct PlayerColdGroup;

impl PlayerColdGroup {
    /// Packs items, which need not be sorted or unique.
    pub fn pack(items: &mut Vec<(u16, u64)>, buf: &mut Vec<u8>) {
        items.sort_unstable();
        items.dedup();
        let max = items.iter().map(|&(_, seq)| seq).max().unwrap_or(0);
        let index_bits = u64::BITS - max.leading_zeros();
        pack_uint(buf, u64::from(index_bits));
        pack_cold_items(buf, items, index_bits);
    }

    fn view(value: &[u8]) -> ColdItemsView<'_> {
        let mut rest = value;
        let index_bits = unpack_uint(&mut rest) as u32;
        ColdItemsView::new(rest, index_bits)
    }

    /// The games that may have reached a position with the fingerprint.
    pub fn candidates(value: &[u8], fingerprint: u16) -> impl Iterator<Item = GameSeq> + '_ {
        PlayerColdGroup::view(value)
            .candidates(fingerprint)
            .map(|seq| GameSeq(seq as u32))
    }

    /// Adds the items of a packed group.
    pub fn unpack_into(value: &[u8], items: &mut Vec<(u16, u64)>) {
        items.extend(PlayerColdGroup::view(value).items());
    }

    /// Union of packed groups.
    pub fn merge<'a>(values: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
        let mut items = Vec::new();
        for value in values {
            PlayerColdGroup::unpack_into(value, &mut items);
        }
        let mut buf = Vec::new();
        PlayerColdGroup::pack(&mut items, &mut buf);
        buf
    }

    pub fn len(value: &[u8]) -> u64 {
        PlayerColdGroup::view(value).len()
    }
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_merge_is_union() {
        arbtest(|u| {
            let mut all = Vec::new();
            let mut values = Vec::new();
            for _ in 0..u.int_in_range(1..=4)? {
                let mut items: Vec<(u16, u64)> = (0..u.int_in_range(0..=50)?)
                    .map(|_| Ok((u.int_in_range(0..=20)?, u.int_in_range(0..=1_000_000)?)))
                    .collect::<arbitrary::Result<_>>()?;
                all.extend_from_slice(&items);
                let mut buf = Vec::new();
                PlayerColdGroup::pack(&mut items, &mut buf);
                values.push(buf);
            }
            let merged = PlayerColdGroup::merge(values.iter().map(Vec::as_slice));
            all.sort_unstable();
            all.dedup();
            assert_eq!(PlayerColdGroup::len(&merged), all.len() as u64);
            let probe = u.int_in_range(0..=20)?;
            let expected: Vec<GameSeq> = all
                .iter()
                .filter(|&&(f, _)| f == probe)
                .map(|&(_, seq)| GameSeq(seq as u32))
                .collect();
            assert_eq!(
                PlayerColdGroup::candidates(&merged, probe).collect::<Vec<_>>(),
                expected
            );
            Ok(())
        });
    }
}
