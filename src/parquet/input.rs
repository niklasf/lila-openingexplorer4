//! The row groups of a set of parquet files, with prefetching for scans.

use std::{
    fs::File,
    io,
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};

use crossbeam::channel;

use crate::{
    parquet::{GameBatch, GameReader, PositionalFile, RowGroupBytes},
    scatter::{AlignedBuf, DIRECT_ALIGN, timed},
};

/// The row groups of all input files, in order.
pub struct Input {
    readers: Vec<GameReader<PositionalFile>>,
    /// `O_DIRECT` handles of the input files, for fetching row groups
    /// without the page cache.
    direct_files: Option<Vec<File>>,
    /// Pairs of reader index and row group index within that reader.
    row_groups: Vec<(usize, usize)>,
}

impl Input {
    pub fn open(paths: &[PathBuf], limit: Option<usize>, direct: bool) -> io::Result<Input> {
        let readers = paths
            .iter()
            .map(|path| Ok(GameReader::try_new(PositionalFile::open(path)?)?))
            .collect::<io::Result<Vec<_>>>()?;
        let direct_files = direct
            .then(|| {
                paths
                    .iter()
                    .map(|path| {
                        File::options()
                            .read(true)
                            .custom_flags(libc::O_DIRECT)
                            .open(path)
                    })
                    .collect::<io::Result<Vec<_>>>()
            })
            .transpose()?;
        let mut row_groups: Vec<(usize, usize)> = readers
            .iter()
            .enumerate()
            .flat_map(|(r, reader)| (0..reader.num_row_groups()).map(move |i| (r, i)))
            .collect();
        if let Some(limit) = limit {
            row_groups.truncate(limit);
        }
        Ok(Input {
            readers,
            direct_files,
            row_groups,
        })
    }

    pub fn num_row_groups(&self) -> usize {
        self.row_groups.len()
    }

    pub fn num_rows(&self, row_group: usize) -> u64 {
        let (r, i) = self.row_groups[row_group];
        self.readers[r].metadata().row_group(i).num_rows() as u64
    }

    /// Rows of all row groups.
    pub fn total_rows(&self) -> u64 {
        (0..self.num_row_groups()).map(|i| self.num_rows(i)).sum()
    }

    pub fn read_games(&self, row_group: usize) -> parquet::errors::Result<GameBatch> {
        let (r, i) = self.row_groups[row_group];
        self.readers[r].read_games(i)
    }

    pub fn fetch(&self, row_group: usize) -> parquet::errors::Result<RowGroupBytes> {
        let (r, i) = self.row_groups[row_group];
        let Some(direct_files) = &self.direct_files else {
            return self.readers[r].fetch_row_group(i);
        };
        // One aligned read covering the row group.
        let reader = &self.readers[r];
        let (start, end) = reader.row_group_byte_range(i);
        let file_len = parquet::file::reader::Length::len(reader.input());
        let aligned_start = start / DIRECT_ALIGN as u64 * DIRECT_ALIGN as u64;
        let aligned_end = end.next_multiple_of(DIRECT_ALIGN as u64).min(file_len);
        let mut buf = AlignedBuf::new();
        let dst = buf.resize((aligned_end - aligned_start) as usize);
        // Reads past the end of the file are short, so read until the row
        // group is covered rather than until the aligned length is.
        let needed = (end - aligned_start) as usize;
        let mut filled = 0;
        while filled < needed {
            match direct_files[r].read_at(&mut dst[filled..], aligned_start + filled as u64)? {
                0 => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
                n => filled += n,
            }
        }
        let (vec, offset) = buf.into_parts();
        let bytes = bytes::Bytes::from(vec).slice(
            offset + (start - aligned_start) as usize..offset + (end - aligned_start) as usize,
        );
        Ok(RowGroupBytes::new(start, file_len, bytes))
    }

    pub fn read_games_from(
        &self,
        row_group: usize,
        bytes: RowGroupBytes,
    ) -> parquet::errors::Result<GameBatch> {
        let (r, i) = self.row_groups[row_group];
        self.readers[r].read_games_from(i, bytes)
    }
}

/// Accumulated time of the prefetch stages, summed over their threads.
#[derive(Default)]
pub struct PrefetchTimes {
    pub fetch: AtomicU64,
    pub send: AtomicU64,
}

/// Hands out row groups to decoding threads, prefetching their bytes with
/// `io_depth` concurrent reads. Cloned into each decoding thread.
#[derive(Clone)]
pub struct Prefetcher<'a> {
    input: &'a Input,
    /// Fetched row groups, in flight to the decoding threads.
    rx: channel::Receiver<(usize, RowGroupBytes)>,
}

impl<'a> Prefetcher<'a> {
    /// Spawns `io_depth` prefetch threads in the given scope. They finish
    /// once the input is exhausted or all decoding threads are done.
    pub fn spawn<'s>(
        input: &'a Input,
        io_depth: usize,
        times: &'a PrefetchTimes,
        s: &'s thread::Scope<'s, '_>,
    ) -> Prefetcher<'a>
    where
        'a: 's,
    {
        let next_row_group = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = channel::bounded(io_depth);
        for _ in 0..io_depth {
            let tx = tx.clone();
            let next_row_group = Arc::clone(&next_row_group);
            s.spawn(move || {
                loop {
                    let i = next_row_group.fetch_add(1, Ordering::Relaxed);
                    if i >= input.num_row_groups() {
                        break;
                    }
                    let bytes = timed(&times.fetch, || input.fetch(i)).expect("fetch row group");
                    if timed(&times.send, || tx.send((i, bytes))).is_err() {
                        break;
                    }
                }
            });
        }
        Prefetcher { input, rx }
    }

    /// The next row group to decode, or `None` at the end of the input.
    pub fn recv(&self) -> Option<(usize, RowGroupBytes)> {
        self.rx.recv().ok()
    }

    pub fn decode(&self, (i, bytes): (usize, RowGroupBytes)) -> parquet::errors::Result<GameBatch> {
        self.input.read_games_from(i, bytes)
    }
}
