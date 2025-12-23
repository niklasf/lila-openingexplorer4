use std::fs::{File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::os::unix::fs::FileExt;
use std::path::Path;

pub trait ByteArray {
    fn zeroed() -> Self;
    fn as_slice(&self) -> &[u8];
    fn as_mut_slice(&mut self) -> &mut [u8];
}

pub trait Config {
    type Key;
    type Value;
    type EncodedValue: ByteArray;

    const KEY_BITS: u32;
    const VALUE_SIZE: usize;

    fn encode_key(key: &Self::Key) -> u64;
    fn encode_value(value: &Self::Value) -> Self::EncodedValue;
    fn decode_value(bytes: &Self::EncodedValue) -> Self::Value;
}

struct UpdateBatch<C: Config> {
    writes: Vec<(C::Key, C::Value)>,
}

struct Spills<C: Config> {
    spills: Vec<(C::Key, C::Value)>,
}

struct RawTable<V> {
    file: File,
    key_mask: u64,
    _marker: PhantomData<V>,
}

impl<V: ByteArray> RawTable<V> {
    pub fn open(path: &Path, key_bits: u32) -> io::Result<Self> {
        let table = RawTable {
            file: OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(path)?,
            key_mask: (1 << key_bits) - 1,
            _marker: PhantomData,
        };
        table
            .file
            .set_len((table.key_mask + 1) * V::zeroed().as_slice().len() as u64)?;
        Ok(table)
    }

    pub fn read(&self, key: u64) -> io::Result<V> {
        let mut value = V::zeroed();
        let slice = value.as_mut_slice();
        self.file
            .read_exact_at(slice, (key & self.key_mask) * slice.len() as u64)?;
        Ok(value)
    }

    pub fn write(&self, key: u64, value: V) -> io::Result<()> {
        let slice = value.as_slice();
        self.file
            .write_all_at(slice, (key & self.key_mask) * slice.len() as u64)
    }
}
