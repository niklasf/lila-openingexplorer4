//! Fast, allocation-light reading of game row groups.
//!
//! Decodes pages directly into flat buffers, avoiding the per-value reference
//! counting of the generic column readers. Supports exactly the encodings
//! produced by `GameColumns::properties()`: PLAIN and RLE_DICTIONARY values
//! with RLE definition levels, in data pages v1 or v2.

use bytes::Bytes;
use parquet::{
    basic::Encoding,
    column::page::Page,
    errors::{ParquetError, Result},
    file::reader::RowGroupReader,
};
use shakmaty::ByColor;

use crate::{
    packed::GameId,
    parquet::{GameRow, PlayerRow},
};

fn general(msg: impl Into<String>) -> ParquetError {
    ParquetError::General(msg.into())
}

/// Decodes a parquet RLE/bit-packed hybrid run of `n` values.
fn decode_hybrid(mut data: &[u8], bit_width: u32, n: usize, out: &mut Vec<u32>) -> Result<()> {
    debug_assert!(bit_width <= 32);
    let mask = if bit_width == 32 {
        u32::MAX
    } else {
        (1u32 << bit_width) - 1
    };
    let value_bytes = bit_width.div_ceil(8) as usize;

    while out.len() < n {
        // ULEB128 header
        let mut header: u64 = 0;
        let mut shift = 0;
        loop {
            let (&byte, rest) = data
                .split_first()
                .ok_or_else(|| general("truncated hybrid header"))?;
            data = rest;
            header |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }

        if header & 1 == 0 {
            // RLE run
            let count = (header >> 1) as usize;
            let (raw, rest) = data
                .split_at_checked(value_bytes)
                .ok_or_else(|| general("truncated rle value"))?;
            data = rest;
            let mut value = 0u32;
            for (i, &b) in raw.iter().enumerate() {
                value |= u32::from(b) << (8 * i);
            }
            let count = count.min(n - out.len());
            out.extend(std::iter::repeat_n(value & mask, count));
        } else {
            // Bit-packed run of groups of 8 values
            let groups = (header >> 1) as usize;
            let values = groups * 8;
            let bytes = groups * bit_width as usize;
            let take = values.min(n - out.len());
            let mut bit = 0usize;
            for _ in 0..take {
                let byte = bit >> 3;
                let mut word = [0u8; 8];
                let avail = data.len().saturating_sub(byte).min(8);
                word[..avail].copy_from_slice(&data[byte..byte + avail]);
                let v = (u64::from_le_bytes(word) >> (bit & 7)) as u32 & mask;
                out.push(v);
                bit += bit_width as usize;
            }
            data = data
                .get(bytes..)
                .ok_or_else(|| general("truncated bit-packed run"))?;
        }
    }
    Ok(())
}

/// Splits definition levels (max level 1) off a data page, returning the
/// remaining value bytes.
fn decode_def_levels(page: &Page, num_values: usize, out: &mut Vec<u32>) -> Result<Bytes> {
    match page {
        Page::DataPage {
            buf,
            def_level_encoding,
            ..
        } => {
            if *def_level_encoding != Encoding::RLE {
                return Err(general("unsupported definition level encoding"));
            }
            let len = u32::from_le_bytes(
                buf.get(..4)
                    .ok_or_else(|| general("truncated level length"))?
                    .try_into()
                    .expect("4 bytes"),
            ) as usize;
            let levels = buf
                .get(4..4 + len)
                .ok_or_else(|| general("truncated levels"))?;
            decode_hybrid(levels, 1, num_values, out)?;
            Ok(buf.slice(4 + len..))
        }
        Page::DataPageV2 {
            buf,
            def_levels_byte_len,
            rep_levels_byte_len,
            ..
        } => {
            let start = *rep_levels_byte_len as usize;
            let end = start + *def_levels_byte_len as usize;
            let levels = buf
                .get(start..end)
                .ok_or_else(|| general("truncated levels"))?;
            decode_hybrid(levels, 1, num_values, out)?;
            Ok(buf.slice(end..))
        }
        Page::DictionaryPage { .. } => Err(general("unexpected dictionary page")),
    }
}

fn page_parts(page: &Page) -> (Bytes, usize, Encoding) {
    match page {
        Page::DataPage {
            buf,
            num_values,
            encoding,
            ..
        }
        | Page::DataPageV2 {
            buf,
            num_values,
            encoding,
            ..
        }
        | Page::DictionaryPage {
            buf,
            num_values,
            encoding,
            ..
        } => (buf.clone(), *num_values as usize, *encoding),
    }
}

/// Variable length byte values, optionally nullable.
///
/// Values are not copied: each row references a range in one of the page
/// buffers (a data page for PLAIN pages, the dictionary page for dictionary
/// encoded pages).
#[derive(Default)]
pub struct BytesColumn {
    /// Buffer for each range of rows.
    buffers: Vec<Bytes>,
    /// First row of each buffer, ascending.
    buffer_starts: Vec<usize>,
    /// Start and length within the buffer of the row, for each row.
    /// A length of `u32::MAX` marks a null.
    ranges: Vec<(u32, u32)>,
    // Dictionary of the current column chunk.
    dict_buf: Bytes,
    dict_ranges: Vec<(u32, u32)>,
    // Scratch buffers, kept to reuse allocations.
    indices: Vec<u32>,
    levels: Vec<u32>,
}

const NULL_LEN: u32 = u32::MAX;

impl BytesColumn {
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn get(&self, i: usize) -> Option<&[u8]> {
        let (start, len) = self.ranges[i];
        if len == NULL_LEN {
            return None;
        }
        let buffer = self.buffer_starts.partition_point(|&s| s <= i) - 1;
        Some(&self.buffers[buffer][start as usize..start as usize + len as usize])
    }

    fn clear(&mut self) {
        self.buffers.clear();
        self.buffer_starts.clear();
        self.ranges.clear();
    }

    /// Reads the column `i` of the row group.
    pub fn read(&mut self, reader: &dyn RowGroupReader, i: usize, optional: bool) -> Result<()> {
        self.clear();
        let num_rows = reader.metadata().num_rows() as usize;
        self.ranges.reserve(num_rows);

        let mut pages = reader.get_column_page_reader(i)?;
        let mut has_dict = false;
        while let Some(page) = pages.get_next_page()? {
            let (buf, num_values, encoding) = page_parts(&page);
            if let Page::DictionaryPage { .. } = page {
                if encoding != Encoding::PLAIN && encoding != Encoding::PLAIN_DICTIONARY {
                    return Err(general("unsupported dictionary encoding"));
                }
                self.dict_ranges.clear();
                plain_byte_array_ranges(&buf, num_values, &mut self.dict_ranges)?;
                self.dict_buf = buf;
                has_dict = true;
                continue;
            }

            let values = if optional {
                self.levels.clear();
                decode_def_levels(&page, num_values, &mut self.levels)?
            } else {
                buf
            };
            let non_null = if optional {
                self.levels.iter().filter(|&&l| l == 1).count()
            } else {
                num_values
            };

            self.buffer_starts.push(self.ranges.len());
            match encoding {
                Encoding::PLAIN => {
                    self.buffers.push(values.clone());
                    if optional {
                        let mut ranges = Vec::with_capacity(non_null);
                        plain_byte_array_ranges(&values, non_null, &mut ranges)?;
                        self.emit_optional(ranges.into_iter().map(Ok))?;
                    } else {
                        plain_byte_array_ranges(&values, non_null, &mut self.ranges)?;
                    }
                }
                Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY => {
                    if !has_dict {
                        return Err(general("missing dictionary page"));
                    }
                    self.buffers.push(self.dict_buf.clone());
                    let (&bit_width, rest) = values
                        .split_first()
                        .ok_or_else(|| general("truncated dictionary indices"))?;
                    self.indices.clear();
                    decode_hybrid(rest, u32::from(bit_width), non_null, &mut self.indices)?;
                    let indices = std::mem::take(&mut self.indices);
                    let dict_ranges = std::mem::take(&mut self.dict_ranges);
                    let lookup = |idx: &u32| -> Result<(u32, u32)> {
                        dict_ranges
                            .get(*idx as usize)
                            .copied()
                            .ok_or_else(|| general("dictionary index out of range"))
                    };
                    let result = if optional {
                        self.emit_optional(indices.iter().map(lookup))
                    } else {
                        indices.iter().try_for_each(|idx| {
                            self.ranges.push(lookup(idx)?);
                            Ok(())
                        })
                    };
                    self.indices = indices;
                    self.dict_ranges = dict_ranges;
                    result?;
                }
                _ => return Err(general(format!("unsupported encoding {encoding}"))),
            }
        }

        if self.len() != num_rows {
            return Err(general(format!(
                "expected {num_rows} rows, decoded {}",
                self.len()
            )));
        }
        Ok(())
    }

    /// Appends decoded non-null ranges, interleaving nulls according to the
    /// most recently decoded levels.
    fn emit_optional(
        &mut self,
        mut values: impl Iterator<Item = Result<(u32, u32)>>,
    ) -> Result<()> {
        for &level in &self.levels {
            if level == 1 {
                let range = values
                    .next()
                    .ok_or_else(|| general("fewer values than levels"))??;
                self.ranges.push(range);
            } else {
                self.ranges.push((0, NULL_LEN));
            }
        }
        Ok(())
    }
}

/// Parses `n` PLAIN encoded byte arrays into (start, len) ranges.
fn plain_byte_array_ranges(buf: &[u8], n: usize, out: &mut Vec<(u32, u32)>) -> Result<()> {
    let mut pos = 0usize;
    for _ in 0..n {
        let len = u32::from_le_bytes(
            buf.get(pos..pos + 4)
                .ok_or_else(|| general("truncated byte array length"))?
                .try_into()
                .expect("4 bytes"),
        );
        pos += 4;
        if pos + len as usize > buf.len() {
            return Err(general("truncated byte array"));
        }
        out.push((pos as u32, len));
        pos += len as usize;
    }
    Ok(())
}

/// Nullable 32-bit integers.
#[derive(Default)]
pub struct Int32Column {
    values: Vec<Option<i32>>,
    dict: Vec<i32>,
    indices: Vec<u32>,
    levels: Vec<u32>,
}

impl Int32Column {
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn get(&self, i: usize) -> Option<i32> {
        self.values[i]
    }

    pub fn read(&mut self, reader: &dyn RowGroupReader, i: usize, optional: bool) -> Result<()> {
        self.values.clear();
        let num_rows = reader.metadata().num_rows() as usize;
        self.values.reserve(num_rows);

        let mut pages = reader.get_column_page_reader(i)?;
        let mut has_dict = false;
        while let Some(page) = pages.get_next_page()? {
            let (buf, num_values, encoding) = page_parts(&page);
            if let Page::DictionaryPage { .. } = page {
                if encoding != Encoding::PLAIN && encoding != Encoding::PLAIN_DICTIONARY {
                    return Err(general("unsupported dictionary encoding"));
                }
                self.dict.clear();
                plain_int32(&buf, num_values, |v| self.dict.push(v))?;
                has_dict = true;
                continue;
            }

            let values = if optional {
                self.levels.clear();
                decode_def_levels(&page, num_values, &mut self.levels)?
            } else {
                self.levels.clear();
                self.levels.extend(std::iter::repeat_n(1, num_values));
                buf
            };
            let non_null = self.levels.iter().filter(|&&l| l == 1).count();

            match encoding {
                Encoding::PLAIN => {
                    let mut plain = Vec::with_capacity(non_null);
                    plain_int32(&values, non_null, |v| plain.push(v))?;
                    let mut plain = plain.into_iter();
                    for &level in &self.levels {
                        self.values.push(if level == 1 {
                            Some(
                                plain
                                    .next()
                                    .ok_or_else(|| general("fewer values than levels"))?,
                            )
                        } else {
                            None
                        });
                    }
                }
                Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY => {
                    if !has_dict {
                        return Err(general("missing dictionary page"));
                    }
                    let (&bit_width, rest) = values
                        .split_first()
                        .ok_or_else(|| general("truncated dictionary indices"))?;
                    self.indices.clear();
                    decode_hybrid(rest, u32::from(bit_width), non_null, &mut self.indices)?;
                    let mut indices = self.indices.iter();
                    for &level in &self.levels {
                        self.values.push(if level == 1 {
                            let idx = *indices
                                .next()
                                .ok_or_else(|| general("fewer values than levels"))?;
                            Some(
                                *self
                                    .dict
                                    .get(idx as usize)
                                    .ok_or_else(|| general("dictionary index out of range"))?,
                            )
                        } else {
                            None
                        });
                    }
                }
                _ => return Err(general(format!("unsupported encoding {encoding}"))),
            }
        }

        if self.values.len() != num_rows {
            return Err(general(format!(
                "expected {num_rows} rows, decoded {}",
                self.values.len()
            )));
        }
        Ok(())
    }
}

fn plain_int32(mut buf: &[u8], n: usize, mut f: impl FnMut(i32)) -> Result<()> {
    for _ in 0..n {
        let (v, rest) = buf
            .split_at_checked(4)
            .ok_or_else(|| general("truncated int32"))?;
        f(i32::from_le_bytes(v.try_into().expect("4 bytes")));
        buf = rest;
    }
    Ok(())
}

/// Required fixed length byte values (PLAIN only).
#[derive(Default)]
pub struct FixedColumn {
    width: usize,
    data: Vec<u8>,
}

impl FixedColumn {
    pub fn len(&self) -> usize {
        if self.width == 0 {
            0
        } else {
            self.data.len() / self.width
        }
    }

    pub fn get(&self, i: usize) -> &[u8] {
        &self.data[i * self.width..(i + 1) * self.width]
    }

    pub fn read(&mut self, reader: &dyn RowGroupReader, i: usize, width: usize) -> Result<()> {
        self.width = width;
        self.data.clear();
        let num_rows = reader.metadata().num_rows() as usize;
        self.data.reserve(num_rows * width);

        let mut pages = reader.get_column_page_reader(i)?;
        while let Some(page) = pages.get_next_page()? {
            let (buf, num_values, encoding) = page_parts(&page);
            if let Page::DictionaryPage { .. } = page {
                return Err(general("unexpected dictionary page for fixed column"));
            }
            if encoding != Encoding::PLAIN {
                return Err(general(format!("unsupported encoding {encoding}")));
            }
            let values = buf
                .get(..num_values * width)
                .ok_or_else(|| general("truncated fixed values"))?;
            self.data.extend_from_slice(values);
        }

        if self.len() != num_rows {
            return Err(general(format!(
                "expected {num_rows} rows, decoded {}",
                self.len()
            )));
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct FastPlayerColumns {
    title: BytesColumn,
    name: BytesColumn,
    rating: Int32Column,
}

impl FastPlayerColumns {
    fn read(&mut self, reader: &dyn RowGroupReader, base: usize) -> Result<()> {
        self.title.read(reader, base, true)?;
        self.name.read(reader, base + 1, false)?;
        self.rating.read(reader, base + 2, true)?;
        Ok(())
    }

    fn get(&self, i: usize) -> PlayerRow<'_> {
        PlayerRow {
            title: self.title.get(i),
            name: self.name.get(i).expect("required name"),
            rating: self.rating.get(i),
        }
    }
}

/// A row group of games, decoded into flat buffers.
#[derive(Default)]
pub struct FastGameColumns {
    game_id: FixedColumn,
    players: ByColor<FastPlayerColumns>,
    date_year: Int32Column,
    date_month: Int32Column,
    date_day: Int32Column,
    outcome: BytesColumn,
    speed: BytesColumn,
    rating_mode: BytesColumn,
    setup: BytesColumn,
    moves: BytesColumn,
}

impl FastGameColumns {
    pub fn len(&self) -> usize {
        self.game_id.len()
    }

    pub fn read_columns(&mut self, reader: &dyn RowGroupReader) -> Result<()> {
        self.game_id.read(reader, 0, GameId::SIZE)?;
        self.players.white.read(reader, 1)?;
        self.players.black.read(reader, 4)?;
        self.date_year.read(reader, 7, true)?;
        self.date_month.read(reader, 8, true)?;
        self.date_day.read(reader, 9, true)?;
        self.outcome.read(reader, 10, false)?;
        self.speed.read(reader, 11, true)?;
        self.rating_mode.read(reader, 12, false)?;
        self.setup.read(reader, 13, true)?;
        self.moves.read(reader, 14, false)?;
        Ok(())
    }

    pub fn get(&self, i: usize) -> GameRow<'_> {
        GameRow {
            game_id: GameId::unpack(&mut self.game_id.get(i)),
            players: ByColor {
                white: self.players.white.get(i),
                black: self.players.black.get(i),
            },
            date_year: self.date_year.get(i),
            date_month: self.date_month.get(i),
            date_day: self.date_day.get(i),
            outcome: self.outcome.get(i).expect("required outcome"),
            speed: self.speed.get(i),
            rating_mode: self.rating_mode.get(i).expect("required rating mode"),
            setup: self.setup.get(i),
            moves: self.moves.get(i).expect("required moves"),
        }
    }

    pub fn rows(&self) -> impl Iterator<Item = GameRow<'_>> {
        (0..self.len()).map(|i| self.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_hybrid_rle() {
        // RLE run: header = count << 1 = 5 << 1 = 10, value 3 (1 byte for width 2)
        let mut out = Vec::new();
        decode_hybrid(&[10, 3], 2, 5, &mut out).unwrap();
        assert_eq!(out, vec![3, 3, 3, 3, 3]);
    }

    #[test]
    fn test_decode_hybrid_bitpacked() {
        // One group of 8 values, bit width 1: header = (1 << 1) | 1 = 3,
        // values 1,0,1,1,0,0,1,0 packed LSB first = 0b01001101 = 0x4d.
        let mut out = Vec::new();
        decode_hybrid(&[3, 0x4d], 1, 8, &mut out).unwrap();
        assert_eq!(out, vec![1, 0, 1, 1, 0, 0, 1, 0]);

        // Only 5 of the 8 values requested.
        let mut out = Vec::new();
        decode_hybrid(&[3, 0x4d], 1, 5, &mut out).unwrap();
        assert_eq!(out, vec![1, 0, 1, 1, 0]);
    }

    #[test]
    fn test_decode_hybrid_bitpacked_wide() {
        // Bit width 12, 8 values: 0x123, 0x456, 0x789, 0xabc, 0xdef, 0x111, 0x222, 0x333.
        let values = [0x123u32, 0x456, 0x789, 0xabc, 0xdef, 0x111, 0x222, 0x333];
        let mut bits = 0u128;
        for (i, &v) in values.iter().enumerate() {
            bits |= u128::from(v) << (12 * i);
        }
        let mut data = vec![3u8];
        data.extend_from_slice(&bits.to_le_bytes()[..12]);
        let mut out = Vec::new();
        decode_hybrid(&data, 12, 8, &mut out).unwrap();
        assert_eq!(out, values);
    }
}
