//! Scratch files holding compressed chunks of records.

use std::{
    fs::{self, File},
    io,
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::PathBuf,
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

use crate::scatter::chunk::{AlignedBuf, Compressed, DIRECT_ALIGN};

/// A counter that threads can wait on.
#[derive(Default)]
pub struct Gauge {
    value: Mutex<u64>,
    changed: Condvar,
}

impl Gauge {
    pub fn add(&self, n: u64) {
        *self.value.lock().expect("lock gauge") += n;
        self.changed.notify_all();
    }

    pub fn sub(&self, n: u64) {
        *self.value.lock().expect("lock gauge") -= n;
        self.changed.notify_all();
    }

    pub fn get(&self) -> u64 {
        *self.value.lock().expect("lock gauge")
    }

    /// Blocks until the value satisfies `ready`, and returns the time
    /// waited.
    pub fn wait_until(&self, ready: impl Fn(u64) -> bool) -> Duration {
        let timer = Instant::now();
        let mut value = self.value.lock().expect("lock gauge");
        while !ready(*value) {
            value = self.changed.wait(value).expect("wait gauge");
        }
        timer.elapsed()
    }
}

#[derive(Copy, Clone)]
pub struct ChunkMeta {
    pub offset: u64,
    pub raw_len: u32,
    pub compressed_len: u32,
}

/// A scratch file holding compressed chunks of records.
pub struct ScratchFile {
    path: PathBuf,
    /// Buffered handle for reading.
    file: File,
    /// Handle for writing, and for reading back if `direct`: then with
    /// `O_DIRECT`, and chunks padded to `DIRECT_ALIGN`.
    write_file: File,
    direct: bool,
    zstd_level: i32,
    len: u64,
    chunks: Vec<ChunkMeta>,
}

impl ScratchFile {
    pub fn create(path: PathBuf, zstd_level: i32, direct: bool) -> io::Result<ScratchFile> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        let write_file = if direct {
            File::options()
                .read(true)
                .write(true)
                .custom_flags(libc::O_DIRECT)
                .open(&path)?
        } else {
            file.try_clone()?
        };
        Ok(ScratchFile {
            path,
            file,
            write_file,
            direct,
            zstd_level,
            len: 0,
            chunks: Vec::new(),
        })
    }

    pub fn zstd_level(&self) -> i32 {
        self.zstd_level
    }

    /// Bytes written so far.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn chunks(&self) -> &[ChunkMeta] {
        &self.chunks
    }

    /// Raw bytes of all chunks.
    pub fn raw_len(&self) -> u64 {
        self.chunks.iter().map(|c| u64::from(c.raw_len)).sum()
    }

    pub fn append(
        &mut self,
        raw_len: usize,
        compressed: Compressed<'_>,
        usage: &Gauge,
    ) -> io::Result<()> {
        let bytes = if self.direct {
            compressed.padded
        } else {
            &compressed.padded[..compressed.len]
        };
        self.write_file.write_all_at(bytes, self.len)?;
        self.chunks.push(ChunkMeta {
            offset: self.len,
            raw_len: raw_len as u32,
            compressed_len: compressed.len as u32,
        });
        self.len += bytes.len() as u64;
        usage.add(bytes.len() as u64);
        Ok(())
    }

    /// Reads the compressed bytes of chunk `i` into `buf`.
    pub fn read_chunk<'a>(&self, i: usize, buf: &'a mut AlignedBuf) -> io::Result<&'a [u8]> {
        let chunk = self.chunks[i];
        let len = chunk.compressed_len as usize;
        let dst = buf.resize(len);
        if self.direct {
            self.write_file.read_exact_at(dst, chunk.offset)?;
        } else {
            self.file.read_exact_at(&mut dst[..len], chunk.offset)?;
        }
        Ok(&dst[..len])
    }

    pub fn remove(self, usage: &Gauge) -> io::Result<()> {
        drop(self.file);
        drop(self.write_file);
        fs::remove_file(&self.path)?;
        usage.sub(self.len);
        Ok(())
    }
}

// Chunks are padded to the alignment, so compressed lengths fit.
const _: () = assert!(DIRECT_ALIGN.is_power_of_two());
