use std::{ffi::OsStr, fs::File, io, path::PathBuf, thread};

use clap::Parser;
use crossbeam::channel;
use fjall::{Database, KeyspaceCreateOptions, config::CompressionPolicy};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::pgn::{GameVisitor, PreparedGame};
use pgn_reader::Reader;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

#[derive(Parser)]
struct Args {
    pgns: Vec<PathBuf>,
    #[clap(long, default_value = "_db")]
    db: String,
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

        for i in 0..2 {
            let path = format!("{}_{}", &args.db, i);
            let prepared_rx = prepared_rx.clone();
            s.spawn(move || {
                let db = Database::builder(path)
                    .cache_size(1024 * 1024 * 1024)
                    .open()
                    .expect("open db");

                let index_partition = db
                    .keyspace("lichess_index", KeyspaceCreateOptions::default)
                    .expect("open keyspace");

                for prepared in prepared_rx {
                    for hash in prepared.hashes {
                        index_partition
                            .insert(
                                &u64::from(hash).to_be_bytes()[0..5],
                                prepared.game_id.to_bytes(),
                            )
                            .expect("insert hash")
                    }
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
