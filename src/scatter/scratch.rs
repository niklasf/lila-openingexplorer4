use std::{
    fs::{self, File},
    io,
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use crate::util::{AlignedBuf, DIRECT_ALIGN, Gauge};

/// A compressed chunk: `len` bytes, followed by zero padding to
/// `DIRECT_ALIGN`.
pub struct Compressed<'a> {
    len: usize,
    padded: &'a [u8],
}

impl Compressed<'_> {
    /// Bytes occupied in a scratch file opened with `O_DIRECT`.
    pub fn padded_len(&self) -> usize {
        self.padded.len()
    }
}

pub struct ChunkCompressor {
    /// `None` for no compression.
    zstd: Option<zstd::bulk::Compressor<'static>>,
    buf: AlignedBuf,
}

impl ChunkCompressor {
    pub fn new(zstd_level: i32) -> ChunkCompressor {
        ChunkCompressor {
            zstd: (zstd_level != 0)
                .then(|| zstd::bulk::Compressor::new(zstd_level).expect("zstd compressor")),
            buf: AlignedBuf::new(),
        }
    }

    pub fn compress(&mut self, raw: &[u8]) -> Compressed<'_> {
        let len = match &mut self.zstd {
            None => {
                self.buf.resize(raw.len())[..raw.len()].copy_from_slice(raw);
                raw.len()
            }
            Some(zstd) => {
                let dst = self.buf.resize(zstd::zstd_safe::compress_bound(raw.len()));
                zstd.compress_to_buffer(raw, dst).expect("compress")
            }
        };
        // The padding is zero filled by `resize`.
        let padded = &self.buf.as_slice()[..len.next_multiple_of(DIRECT_ALIGN)];
        Compressed { len, padded }
    }
}

struct ChunkDecompressor {
    /// `None` for no compression.
    zstd: Option<zstd::bulk::Decompressor<'static>>,
}

impl ChunkDecompressor {
    fn new(zstd_level: i32) -> ChunkDecompressor {
        ChunkDecompressor {
            zstd: (zstd_level != 0)
                .then(|| zstd::bulk::Decompressor::new().expect("zstd decompressor")),
        }
    }

    fn decompress(&mut self, compressed: &[u8], dst: &mut [u8]) {
        let n = match &mut self.zstd {
            None => {
                dst.copy_from_slice(compressed);
                compressed.len()
            }
            Some(zstd) => zstd
                .decompress_to_buffer(compressed, dst)
                .expect("decompress"),
        };
        assert_eq!(n, dst.len(), "chunk length");
    }
}

#[derive(Copy, Clone)]
struct ChunkMeta {
    offset: u64,
    raw_len: u32,
    compressed_len: u32,
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

    /// Bytes occupied on disk.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Uncompressed bytes of all chunks.
    pub fn raw_len(&self) -> u64 {
        self.chunks
            .iter()
            .map(|chunk| u64::from(chunk.raw_len))
            .sum()
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

    /// Loads and decompresses all chunks in parallel. Each thread starts
    /// with a state from `init` and calls `f` with the index and the raw
    /// bytes of each chunk it loads (which `f` may take). Returns the states
    /// of all threads.
    pub fn load<S: Send>(
        &self,
        threads: usize,
        init: impl Fn() -> S + Sync,
        f: impl Fn(&mut S, usize, &mut Vec<u8>) + Sync,
    ) -> Vec<S> {
        let next = AtomicUsize::new(0);
        let states = Mutex::new(Vec::with_capacity(threads));

        thread::scope(|s| {
            for _ in 0..threads.max(1) {
                s.spawn(|| {
                    let mut state = init();
                    let mut decompressor = ChunkDecompressor::new(self.zstd_level);
                    let mut compressed = AlignedBuf::new();
                    let mut raw = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(chunk) = self.chunks.get(i) else {
                            break;
                        };
                        let len = chunk.compressed_len as usize;
                        let dst = compressed.resize(len);
                        if self.direct {
                            self.write_file
                                .read_exact_at(dst, chunk.offset)
                                .expect("read chunk");
                        } else {
                            self.file
                                .read_exact_at(&mut dst[..len], chunk.offset)
                                .expect("read chunk");
                        }
                        raw.clear();
                        raw.resize(chunk.raw_len as usize, 0);
                        decompressor.decompress(&dst[..len], &mut raw);
                        f(&mut state, i, &mut raw);
                    }
                    states.lock().expect("lock states").push(state);
                });
            }
        });

        states.into_inner().expect("states")
    }

    pub fn remove(self, usage: &Gauge) -> io::Result<()> {
        drop(self.file);
        drop(self.write_file);
        fs::remove_file(&self.path)?;
        usage.sub(self.len);
        Ok(())
    }
}
