use arbitrary::{Arbitrary, Unstructured};
use bitflags::bitflags;
use btoi::btou;
use bytes::{Buf, BufMut};
use shakmaty::{
    ByColor, Color, KnownOutcome, Outcome,
    packed::{PackedSetup, PackedUciMove},
};
use thiserror::Error;

use crate::packed::{LaxDate, Rating, pack_bytes, pack_string, unpack_bytes, unpack_string};

pub fn pack_outcome<B: BufMut>(buf: &mut B, outcome: Outcome) {
    buf.put_u8(match outcome {
        Outcome::Unknown => 0,
        Outcome::Known(KnownOutcome::Decisive {
            winner: Color::White,
        }) => 1,
        Outcome::Known(KnownOutcome::Decisive {
            winner: Color::Black,
        }) => 2,
        Outcome::Known(KnownOutcome::Draw) => 3,
    });
}

pub fn unpack_outcome<B: Buf>(buf: &mut B) -> Outcome {
    match buf.get_u8() {
        0 => Outcome::Unknown,
        1 => Outcome::Known(KnownOutcome::Decisive {
            winner: Color::White,
        }),
        2 => Outcome::Known(KnownOutcome::Decisive {
            winner: Color::Black,
        }),
        3 => Outcome::Known(KnownOutcome::Draw),
        _ => panic!("invalid outcome"),
    }
}

#[derive(Debug)]
pub struct GamePlayer {
    pub title: Option<String>,
    pub name: String,
    pub rating: Rating,
}

impl GamePlayer {
    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        pack_string(buf, self.title.as_ref().unwrap_or(&String::new()));
        pack_string(buf, &self.name);
        self.rating.pack(buf);
    }

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        GamePlayer {
            title: Some(unpack_string(buf)).filter(|s| !s.is_empty()),
            name: unpack_string(buf),
            rating: Rating::unpack(buf),
        }
    }
}

impl Arbitrary<'_> for GamePlayer {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(GamePlayer {
            title: Some(u.arbitrary::<String>()?).filter(|s| !s.is_empty()),
            name: u.arbitrary()?,
            rating: u.arbitrary()?,
        })
    }
}

#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Speed {
    UltraBullet,
    Bullet,
    Blitz,
    Rapid,
    Classical,
    Correspondence,
}

impl Speed {
    fn from_seconds_and_increment(seconds: u32, increment: u32) -> Speed {
        let total = seconds.saturating_add(increment.saturating_mul(40));

        if total < 30 {
            Speed::UltraBullet
        } else if total < 100 {
            Speed::Bullet
        } else if total < 480 {
            Speed::Blitz
        } else if total < 1500 {
            Speed::Rapid
        } else if total < 21_600 {
            Speed::Classical
        } else {
            Speed::Correspondence
        }
    }

    pub fn from_time_control_tag(value: &[u8]) -> Result<Speed, InvalidSpeed> {
        if value == b"-" {
            return Ok(Speed::Correspondence);
        }

        let mut parts = value.splitn(2, |&ch| ch == b'+');
        let seconds = btou(parts.next().expect("first part")).map_err(|_| InvalidSpeed)?;
        let increment = btou(parts.next().ok_or(InvalidSpeed)?).map_err(|_| InvalidSpeed)?;
        Ok(Speed::from_seconds_and_increment(seconds, increment))
    }
}

#[derive(Debug, Error)]
#[error("Invalid speed")]
pub struct InvalidSpeed;

#[derive(Arbitrary, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum RatingMode {
    Unrated,
    Rated,
}

pub fn pack_speed_and_rating_mode<B: BufMut>(buf: &mut B, speed: Speed, rating_mode: RatingMode) {
    let packed_speed = match speed {
        Speed::UltraBullet => 0,
        Speed::Bullet => 1,
        Speed::Blitz => 2,
        Speed::Rapid => 3,
        Speed::Classical => 4,
        Speed::Correspondence => 5,
    };
    let packed_rating_mode = match rating_mode {
        RatingMode::Unrated => 0,
        RatingMode::Rated => 1,
    };
    buf.put_u8(packed_speed | (packed_rating_mode << 7));
}

pub fn unpack_speed_and_rating_mode<B: Buf>(buf: &mut B) -> (Speed, RatingMode) {
    let packed = buf.get_u8();
    let speed = match packed & 0x7F {
        0 => Speed::UltraBullet,
        1 => Speed::Bullet,
        2 => Speed::Blitz,
        3 => Speed::Rapid,
        4 => Speed::Classical,
        5 => Speed::Correspondence,
        _ => panic!("invalid speed"),
    };
    let rating_mode = match packed >> 7 {
        0 => RatingMode::Unrated,
        1 => RatingMode::Rated,
        _ => panic!("invalid rating mode"),
    };
    (speed, rating_mode)
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct IndexStatus: u8 {
        const LICHESS = 1 << 0;
        const PLAYER_WHITE = 1 << 1;
        const PLAYER_BLACK = 1 << 2;
    }
}

impl IndexStatus {
    pub fn pack<B: BufMut>(&self, buf: &mut B) {
        buf.put_u8(self.bits());
    }

    pub fn unpack<B: Buf>(buf: &mut B) -> Self {
        Self::from_bits_truncate(buf.get_u8())
    }
}

impl Arbitrary<'_> for IndexStatus {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(IndexStatus::from_bits_truncate(u.arbitrary()?))
    }
}

#[derive(Debug)]
pub struct LichessGame {
    pub index_status: IndexStatus,
    pub players: ByColor<GamePlayer>,
    pub date: LaxDate,
    pub outcome: Outcome,
    pub speed: Speed,
    pub rating_mode: RatingMode,
    pub setup: Option<PackedSetup>,
    pub moves: Vec<PackedUciMove>,
}

impl LichessGame {
    pub fn pack_entire<B: BufMut>(&self, buf: &mut B) {
        self.index_status.pack(buf);
        self.players.white.pack(buf);
        self.players.black.pack(buf);
        self.date.pack(buf);
        pack_outcome(buf, self.outcome);
        pack_speed_and_rating_mode(buf, self.speed, self.rating_mode);
        pack_bytes(
            buf,
            self.setup.as_ref().map_or(b"", |setup| setup.as_bytes()),
        );
        for m in &self.moves {
            buf.put_slice(&m.to_bytes());
        }
    }

    pub fn unpack_entire<B: Buf>(buf: &mut B) -> Self {
        let index_status = IndexStatus::unpack(buf);
        let players = ByColor {
            white: GamePlayer::unpack(buf),
            black: GamePlayer::unpack(buf),
        };
        let date = LaxDate::unpack(buf);
        let outcome = unpack_outcome(buf);
        let (speed, rating_mode) = unpack_speed_and_rating_mode(buf);

        let setup = Some(unpack_bytes(buf))
            .filter(|bytes| !bytes.is_empty())
            .map(|bytes| PackedSetup::try_from_bytes(&bytes).expect("packed setup"));

        let mut moves = Vec::with_capacity(buf.remaining() / 2);
        while buf.has_remaining() {
            let mut packed_bytes = [0; 2];
            buf.copy_to_slice(&mut packed_bytes);
            moves.push(PackedUciMove::from_bytes(packed_bytes));
        }

        Self {
            index_status,
            players,
            date,
            outcome,
            speed,
            rating_mode,
            setup,
            moves,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_speed_from_time_control_tag() {
        assert_eq!(
            Speed::from_time_control_tag(b"-").unwrap(),
            Speed::Correspondence
        );
        assert_eq!(
            Speed::from_time_control_tag(b"60+0").unwrap(),
            Speed::Bullet
        );
        assert_eq!(
            Speed::from_time_control_tag(b"120+2").unwrap(),
            Speed::Blitz
        );
    }
}
