use std::{
    ffi::OsStr,
    fs::File,
    io::{self, BufWriter},
    path::PathBuf,
    sync::Arc,
    thread,
};

use clap::Parser;
use crossbeam::channel;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::{
    indexing::{FlatGame, GameVisitor, ParsedGame, RecentGameIds},
    parquet::GameColumns,
};
use parquet::file::writer::SerializedFileWriter;
use pgn_reader::Reader;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

const ROW_GROUP_LEN: usize = 100_000;

const FLATTEN_THREADS: usize = 2; // Enough to keep up with parsed games

const CHANNEL_LEN: usize = 32768; // Enough to buffer while previous row group is being written

#[derive(Parser)]
struct Args {
    pgns: Vec<PathBuf>,
    #[clap(long)]
    out: PathBuf,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    thread::scope(|s| {
        // Flatten in round robin, so that games can be collected in the
        // original order.
        let mut parsed_txs = Vec::with_capacity(FLATTEN_THREADS);
        let mut flat_rxs = Vec::with_capacity(FLATTEN_THREADS);

        for _ in 0..FLATTEN_THREADS {
            let (parsed_tx, parsed_rx) = channel::bounded::<ParsedGame>(CHANNEL_LEN);
            let (flat_tx, flat_rx) = channel::bounded::<io::Result<FlatGame>>(CHANNEL_LEN);
            parsed_txs.push(parsed_tx);
            flat_rxs.push(flat_rx);

            s.spawn(move || {
                for parsed in parsed_rx {
                    flat_tx
                        .send(FlatGame::flatten(parsed))
                        .expect("send flat game");
                }
            });
        }

        s.spawn(move || {
            let file = File::create(&args.out).expect("create parquet file");
            let mut writer = SerializedFileWriter::new(
                file,
                GameColumns::schema(),
                Arc::new(GameColumns::properties()),
            )
            .expect("parquet writer");

            let mut columns = GameColumns::default();

            'collect: loop {
                for flat_rx in &flat_rxs {
                    match flat_rx.recv() {
                        Ok(Ok(flat)) => {
                            columns.push(flat);
                            if columns.len() >= ROW_GROUP_LEN {
                                columns
                                    .write_row_group(&mut writer)
                                    .expect("write row group");
                                columns.clear();
                            }
                        }
                        Ok(Err(err)) => eprintln!("skipping game: {}", err),
                        Err(channel::RecvError) => break 'collect,
                    }
                }
            }

            columns
                .write_row_group(&mut writer)
                .expect("write final row group");
            columns.clear();
            writer.close().expect("close parquet file");
        });

        let mut next_thread = 0;

        // Consecutive database files can overlap, so that some games appear
        // twice.
        let mut recent_game_ids = RecentGameIds::new();

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
                let parsed = match parsed? {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        eprintln!("skipping game: {}", err);
                        continue;
                    }
                };

                if parsed
                    .game_id()
                    .is_some_and(|game_id| !recent_game_ids.insert(game_id))
                {
                    continue;
                }

                parsed_txs[next_thread].send(parsed).expect("send parsed");
                next_thread = (next_thread + 1) % FLATTEN_THREADS;
            }

            progress.finish();
        }

        if recent_game_ids.duplicates() > 0 {
            eprintln!("skipped {} duplicate games", recent_game_ids.duplicates());
        }

        Ok(())
    })
}
