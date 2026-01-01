use clap::Parser;
use lila_openingexplorer4::indexing::IntermediateEntry;
use std::cmp::min;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

#[derive(Parser)]
struct Args {
    intermediate: PathBuf,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let mut file = File::open(args.intermediate)?;
    let mut remaining_len = file.metadata()?.len();
    assert!(remaining_len.is_multiple_of(IntermediateEntry::SIZE as u64));

    let mut buffer = Vec::new();

    while remaining_len > 0 {
        buffer.resize(
            min(
                remaining_len,
                IntermediateEntry::SIZE as u64 * 1024 * 1024 * 256,
            ) as usize,
            0,
        );

        println!(
            "reading {} bytes, {} remaining ...",
            buffer.len(),
            remaining_len
        );
        file.read_exact(&mut buffer)?;
        remaining_len -= buffer.len() as u64;

        println!("sorting ...");
        let (chunks, tail) = buffer.as_chunks_mut::<{ IntermediateEntry::SIZE }>();
        assert!(tail.is_empty());
        chunks.sort_by_key(|chunk| {
            let key: [u8; IntermediateEntry::KEY_SIZE] =
                chunk[..IntermediateEntry::KEY_SIZE].try_into().unwrap();
            key
        });
    }

    Ok(())
}
