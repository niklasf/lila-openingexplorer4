use lila_openingexplorer4::indexing::IntermediateEntry;
use shakmaty::Outcome;
use std::{ffi::OsStr, fs::File, io, path::PathBuf, thread};

use clap::Parser;
use crossbeam::channel;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::indexing::{GameVisitor, IntermediateSink, PreparedGame};
use pgn_reader::Reader;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

#[derive(Parser)]
struct Args {
    pgns: Vec<PathBuf>,
    #[clap(long)]
    workspace: PathBuf,
}

fn main() -> Result<(), io::Error> {
    let args = Args::parse();

    thread::scope(|s| {
        let (parsed_tx, parsed_rx) = channel::bounded(128);
        //let (prepared_tx, prepared_rx) = channel::bounded(1000 * 1000);

        {
            let parsed_rx = parsed_rx.clone();
            //let prepared_tx = prepared_tx.clone();
            let mut sink = IntermediateSink::new(&args.workspace).expect("sink");
            s.spawn(move || {
                for parsed in parsed_rx {
                    let prepared = PreparedGame::prepare(parsed).expect("prepare game");
                    if let Outcome::Known(outcome) = prepared.meta.outcome {
                        for (hash, (turn, m)) in prepared.positions {
                            sink.write(&IntermediateEntry {
                                hash: hash.into(),
                                month: prepared.meta.date.earliest_month(),
                                m,
                                game_id: prepared.meta.game_id,
                                speed: prepared.meta.speed,
                                rating: prepared.meta.players[turn].rating,
                                outcome,
                            })
                            .expect("write intermediate entry")
                        }
                    }
                }
                sink.flush().expect("flush sink");
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
