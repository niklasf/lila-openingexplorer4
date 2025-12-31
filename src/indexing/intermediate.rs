use std::fs::File;
use std::io;
use std::io::BufWriter;
use std::path::Path;

use shakmaty::KnownOutcome;
use shakmaty::zobrist::Zobrist128;

use crate::packed::GameId;
use crate::packed::Month;
use crate::packed::RatingGroup;
use crate::packed::Speed;
use shakmaty::packed::PackedUciMove;

pub struct IntermediateSink {
    partitions: Vec<BufWriter<File>>,
}

impl IntermediateSink {
    pub fn new(workspace: &Path) -> io::Result<Self> {
        Ok(IntermediateSink {
            partitions: (0..1024)
                .map(|i| {
                    File::create(workspace.join(format!("partition_{}", i))).map(BufWriter::new)
                })
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn write(
        &mut self,
        hash: Zobrist128,
        month: Month,
        m: PackedUciMove,
        game: GameId,
        speed: Speed,
        rating_group: RatingGroup,
        outcome: KnownOutcome,
    ) {
    }
}
