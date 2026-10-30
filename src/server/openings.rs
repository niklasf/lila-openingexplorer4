use std::{fs, io, path::Path, time::Duration};

use nohash_hasher::IntMap;
use serde::Serialize;
use shakmaty::{
    Chess, EnPassantMode, Position,
    san::San,
    variant::{Variant, VariantPosition},
    zobrist::Zobrist64,
};
use thiserror::Error;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Opening {
    pub eco: String,
    pub name: String,
}

#[derive(Debug, Error)]
pub enum OpeningsError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("download: {0}")]
    Download(#[from] reqwest::Error),
    #[error("invalid tsv line {line}: {reason}")]
    Tsv { line: usize, reason: String },
    #[error("duplicate opening position for {0}")]
    Duplicate(String),
}

/// Names of the files of <https://github.com/lichess-org/chess-openings>.
pub const TSV_FILES: [&str; 5] = ["a.tsv", "b.tsv", "c.tsv", "d.tsv", "e.tsv"];

/// Opening names by position (standard chess only).
#[derive(Default)]
pub struct Openings {
    data: IntMap<Zobrist64, Opening>,
}

impl Openings {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Loads `a.tsv` to `e.tsv` from a directory.
    pub fn load_dir(dir: &Path) -> Result<Openings, OpeningsError> {
        let mut openings = Openings::default();
        for file in TSV_FILES {
            openings.load_tsv(&fs::read_to_string(dir.join(file))?)?;
        }
        Ok(openings)
    }

    /// Downloads the current opening names from GitHub.
    pub async fn download() -> Result<Openings, OpeningsError> {
        let client = reqwest::Client::builder()
            .user_agent("lila-openingexplorer4")
            .timeout(Duration::from_secs(60))
            .build()?;
        let mut openings = Openings::default();
        for file in TSV_FILES {
            let tsv = client
                .get(format!(
                    "https://raw.githubusercontent.com/lichess-org/chess-openings/master/{file}"
                ))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            openings.load_tsv(&tsv)?;
        }
        Ok(openings)
    }

    /// Loads a tab separated file with the columns eco, name and pgn.
    pub fn load_tsv(&mut self, tsv: &str) -> Result<(), OpeningsError> {
        for (i, line) in tsv.lines().enumerate().skip(1) {
            if line.trim().is_empty() {
                continue;
            }
            let mut fields = line.split('\t');
            let (Some(eco), Some(name), Some(pgn)) = (fields.next(), fields.next(), fields.next())
            else {
                return Err(OpeningsError::Tsv {
                    line: i + 1,
                    reason: "expected 3 columns".to_owned(),
                });
            };
            let mut pos = Chess::default();
            for token in pgn.split(' ') {
                if let Ok(san) = token.parse::<San>() {
                    let m = san.to_move(&pos).map_err(|err| OpeningsError::Tsv {
                        line: i + 1,
                        reason: err.to_string(),
                    })?;
                    pos.play_unchecked(m);
                }
            }
            let opening = Opening {
                eco: eco.to_owned(),
                name: name.to_owned(),
            };
            if self
                .data
                .insert(pos.zobrist_hash(EnPassantMode::Legal), opening)
                .is_some()
            {
                return Err(OpeningsError::Duplicate(format!("{eco} {name}")));
            }
        }
        Ok(())
    }

    pub fn classify_exact(&self, pos: &VariantPosition) -> Option<&Opening> {
        if matches!(
            pos.variant(),
            Variant::Chess | Variant::Crazyhouse | Variant::ThreeCheck | Variant::KingOfTheHill
        ) {
            self.data.get(&pos.zobrist_hash(EnPassantMode::Legal))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use shakmaty::uci::UciMove;

    use super::*;

    #[test]
    fn test_load_tsv() {
        let mut openings = Openings::default();
        openings
            .load_tsv("eco\tname\tpgn\nB00\tKing's Pawn\t1. e4\nA40\tQueen's Pawn\t1. d4\n")
            .unwrap();
        assert_eq!(openings.len(), 2);
        let mut pos = VariantPosition::new(Variant::Chess);
        assert_eq!(openings.classify_exact(&pos), None);
        let m = "e2e4".parse::<UciMove>().unwrap().to_move(&pos).unwrap();
        pos.play_unchecked(m);
        assert_eq!(openings.classify_exact(&pos).unwrap().eco, "B00");
    }
}
