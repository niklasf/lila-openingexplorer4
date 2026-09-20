use std::{ffi::OsStr, fs::File, io, path::PathBuf};

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use lila_openingexplorer4::indexing::GameVisitor;
use pgn_reader::Reader;

#[derive(Parser)]
struct Args {
    pgns: Vec<PathBuf>,
    #[clap(long)]
    out: PathBuf,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    for arg in args.pgns {
        let file = File::open(&arg)?;
        let progress = ProgressBar::new(file.metadata()?.len())
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
            let _parsed = parsed??;
        }

        progress.finish();
    }

    Ok(())
}
