use std::fs::OpenOptions;
use std::path::Path;
use std::{ffi::OsStr, fs::File, io, path::PathBuf, thread};

use clap::Parser;
use crossbeam::channel;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lila_openingexplorer4::packed::GameId;
use lila_openingexplorer4::pgn::{GameVisitor, PreparedGame, PreparedGameMeta};
use memmap2::{MmapMut, MmapOptions};
use pgn_reader::Reader;
use rocksdb::{
    BlockBasedOptions, Cache, DB, DataBlockIndexType, MemtableFactory, Options, SstFileWriter,
};
use rustc_hash::FxHashMap;
use shakmaty::packed::PackedUciMove;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOC: Jemalloc = Jemalloc;

struct Bitmap {
    mmap: MmapMut,
}

impl Bitmap {
    pub fn open(path: &Path) -> io::Result<Bitmap> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        file.set_len(0xff_ffff_ffff / 8)?;

        Ok(Bitmap {
            mmap: unsafe { MmapOptions::new().map_mut(&file)? },
        })
    }

    pub fn acquire(&mut self, index: [u8; 5]) -> bool {
        let index = u64::from_be_bytes([index[0], index[1], index[2], index[3], index[4], 0, 0, 0]);
        let byte_index = index / 8;
        let bit_index = index % 8;

        let byte = &mut self.mmap[byte_index as usize];
        let mask = 1 << bit_index;

        if *byte & mask == 0 {
            *byte |= mask;
            true
        } else {
            false
        }
    }
}

struct PositionEntry {
    zobrist: [u8; 16],
    m: PackedUciMove,
    game_id: GameId,
}

impl PositionEntry {
    fn short_zobrist(&self) -> [u8; 5] {
        let mut bytes = [0; 5];
        bytes.copy_from_slice(&self.zobrist[0..5]);
        bytes
    }
}

struct Indexer {
    games: FxHashMap<GameId, PreparedGameMeta>,
    positions: Vec<PositionEntry>,
    workspace: PathBuf,
    flushes: u64,
}

impl Indexer {
    pub fn new(workspace: PathBuf) -> Indexer {
        Indexer {
            games: FxHashMap::default(),
            positions: Vec::new(),
            workspace,
            flushes: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.positions.len()
    }

    pub fn push(&mut self, prepared: PreparedGame) {
        let game_id = prepared.meta.game_id;
        self.games.insert(game_id, prepared.meta);
        self.positions.extend(
            prepared
                .positions
                .into_iter()
                .map(|(pos, m)| PositionEntry {
                    zobrist: u128::from(pos).to_be_bytes(),
                    m,
                    game_id,
                }),
        );
    }

    pub fn flush(&mut self, bitmap: &mut Bitmap) {
        self.flushes += 1;

        self.positions.sort_by_key(|e| e.short_zobrist());

        let opts = Options::default();

        let mut writer = SstFileWriter::create(&opts);
        writer
            .open(self.workspace.join(format!("low_{}.sst", self.flushes)))
            .unwrap();

        let mut seen_hash = [0xff; 5];
        for position in &self.positions {
            if position.short_zobrist() != seen_hash {
                seen_hash = position.short_zobrist();

                if bitmap.acquire(seen_hash) {
                    writer.put(seen_hash, position.game_id.to_bytes()).unwrap();
                }
            }
        }

        writer.finish().unwrap();

        self.games.clear();
        self.positions.clear();
    }
}

#[derive(Parser)]
struct Args {
    pgns: Vec<PathBuf>,
    #[clap(long, default_value = "_db")]
    db: PathBuf,
    #[clap(long)]
    workspace: PathBuf,
    #[clap(long)]
    bitmap: PathBuf,
}

fn main() -> Result<(), io::Error> {
    let args = Args::parse();

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
    opts.set_block_based_table_factory(&table_opts);
    opts.set_allow_concurrent_memtable_write(false);
    opts.set_memtable_factory(MemtableFactory::HashLinkList {
        bucket_count: 128 * 1024 * 1024 / 5 / 10,
    });

    let db = DB::open(&opts, args.db).unwrap();

    let mut bitmap = Bitmap::open(&args.bitmap).unwrap();

    thread::scope(|s| {
        let (parsed_tx, parsed_rx) = channel::bounded(128);
        let (prepared_tx, prepared_rx) = channel::bounded(1000 * 1000);

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
                let mut indexer = Indexer::new(args.workspace.clone());

                for prepared in prepared_rx {
                    indexer.push(prepared);

                    if indexer.len() >= 200_000_000 {
                        indexer.flush(&mut bitmap);
                    }
                }

                indexer.flush(&mut bitmap);
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
