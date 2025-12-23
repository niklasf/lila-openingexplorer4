use std::{
    cmp::Reverse,
    cmp::min,
    collections::HashMap,
    fs::{File, OpenOptions},
    io,
    path::PathBuf,
};

use clap::Parser;
use itertools::Itertools;
use memmap2::{Mmap, MmapMut};

#[derive(Parser)]
struct Args {
    zobrist: PathBuf,
    #[clap(long)]
    sort: bool,
    #[clap(long)]
    chunk_bytes: Option<usize>,
    #[clap(long)]
    take_chunks: Option<usize>,
}

impl Args {
    pub fn chunk_bytes(&self) -> usize {
        self.chunk_bytes.unwrap_or(128 * 1024 * 1024 * 1024)
    }

    pub fn take_chunks(&self) -> usize {
        self.take_chunks.unwrap_or(usize::MAX)
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let file = OpenOptions::new()
        .read(true)
        .write(args.sort)
        .create(false)
        .open(&args.zobrist)?;

    if args.sort {
        do_sort(&file, &args)?;
    }

    let mmap = unsafe { Mmap::map(&file)? };

    let mut positions_by_occurences = HashMap::new();

    for (i, (occurences, _hash)) in mmap
        .chunks(args.chunk_bytes())
        .take(args.take_chunks())
        .map(|chunk| {
            let (hashes, tail) = chunk.as_chunks::<8>();
            assert!(tail.is_empty(), "expected multiple of 8 bytes");
            hashes
        })
        .kmerge()
        .dedup_with_count()
        .enumerate()
    {
        if i.is_multiple_of(1024 * 1024 * 1024) {
            println!("Processed {} runs", i);
        }

        *positions_by_occurences.entry(occurences).or_insert(0) += 1;
    }

    println!();

    let mut positions_by_occurences = positions_by_occurences.into_iter().collect::<Vec<_>>();
    positions_by_occurences
        .sort_by_key(|&(occurences, positions)| (positions, Reverse(occurences)));

    let distinct_positions = positions_by_occurences
        .iter()
        .map(|&(_, positions)| positions)
        .sum::<usize>();

    for &(occurences, positions) in &positions_by_occurences {
        println!(
            "{} occurences: {} positions ({:.3}% of distinct)",
            occurences,
            positions,
            positions as f64 / distinct_positions as f64 * 100.0
        );
    }

    println!();

    let total_positions = min(
        mmap.len(),
        args.take_chunks().saturating_mul(args.chunk_bytes()),
    ) / 8;
    println!(
        "{} repeated positions",
        total_positions - distinct_positions
    );
    println!("{} distinct positions", distinct_positions);
    println!("{} total positions", total_positions);

    Ok(())
}

fn do_sort(file: &File, args: &Args) -> io::Result<()> {
    let mut mmap = unsafe { MmapMut::map_mut(file)? };

    let chunks = mmap.chunks_mut(args.chunk_bytes());
    let len = chunks.len();

    for (i, chunk) in chunks.take(args.take_chunks()).enumerate() {
        println!("Sorting chunk {}/{} ...", i + 1, len);
        let (hashes, tail) = chunk.as_chunks_mut::<8>();
        assert!(tail.is_empty(), "expected multiple of 8 bytes");
        hashes.sort_unstable();
    }

    Ok(())
}
