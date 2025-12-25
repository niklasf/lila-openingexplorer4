use std::iter;
use std::{ffi::OsStr, fs::File, io, path::PathBuf, thread};

use clap::Parser;
use crossbeam::channel;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::pgn::{GameVisitor, PreparedGame};
use pgn_reader::Reader;
use rocksdb::{
    BlockBasedOptions, Cache, DB, DataBlockIndexType, Options, WriteBatchWithTransaction,
};
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
            let prepared_rx = prepared_rx.clone();
            s.spawn(move || {
                let cache = Cache::new_lru_cache(8 * 1024 * 1024 * 1024);

                let mut table_opts = BlockBasedOptions::default();
                table_opts.set_block_cache(&cache);
                table_opts.set_block_size(16 * 1024);
                table_opts.set_cache_index_and_filter_blocks(true);
                table_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
                table_opts.set_hybrid_ribbon_filter(10.0, 1);
                table_opts.set_whole_key_filtering(true);
                table_opts.set_format_version(7);
                table_opts.set_optimize_filters_for_memory(true);
                table_opts.set_data_block_index_type(DataBlockIndexType::BinaryAndHash);

                let mut opts = Options::default();
                opts.create_if_missing(true);
                opts.create_missing_column_families(true);
                opts.set_compaction_readahead_size(2 * 1024 * 1024);
                opts.set_max_background_jobs(2);
                opts.optimize_level_style_compaction(512 * 1024 * 1024);
                opts.set_track_and_verify_wals_in_manifest(true);
                opts.set_memtable_prefix_bloom_ratio(0.02);
                opts.set_memtable_whole_key_filtering(true);

                opts.set_use_direct_io_for_flush_and_compaction(true);
                opts.set_level_compaction_dynamic_level_bytes(false);

                let db = DB::open(&opts, args.db).unwrap();
                for prepared in prepared_rx {
                    let existing_positions =
                        db.multi_get(prepared.positions.iter().map(|p| p.short_key()));

                    let mut write_batch = WriteBatchWithTransaction::<false>::default();

                    for (pos, existing) in iter::zip(prepared.positions, existing_positions) {
                        let existing = existing.expect("multi get");
                        if existing.is_none() {
                            write_batch.put(pos.short_key(), prepared.game_id.to_bytes());
                        }
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
