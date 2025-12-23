use std::{ffi::OsStr, fs::File, io, path::PathBuf, thread};

use clap::Parser;
use crossbeam::channel;
use fjall::{Database, KeyspaceCreateOptions, config::CompressionPolicy};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::pgn::{GameVisitor, PreparedGame};
use pgn_reader::Reader;
use rocksdb::{BlockBasedOptions, Cache, WriteBatchWithTransaction};
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

#[derive(Parser)]
struct Args {
    pgns: Vec<PathBuf>,
    #[clap(long, default_value = "_db")]
    db: PathBuf,
}

fn main() -> Result<(), io::Error> {
    let args = Args::parse();

    thread::scope(|s| {
        let (parsed_tx, parsed_rx) = channel::bounded(128);
        let (prepared_tx, prepared_rx) = channel::bounded(128);

        for _ in 0..1 {
            let parsed_rx = parsed_rx.clone();
            let prepared_tx = prepared_tx.clone();
            s.spawn(move || {
                for parsed in parsed_rx {
                    prepared_tx
                        .send(PreparedGame::prepare(parsed).expect("prepare game"))
                        .expect("send prepared game");
                }
            });
        }

        {
            let mut block_opts = BlockBasedOptions::default();
            block_opts.set_block_cache(&Cache::new_lru_cache(4 * 1024 * 1024 * 1024));

            let mut opts = rocksdb::Options::default();
            opts.create_if_missing(true);
            opts.create_missing_column_families(true);
            opts.set_block_based_table_factory(&block_opts);

            let db = rocksdb::DB::open(&opts, args.db).unwrap();
            let prepared_rx = prepared_rx.clone();
            s.spawn(move || {
                for prepared in prepared_rx {
                    let mut write_batch = WriteBatchWithTransaction::<false>::default();

                    for hash in prepared.hashes {
                        write_batch.put(
                            &u64::from(hash).to_be_bytes()[0..5],
                            prepared.game_id.to_bytes(),
                        );
                    }

                    db.write(write_batch).expect("write batch");
                }
            });
        }

        for arg in args.pgns {
            let file = File::open(&arg)?;
            let progress = ProgressBar::with_draw_target(
                Some(file.metadata()?.len()),
                ProgressDrawTarget::stdout_with_hz(4),
            )
            .with_style(
                ProgressStyle::with_template(
                    "{spinner} {prefix} {msg} {wide_bar} {bytes_per_sec:>14} {eta:>7}",
                )
                .expect("progress template"),
            )
            .with_prefix(format!("{arg:?}"));

            let file = progress.wrap_read(file);

            let uncompressed: Box<dyn io::Read> = if arg.extension() == Some(OsStr::new("zst")) {
                Box::new(zstd::Decoder::new(file)?)
            } else {
                Box::new(file)
            };

            let mut reader = Reader::new(uncompressed);

            for parsed in reader.read_games(&mut GameVisitor) {
                let parsed = parsed??;
                parsed_tx.send(parsed).expect("send parsed");
            }

            progress.finish();
        }
        Ok(())
    })
}
