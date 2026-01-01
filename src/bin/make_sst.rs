use std::{
    cmp::min,
    fs::File,
    io::{self, Read},
    path::PathBuf,
};

use clap::Parser;
use lila_openingexplorer4::indexing::{Bitmap, IntermediateEntry};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

#[derive(Parser)]
struct Args {
    intermediate: PathBuf,
    #[clap(long)]
    workspace: PathBuf,
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

        println!("checking first entry ...");
        let first_entry = IntermediateEntry::unpack(&mut &chunks[0][..]);
        println!(
            "{:?} from partition {:04}",
            first_entry,
            first_entry.hash.partition()
        );

        println!("loading bitmap ...");
        let mut bitmap = Bitmap::load(&args.workspace, first_entry.partition())?;

        println!("reading entries ...");
        let mut success: u64 = 0;
        let mut fail: u64 = 0;
        for chunk in chunks {
            let entry = IntermediateEntry::unpack(&mut &chunk[..]);
            assert_eq!(first_entry.partition(), entry.partition());

            if bitmap.acquire(entry.hash) {
                success += 1;
            } else {
                fail += 1;
            }
        }
        println!(
            "comitting bitmap (acquired {} entries ({:.2}%), failed to acquire {} entries) ...",
            success,
            success as f64 / (success + fail) as f64 * 100.0,
            fail
        );
        bitmap.commit()?;
    }

    Ok(())
}
