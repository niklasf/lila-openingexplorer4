//! Compressed chunks of records, padded for `O_DIRECT`.

/// Alignment of buffers, offsets and lengths for `O_DIRECT` reads and
/// writes.
pub const DIRECT_ALIGN: usize = 4096;

/// A zero filled buffer aligned to `DIRECT_ALIGN`.
pub struct AlignedBuf {
    vec: Vec<u8>,
    offset: usize,
}

impl Default for AlignedBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl AlignedBuf {
    pub fn new() -> AlignedBuf {
        AlignedBuf {
            vec: Vec::new(),
            offset: 0,
        }
    }

    /// Resizes to `len` bytes (rounded up to the alignment) and returns
    /// them, zero filled.
    pub fn resize(&mut self, len: usize) -> &mut [u8] {
        let len = len.next_multiple_of(DIRECT_ALIGN);
        if self.vec.capacity() < len + DIRECT_ALIGN {
            self.vec = Vec::with_capacity(len + DIRECT_ALIGN);
        }
        self.offset = self.vec.as_ptr().align_offset(DIRECT_ALIGN);
        self.vec.clear();
        self.vec.resize(self.offset + len, 0);
        &mut self.vec[self.offset..]
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.vec[self.offset..]
    }

    /// The aligned bytes, and the offset of the first one in the underlying
    /// vector.
    pub fn into_parts(self) -> (Vec<u8>, usize) {
        (self.vec, self.offset)
    }
}

/// A compressed chunk: `len` bytes, followed by zero padding to
/// `DIRECT_ALIGN`.
pub struct Compressed<'a> {
    pub len: usize,
    pub padded: &'a [u8],
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

pub struct ChunkDecompressor {
    /// `None` for no compression.
    zstd: Option<zstd::bulk::Decompressor<'static>>,
}

impl ChunkDecompressor {
    pub fn new(zstd_level: i32) -> ChunkDecompressor {
        ChunkDecompressor {
            zstd: (zstd_level != 0)
                .then(|| zstd::bulk::Decompressor::new().expect("zstd decompressor")),
        }
    }

    /// Decompresses into `dst`, which must have exactly the raw length.
    pub fn decompress(&mut self, compressed: &[u8], dst: &mut [u8]) {
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
