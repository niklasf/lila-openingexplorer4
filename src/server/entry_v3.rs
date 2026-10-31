//! Experimental variants of the packed entry format, to measure what group
//! lengths, group subtotals, descending months and a columnar group layout
//! buy for serving. Same cell model as `LichessEntry`, selected by `Flags`.
//!
//! ```text
//! entry      := (move group+)*
//! group      := header first_tag body
//! first_tag  := uint(first month << 1 | more)   (newest month if descending)
//!             | uint(first month << 2 | has subtotal << 1 | more)   with subtotals
//! body       := cell_body                                 if !more
//!             | [uint(len of rest)] rest                  if more
//! rest       := [subtotal] cell_body (uint(delta << 1 | more) cell_body)*
//!             | uint(n) months[n] cum[n] offsets[n] blob   (columnar)
//! cell_body  := SubStats game*      (games newest first if descending)
//! ```

use std::collections::BTreeMap;

use bytes::{Buf, BufMut};
use shakmaty::packed::PackedUciMove;

use crate::{
    packed::{
        EntryCell, GAMES_PER_CELL, GameId, GameSeq, GroupHeader, Month, RatingGroup, SubStats,
        pack_uint, unpack_uint,
    },
    server::{
        explore::{
            Aggregate, GameRef, GameSelection, LichessPrepared, MAX_LICHESS_GAMES, MAX_TOP_GAMES,
            PreparedMove, TopK, finish_lichess,
        },
        query::{LichessFilter, Limits},
    },
};

const LAST_GROUP_OF_MOVE: u8 = 0x80;

/// Bytes of a cumulative stats record in columnar groups.
const CUM_SIZE: usize = 4 + 4 + 4 + 8;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// Multi-month groups are prefixed with their byte length.
    pub lengths: bool,
    /// Multi-month groups start with their all-months statistics.
    pub subtotals: bool,
    /// Only groups with at least this many months get a subtotal (0 or 1
    /// and 2 mean all multi-month groups).
    pub subtotal_min_months: u8,
    /// Months within a group (and games within a cell) newest first.
    pub descending: bool,
    /// Multi-month groups store fixed-width month, cumulative stats and
    /// game offset arrays (implies lengths and descending).
    pub columnar: bool,
}

impl Flags {
    pub const PACKED: Flags = Flags {
        lengths: false,
        subtotals: false,
        subtotal_min_months: 0,
        descending: false,
        columnar: false,
    };

    pub fn columnar() -> Flags {
        Flags {
            lengths: true,
            subtotals: false,
            subtotal_min_months: 0,
            descending: true,
            columnar: true,
        }
    }

    pub fn name(&self) -> String {
        if self.columnar {
            return "C".to_owned();
        }
        let mut name = String::new();
        if self.lengths {
            name.push('L');
        }
        if self.subtotals {
            name.push('S');
            if self.subtotal_min_months > 2 {
                name.push_str(&self.subtotal_min_months.to_string());
            }
        }
        if self.descending {
            name.push('D');
        }
        if name.is_empty() {
            name.push_str("packed");
        }
        name
    }
}

impl Flags {
    /// Whether a multi-month group with the given number of months stores a
    /// subtotal.
    fn has_subtotal(&self, months: usize) -> bool {
        self.subtotals && months >= usize::from(self.subtotal_min_months)
    }
}

fn pack_games(
    buf: &mut Vec<u8>,
    cell: &EntryCell<'_>,
    flags: Flags,
    rank: &impl Fn(Month, GameSeq) -> u64,
) {
    let games = cell.packed_games();
    let order: Vec<&(GameSeq, GameId)> = if flags.descending {
        games.iter().rev().collect()
    } else {
        games.iter().collect()
    };
    for (seq, game_id) in order {
        pack_uint(buf, rank(cell.key.month(), *seq));
        game_id.pack(buf);
    }
}

fn pack_cell_body(
    buf: &mut Vec<u8>,
    cell: &EntryCell<'_>,
    rg: RatingGroup,
    flags: Flags,
    rank: &impl Fn(Month, GameSeq) -> u64,
) {
    cell.stats.pack(buf, rg);
    pack_games(buf, cell, flags, rank);
}

fn pack_group(
    buf: &mut Vec<u8>,
    cells: &[EntryCell<'_>],
    flags: Flags,
    rank: &impl Fn(Month, GameSeq) -> u64,
) {
    let rg = cells[0].key.group_header().rating_group;
    let order: Vec<&EntryCell<'_>> = if flags.descending {
        cells.iter().rev().collect()
    } else {
        cells.iter().collect()
    };
    let first_month = order[0].key.month().months_since_jan_1900();
    let more = order.len() > 1;
    let has_subtotal = more && flags.has_subtotal(order.len());
    if flags.subtotals {
        pack_uint(
            buf,
            u64::from(first_month) << 2 | u64::from(has_subtotal) << 1 | u64::from(more),
        );
    } else {
        pack_uint(buf, u64::from(first_month) << 1 | u64::from(more));
    }
    if !more {
        pack_cell_body(buf, order[0], rg, flags, rank);
        return;
    }

    let mut rest = Vec::new();
    if flags.columnar {
        pack_uint(&mut rest, order.len() as u64);
        for cell in &order {
            rest.put_u16(cell.key.month().months_since_jan_1900());
        }
        let mut cum = SubStats::default();
        for cell in &order {
            cum.merge(&cell.stats);
            rest.put_u32(u32::try_from(cum.white).expect("count"));
            rest.put_u32(u32::try_from(cum.draws).expect("count"));
            rest.put_u32(u32::try_from(cum.black).expect("count"));
            rest.put_i64(cum.rating_offset_sum);
        }
        let mut blob = Vec::new();
        for cell in &order {
            rest.put_u32(blob.len() as u32);
            pack_games(&mut blob, cell, flags, rank);
        }
        rest.extend_from_slice(&blob);
    } else {
        if has_subtotal {
            let mut sub = SubStats::default();
            for cell in &order {
                sub.merge(&cell.stats);
            }
            sub.pack(&mut rest, rg);
        }
        pack_cell_body(&mut rest, order[0], rg, flags, rank);
        let mut prev = first_month;
        for (k, cell) in order.iter().enumerate().skip(1) {
            let month = cell.key.month().months_since_jan_1900();
            let more = k + 1 < order.len();
            pack_uint(
                &mut rest,
                u64::from(prev.abs_diff(month)) << 1 | u64::from(more),
            );
            pack_cell_body(&mut rest, cell, rg, flags, rank);
            prev = month;
        }
    }
    if flags.lengths || flags.columnar {
        pack_uint(buf, rest.len() as u64);
    }
    buf.extend_from_slice(&rest);
}

/// Packs cells in canonical order with the given flags.
pub fn pack<'a, I>(buf: &mut Vec<u8>, cells: I, flags: Flags)
where
    I: Iterator<Item = EntryCell<'a>>,
{
    let cells: Vec<EntryCell<'a>> = cells.collect();
    let mut keys: Vec<(Month, GameSeq)> = cells
        .iter()
        .flat_map(|cell| {
            cell.packed_games()
                .iter()
                .map(move |(seq, _)| (cell.key.month(), *seq))
        })
        .collect();
    keys.sort_unstable();
    let rank = |month: Month, seq: GameSeq| keys.partition_point(|k| *k < (month, seq)) as u64;

    let mut i = 0;
    while i < cells.len() {
        let m = cells[i].key.uci_move();
        let mut j = i;
        while j < cells.len() && cells[j].key.uci_move() == m {
            j += 1;
        }
        buf.put_slice(&m.to_bytes());
        let mut g = i;
        while g < j {
            let header = cells[g].key.group_header().to_byte();
            let mut h = g;
            while h < j && cells[h].key.group_header().to_byte() == header {
                h += 1;
            }
            buf.put_u8(header | if h == j { LAST_GROUP_OF_MOVE } else { 0 });
            pack_group(buf, &cells[g..h], flags, &rank);
            g = h;
        }
        i = j;
    }
}

/// Per-group reading state shared by the row and columnar paths.
struct GroupContext<'a, 'b> {
    header: GroupHeader,
    m: PackedUciMove,
    filter: &'a LichessFilter,
    pm: &'a mut PreparedMove,
    total: &'a mut SubStats,
    months: Option<&'a mut BTreeMap<Month, SubStats>>,
    games: Option<&'a mut &'b mut GameSelection>,
    /// Older cells of this group can no longer contribute games.
    games_done: bool,
}

impl GroupContext<'_, '_> {
    fn add_stats(&mut self, month: Month, stats: &SubStats) {
        self.pm.stats.merge(stats);
        self.total.merge(stats);
        if let Some(months) = self.months.as_deref_mut() {
            months.entry(month).or_default().merge(stats);
        }
    }

    fn skip_games(buf: &mut &[u8], n: usize) {
        for _ in 0..n {
            unpack_uint(buf);
            buf.advance(GameId::SIZE);
        }
    }

    /// Reads the games of a cell. With descending order, the first game has
    /// the highest rank, and the whole cell (and all older cells) can be
    /// skipped if no heap accepts it.
    fn read_games(&mut self, buf: &mut &[u8], n: usize, descending: bool) {
        if n == 0 {
            return;
        }
        let Some(games) = self.games.as_deref_mut() else {
            Self::skip_games(buf, n);
            return;
        };
        if self.games_done {
            Self::skip_games(buf, n);
            return;
        }
        let mut remaining = n;
        if descending {
            let rank = unpack_uint(buf);
            let id = GameId::unpack(buf);
            remaining -= 1;
            if self.pm.latest.is_none_or(|(latest, _)| latest < rank) {
                self.pm.latest = Some((rank, id));
            }
            if !games.accepts_cell(self.header.speed, self.header.rating_group, rank) {
                self.games_done = true;
                Self::skip_games(buf, remaining);
                return;
            }
            games.push(GameRef {
                group: self.header.rating_group,
                speed: self.header.speed,
                rank,
                m: self.m,
                id,
            });
        }
        for _ in 0..remaining {
            let rank = unpack_uint(buf);
            let id = GameId::unpack(buf);
            if self.pm.latest.is_none_or(|(latest, _)| latest < rank) {
                self.pm.latest = Some((rank, id));
            }
            games.push(GameRef {
                group: self.header.rating_group,
                speed: self.header.speed,
                rank,
                m: self.m,
                id,
            });
        }
    }
}

fn month_of(n: u64) -> Month {
    Month::from_months_since_jan_1900(u16::try_from(n).expect("month"))
}

/// Aggregates an entry in the given format, like `explore::aggregate` does
/// for packed entries, skipping what the format allows to skip.
pub(crate) fn aggregate_v3(
    bytes: &[u8],
    flags: Flags,
    filter: &LichessFilter,
    mut games: Option<&mut GameSelection>,
    collect_months: bool,
) -> Aggregate {
    let mut agg = Aggregate::default();
    let no_range = filter.since.is_none() && filter.until.is_none();
    let mut buf = bytes;
    while !buf.is_empty() {
        let m = PackedUciMove::from_bytes([buf[0], buf[1]]);
        buf.advance(2);
        let mut pm = PreparedMove {
            m,
            stats: SubStats::default(),
            game: None,
            latest: None,
        };
        loop {
            let byte = buf.get_u8();
            let last = byte & LAST_GROUP_OF_MOVE != 0;
            let header = GroupHeader::from_byte(byte & !LAST_GROUP_OF_MOVE).expect("header");
            let rg = header.rating_group;
            let selected = filter.contains_speed(header.speed) && filter.contains_rating_group(rg);
            let tag = unpack_uint(&mut buf);
            let more = tag & 1 != 0;
            let (first_month, has_subtotal) = if flags.subtotals {
                (month_of(tag >> 2), tag & 2 != 0)
            } else {
                (month_of(tag >> 1), false)
            };
            let mut ctx = GroupContext {
                header,
                m,
                filter,
                pm: &mut pm,
                total: &mut agg.total,
                months: collect_months.then_some(&mut agg.months),
                games: games.as_mut(),
                games_done: false,
            };

            if !more {
                let stats = SubStats::unpack(&mut buf, rg);
                let n = stats.games().min(GAMES_PER_CELL as u64) as usize;
                if selected && filter.contains_month(first_month) {
                    ctx.add_stats(first_month, &stats);
                    ctx.read_games(&mut buf, n, flags.descending);
                } else {
                    GroupContext::skip_games(&mut buf, n);
                }
            } else {
                let len = (flags.lengths || flags.columnar).then(|| unpack_uint(&mut buf) as usize);
                let group_end = len.map(|len| buf.len() - len);
                if !selected && let Some(end) = group_end {
                    buf.advance(buf.len() - end);
                } else if flags.columnar {
                    read_columnar(&mut buf, &mut ctx, no_range && !collect_months);
                    debug_assert_eq!(Some(buf.len()), group_end);
                } else {
                    let subtotal = has_subtotal.then(|| SubStats::unpack(&mut buf, rg));
                    let use_subtotal =
                        selected && subtotal.is_some() && no_range && !collect_months;
                    if let Some(subtotal) = subtotal.filter(|_| use_subtotal) {
                        ctx.add_stats(first_month, &subtotal);
                    }
                    let mut month = first_month;
                    let mut more_cells = true;
                    let mut first = true;
                    while more_cells {
                        if !first {
                            let tag = unpack_uint(&mut buf);
                            let delta = u16::try_from(tag >> 1).expect("delta");
                            let since_1900 = month.months_since_jan_1900();
                            month = Month::from_months_since_jan_1900(if flags.descending {
                                since_1900 - delta
                            } else {
                                since_1900 + delta
                            });
                            more_cells = tag & 1 != 0;
                        }
                        first = false;
                        // Everything older is out of range, or no longer
                        // needed: skip the rest of the group.
                        if flags.descending
                            && let Some(end) = group_end
                            && (filter.since.is_some_and(|since| month < since)
                                || (use_subtotal && (ctx.games.is_none() || ctx.games_done)))
                        {
                            buf.advance(buf.len() - end);
                            break;
                        }
                        let stats = SubStats::unpack(&mut buf, rg);
                        let n = stats.games().min(GAMES_PER_CELL as u64) as usize;
                        if selected && filter.contains_month(month) {
                            if !use_subtotal {
                                ctx.add_stats(month, &stats);
                            }
                            ctx.read_games(&mut buf, n, flags.descending);
                        } else {
                            GroupContext::skip_games(&mut buf, n);
                        }
                    }
                    debug_assert!(group_end.is_none_or(|end| end == buf.len()));
                }
            }
            if last {
                break;
            }
        }
        if pm.stats.games() > 0 {
            pm.game = pm
                .latest
                .filter(|_| pm.stats.games() == 1)
                .map(|(_, id)| id);
            agg.moves.push(pm);
        }
    }
    agg
}

fn cum_at(cum: &[u8], i: usize) -> SubStats {
    let mut rec = &cum[i * CUM_SIZE..][..CUM_SIZE];
    SubStats {
        white: u64::from(rec.get_u32()),
        draws: u64::from(rec.get_u32()),
        black: u64::from(rec.get_u32()),
        rating_offset_sum: rec.get_i64(),
    }
}

fn sub_stats(a: &SubStats, b: &SubStats) -> SubStats {
    SubStats {
        rating_offset_sum: a.rating_offset_sum - b.rating_offset_sum,
        white: a.white - b.white,
        black: a.black - b.black,
        draws: a.draws - b.draws,
    }
}

/// Reads a selected columnar group (months newest first).
fn read_columnar(buf: &mut &[u8], ctx: &mut GroupContext<'_, '_>, use_total: bool) {
    let n = unpack_uint(buf) as usize;
    let months = &buf[..n * 2];
    buf.advance(n * 2);
    let cum = &buf[..n * CUM_SIZE];
    buf.advance(n * CUM_SIZE);
    let offsets = &buf[..n * 4];
    buf.advance(n * 4);
    let (month_words, rest) = months.as_chunks::<2>();
    debug_assert!(rest.is_empty());
    let month_at = |i: usize| Month::from_months_since_jan_1900(u16::from_be_bytes(month_words[i]));
    let offset_at = |i: usize| (&offsets[i * 4..]).get_u32() as usize;
    // Index range [lo, hi) of months within the filter (descending order).
    let lo = ctx.filter.until.map_or(0, |until| {
        month_words
            .partition_point(|w| Month::from_months_since_jan_1900(u16::from_be_bytes(*w)) > until)
    });
    let hi = ctx.filter.since.map_or(n, |since| {
        month_words
            .partition_point(|w| Month::from_months_since_jan_1900(u16::from_be_bytes(*w)) >= since)
    });
    if lo < hi {
        if use_total {
            ctx.add_stats(month_at(0), &cum_at(cum, n - 1));
        } else if ctx.months.is_none() {
            let upper = cum_at(cum, hi - 1);
            let stats = if lo == 0 {
                upper
            } else {
                sub_stats(&upper, &cum_at(cum, lo - 1))
            };
            ctx.add_stats(month_at(lo), &stats);
        } else {
            for i in lo..hi {
                let upper = cum_at(cum, i);
                let stats = if i == 0 {
                    upper
                } else {
                    sub_stats(&upper, &cum_at(cum, i - 1))
                };
                ctx.add_stats(month_at(i), &stats);
            }
        }
    }
    // The blob extends to the end of the group: find its end by walking the
    // games of the last cell.
    let blob_total = {
        let mut end = offset_at(n - 1);
        let mut rest = &buf[end..];
        let last_stats_games = {
            // Number of games of the last cell = min(8, games of that month).
            let last = cum_at(cum, n - 1);
            let prev = if n >= 2 {
                cum_at(cum, n - 2)
            } else {
                SubStats::default()
            };
            sub_stats(&last, &prev).games().min(GAMES_PER_CELL as u64) as usize
        };
        for _ in 0..last_stats_games {
            unpack_uint(&mut rest);
            rest.advance(GameId::SIZE);
        }
        end += buf[end..].len() - rest.len();
        end
    };
    if ctx.games.is_some() {
        for i in lo..hi {
            if ctx.games_done {
                break;
            }
            let start = offset_at(i);
            let end = if i + 1 < n {
                offset_at(i + 1)
            } else {
                blob_total
            };
            let mut cell = &buf[start..end];
            let games = {
                let upper = cum_at(cum, i);
                let stats = if i == 0 {
                    upper
                } else {
                    sub_stats(&upper, &cum_at(cum, i - 1))
                };
                stats.games().min(GAMES_PER_CELL as u64) as usize
            };
            ctx.read_games(&mut cell, games, true);
        }
    }
    buf.advance(blob_total);
}

pub fn prepare_lichess_v3(
    bytes: &[u8],
    flags: Flags,
    filter: &LichessFilter,
    limits: &Limits,
    history: bool,
) -> LichessPrepared {
    let mut games = GameSelection {
        recent: TopK::new(MAX_LICHESS_GAMES + MAX_TOP_GAMES),
        top: TopK::new(MAX_TOP_GAMES),
        top_group: filter.top_group(),
        candidates: TopK::new(0),
    };
    let agg = aggregate_v3(
        bytes,
        flags,
        filter,
        limits.games_wanted().then_some(&mut games),
        history,
    );
    finish_lichess(agg, games, filter, limits, history)
}

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;
    use crate::{
        packed::{EntryView, LichessEntry},
        server::explore::prepare_lichess,
    };

    fn ids(games: &[GameRef]) -> Vec<GameId> {
        games.iter().map(|g| g.id).collect()
    }

    #[test]
    fn test_variants_match_packed() {
        let variants = [
            Flags::PACKED,
            Flags {
                lengths: true,
                ..Flags::PACKED
            },
            Flags {
                lengths: true,
                subtotals: true,
                ..Flags::PACKED
            },
            Flags {
                lengths: true,
                descending: true,
                ..Flags::PACKED
            },
            Flags {
                lengths: true,
                subtotals: true,
                descending: true,
                ..Flags::PACKED
            },
            Flags {
                lengths: true,
                subtotals: true,
                subtotal_min_months: 3,
                descending: true,
                ..Flags::PACKED
            },
            Flags {
                descending: true,
                ..Flags::PACKED
            },
            Flags::columnar(),
        ];
        arbtest(|u| {
            let mut entry: LichessEntry = u.arbitrary()?;
            let mut packed = Vec::new();
            entry.pack_entire(&mut packed);
            let filter = LichessFilter {
                speeds: u.arbitrary()?,
                ratings: u.arbitrary()?,
                since: u.arbitrary::<bool>()?.then(|| {
                    Month::from_months_since_jan_1900(u.int_in_range(1300..=1600).unwrap())
                }),
                until: u.arbitrary::<bool>()?.then(|| {
                    Month::from_months_since_jan_1900(u.int_in_range(1300..=1600).unwrap())
                }),
            };
            let limits = Limits {
                moves: 12,
                top_games: u.int_in_range(0..=4)?,
                recent_games: u.int_in_range(0..=8)?,
            };
            let history: bool = u.arbitrary()?;
            let expected = prepare_lichess(
                &EntryView::unpack_entire(&packed),
                &filter,
                &limits,
                history,
            );
            for flags in variants {
                let mut buf = Vec::new();
                pack(&mut buf, entry.cells(), flags);
                if flags == Flags::PACKED {
                    assert_eq!(buf, packed, "packed variant is byte-identical");
                }
                let got = prepare_lichess_v3(&buf, flags, &filter, &limits, history);
                assert_eq!(got.total, expected.total, "{}", flags.name());
                assert_eq!(got.moves.len(), expected.moves.len(), "{}", flags.name());
                for (a, b) in got.moves.iter().zip(&expected.moves) {
                    assert_eq!(
                        (a.m, a.stats, a.game),
                        (b.m, b.stats, b.game),
                        "{}",
                        flags.name()
                    );
                }
                assert_eq!(
                    ids(&got.top_games),
                    ids(&expected.top_games),
                    "{}",
                    flags.name()
                );
                assert_eq!(
                    ids(&got.recent_games),
                    ids(&expected.recent_games),
                    "{}",
                    flags.name()
                );
                let h = |p: &LichessPrepared| {
                    p.history
                        .as_ref()
                        .map(|h| h.iter().map(|s| (s.month, s.stats)).collect::<Vec<_>>())
                };
                assert_eq!(h(&got), h(&expected), "{}", flags.name());
            }
            Ok(())
        })
        .budget_ms(3000);
    }
}
