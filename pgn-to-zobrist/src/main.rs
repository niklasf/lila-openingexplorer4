use clap::Parser;
use crossbeam::channel;
use indicatif::ProgressBar;
use indicatif::ProgressDrawTarget;
use indicatif::ProgressStyle;
use pgn_reader::Reader;
use pgn_reader::Skip;
use pgn_reader::Visitor;
use shakmaty::Chess;
use shakmaty::EnPassantMode;
use shakmaty::Position;
use shakmaty::san::San;
use shakmaty::san::SanPlus;
use shakmaty::zobrist::Zobrist64;
use shakmaty::zobrist::ZobristHash;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::io::BufWriter;
use std::io::Write;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::thread;

struct Game {
    index: usize,
    moves: Vec<San>,
}

#[derive(Default)]
struct MakeGame {
    index: usize,
}

impl Visitor for MakeGame {
    type Tags = ();
    type Movetext = Game;
    type Output = Game;

    fn begin_tags(&mut self) -> ControlFlow<Self::Output, Self::Tags> {
        ControlFlow::Continue(())
    }

    fn begin_movetext(&mut self, _tags: Self::Tags) -> ControlFlow<Self::Output, Self::Movetext> {
        let game = Game {
            index: self.index,
            moves: Vec::with_capacity(80),
        };
        self.index += 1;
        ControlFlow::Continue(game)
    }

    fn begin_variation(
        &mut self,
        _movetext: &mut Self::Movetext,
    ) -> ControlFlow<Self::Output, Skip> {
        ControlFlow::Continue(Skip(true)) // Stay in the mainline
    }

    fn san(
        &mut self,
        movetext: &mut Self::Movetext,
        san_plus: SanPlus,
    ) -> ControlFlow<Self::Output> {
        movetext.moves.push(san_plus.san);
        ControlFlow::Continue(())
    }

    fn end_game(&mut self, movetext: Self::Movetext) -> Self::Output {
        movetext
    }
}

#[derive(Parser)]
struct Args {
    #[clap(long)]
    output: PathBuf,
    pgns: Vec<PathBuf>,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let mut output = BufWriter::new(File::create_new(args.output)?);

    thread::scope(|s| {
        let (game_sender, game_receiver) = channel::bounded::<Game>(128);

        let (write_sender, write_receiver) = channel::bounded::<Vec<u8>>(128);

        s.spawn(move || {
            for buffer in write_receiver {
                if let Err(err) = output.write_all(&buffer) {
                    eprintln!("failed to write hashes: {}", err);
                }
            }
        });

        for _ in 0..4 {
            let game_receiver = game_receiver.clone();
            let write_sender = write_sender.clone();
            s.spawn(move || {
                for game in game_receiver {
                    let mut buffer = Vec::with_capacity(8 * game.moves.len());
                    let mut pos = Chess::new();
                    for san in game.moves {
                        let m = match san.to_move(&pos) {
                            Ok(m) => m,
                            Err(err) => {
                                eprintln!("in game {} at move {}: {}", game.index, san, err);
                                break;
                            }
                        };

                        buffer.extend_from_slice(
                            &u64::from(pos.zobrist_hash::<Zobrist64>(EnPassantMode::Legal))
                                .to_be_bytes(),
                        );

                        pos.play_unchecked(m);
                    }
                    write_sender.send(buffer).expect("send hashes");
                }
            });
        }

        for pgn in args.pgns {
            let file = File::open(&pgn)?;
            let progress = ProgressBar::with_draw_target(
                Some(file.metadata()?.len()),
                ProgressDrawTarget::stdout_with_hz(4),
            )
            .with_style(
                ProgressStyle::with_template(
                    "{spinner} {prefix} {wide_bar} {bytes_per_sec:>14} {eta:>7}",
                )
                .expect("progress template"),
            )
            .with_prefix(format!("{pgn:?}"));

            let file = progress.wrap_read(file);

            let uncompressed: Box<dyn io::Read> = if pgn.extension() == Some(OsStr::new("zst")) {
                Box::new(zstd::Decoder::new(file)?)
            } else {
                Box::new(file)
            };

            for game in Reader::new(uncompressed).read_games(&mut MakeGame::default()) {
                let game = game?;
                game_sender.send(game).expect("send game");
            }

            progress.finish();
        }

        Ok(())
    })
}
