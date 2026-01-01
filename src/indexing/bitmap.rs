use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use crate::indexing::intermediate::Zobrist96;

pub struct Bitmap {
    buffer: Vec<u8>,
    workspace: PathBuf,
    partition: usize,
}

impl Bitmap {
    fn path(&self) -> PathBuf {
        self.workspace
            .join(format!("bitmap_{:04}.bin", self.partition))
    }

    fn temp_path(&self) -> PathBuf {
        self.workspace
            .join(format!("bitmap_{:04}_wip.bin", self.partition))
    }

    pub fn load(workspace: &Path, partition: usize) -> io::Result<Self> {
        let mut bitmap = Bitmap {
            buffer: Vec::new(),
            workspace: workspace.to_owned(),
            partition,
        };

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(bitmap.path())?;

        file.read_to_end(&mut bitmap.buffer)?;

        bitmap.buffer.resize(1 << (42 - 10 - 3), 0);

        Ok(bitmap)
    }

    pub fn commit(&self) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(self.temp_path())?;

        file.write_all(&self.buffer)?;

        fs::rename(self.temp_path(), self.path())
    }

    pub fn acquire(&mut self, hash: Zobrist96) -> bool {
        let index = hash.bitmap_index();

        let byte_index = index / 8;
        let bit_index = index % 8;

        let byte = &mut self.buffer[byte_index];
        let bit = 1 << bit_index;

        if *byte & bit == 0 {
            *byte |= bit;
            true
        } else {
            false
        }
    }
}
