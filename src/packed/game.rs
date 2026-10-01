use arbitrary::{Arbitrary, Unstructured};
use bitflags::bitflags;
use btoi::btou;
use bytes::{Buf, BufMut};
use shakmaty::{
    ByColor, CastlingMode, Color, KnownOutcome, Outcome, Position,
    packed::{PackedSetup, PackedUciMove},
    variant::{Variant, VariantPosition},
};
use thiserror::Error;

use crate::packed::{
    EncodedMoves, LaxDate, MoveEncoder, Rating, pack_bytes, pack_string, unpack_bytes,
    unpack_string,
};

pub fn unpack_uci_move<B: Buf>(buf: &mut B) -> PackedUciMove {
    let mut bytes = [0; PackedUciMove::BYTES];
    buf.copy_to_slice(&mut bytes);
    PackedUciMove::from_bytes(bytes)
}

pub fn outcome_bits(outcome: Outcome) -> u8 {
    match outcome {
        Outcome::Unknown => 0,
        Outcome::Known(KnownOutcome::Decisive {
            winner: Color::White,
        }) => 1,
        Outcome::Known(KnownOutcome::Decisive {
            winner: Color::Black,
        }) => 2,
        Outcome::Known(KnownOutcome::Draw) => 3,
    }
}

pub fn outcome_from_bits(bits: u8) -> Option<Outcome> {
    Some(match bits {
        0 => Outcome::Unknown,
        1 => Outcome::Known(KnownOutcome::Decisive {
            winner: Color::White,
        }),
        2 => Outcome::Known(KnownOutcome::Decisive {
            winner: Color::Black,
        }),
        3 => Outcome::Known(KnownOutcome::Draw),
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    UltraBullet = 1,
    Bullet = 2,
    Blitz = 3,
    Rapid = 4,
    Classical = 5,
    Correspondence = 6,
}

impl Speed {
    pub fn bits(self) -> u8 {
        match self {
            Speed::UltraBullet => 1,
            Speed::Bullet => 2,
            Speed::Blitz => 3,
            Speed::Rapid => 4,
            Speed::Classical => 5,
            Speed::Correspondence => 6,
        }
    }

    pub fn from_bits(bits: u8) -> Option<Speed> {
        Some(match bits {
            1 => Speed::UltraBullet,
            2 => Speed::Bullet,
            3 => Speed::Blitz,
            4 => Speed::Rapid,
            5 => Speed::Classical,
            6 => Speed::Correspondence,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Speed::UltraBullet => "ultraBullet",
            Speed::Bullet => "bullet",
            Speed::Blitz => "blitz",
            Speed::Rapid => "rapid",
            Speed::Classical => "classical",
            Speed::Correspondence => "correspondence",
        }
    }

    pub fn from_ascii(bytes: &[u8]) -> Result<Speed, InvalidSpeed> {
        Ok(match bytes {
            b"ultraBullet" => Speed::UltraBullet,
            b"bullet" => Speed::Bullet,
            b"blitz" => Speed::Blitz,
            b"rapid" => Speed::Rapid,
            b"classical" => Speed::Classical,
            b"correspondence" => Speed::Correspondence,
            _ => return Err(InvalidSpeed),
        })
    }

    fn from_seconds_and_increment(seconds: u32, increment: u32) -> Speed {
        let total = seconds.saturating_add(increment.saturating_mul(40));

        if total < 30 {
            Speed::UltraBullet
        } else if total < 180 {
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

impl RatingMode {
    pub fn bits(self) -> u8 {
        match self {
            RatingMode::Unrated => 0,
            RatingMode::Rated => 1,
        }
    }

    pub fn from_bits(bits: u8) -> Option<RatingMode> {
        Some(match bits {
            0 => RatingMode::Unrated,
            1 => RatingMode::Rated,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            RatingMode::Unrated => "unrated",
            RatingMode::Rated => "rated",
        }
    }

    pub fn from_ascii(bytes: &[u8]) -> Result<RatingMode, InvalidRatingMode> {
        Ok(match bytes {
            b"unrated" => RatingMode::Unrated,
            b"rated" => RatingMode::Rated,
            _ => return Err(InvalidRatingMode),
        })
    }
}

#[derive(Debug, Error)]
#[error("invalid rating mode")]
pub struct InvalidRatingMode;

pub fn pack_speed_and_rating_mode<B: BufMut>(buf: &mut B, speed: Speed, rating_mode: RatingMode) {
    buf.put_u8(speed.bits() | (rating_mode.bits() << 3));
}

pub fn unpack_speed_and_rating_mode<B: Buf>(buf: &mut B) -> (Speed, RatingMode) {
    let packed = buf.get_u8();
    let speed = Speed::from_bits(packed & 0b111).expect("valid speed");
    let rating_mode = RatingMode::from_bits(packed >> 3).expect("valid rating mode");
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LichessGame {
    pub index_status: IndexStatus,
    pub players: ByColor<GamePlayer>,
    pub date: LaxDate,
    pub outcome: Outcome,
    pub speed: Speed,
    pub rating_mode: RatingMode,
    pub setup: Option<PackedSetup>,
    pub moves: EncodedMoves,
}

impl LichessGame {
    /// The position before the first move.
    pub fn initial_position(&self) -> Result<VariantPosition, InvalidSetup> {
        match &self.setup {
            None => Ok(VariantPosition::new(Variant::Chess)),
            Some(setup) => {
                let (setup, variant) = setup.unpack_variant().map_err(|_| InvalidSetup)?;
                VariantPosition::from_setup(variant, setup, CastlingMode::Chess960)
                    .map_err(|_| InvalidSetup)
            }
        }
    }

    pub fn pack_entire<B: BufMut>(&self, buf: &mut B) {
        self.index_status.pack(buf);
        self.players.white.pack(buf);
        self.players.black.pack(buf);
        self.date.pack(buf);
        buf.put_u8(outcome_bits(self.outcome));
        pack_speed_and_rating_mode(buf, self.speed, self.rating_mode);
        pack_bytes(
            buf,
            self.setup.as_ref().map_or(b"", |setup| setup.as_bytes()),
        );
        buf.put_slice(self.moves.as_bytes());
    }

    pub fn unpack_entire<B: Buf>(buf: &mut B) -> Self {
        let index_status = IndexStatus::unpack(buf);
        let players = ByColor {
            white: GamePlayer::unpack(buf),
            black: GamePlayer::unpack(buf),
        };
        let date = LaxDate::unpack(buf);
        let outcome = outcome_from_bits(buf.get_u8()).expect("valid outcome");
        let (speed, rating_mode) = unpack_speed_and_rating_mode(buf);

        let setup = Some(unpack_bytes(buf))
            .filter(|bytes| !bytes.is_empty())
            .map(|bytes| PackedSetup::try_from_bytes(&bytes).expect("packed setup"));

        let moves = EncodedMoves::from_bytes(buf.copy_to_bytes(buf.remaining()).to_vec());

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

impl Arbitrary<'_> for LichessGame {
    fn arbitrary(u: &mut Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(LichessGame {
            index_status: u.arbitrary()?,
            players: u.arbitrary()?,
            date: u.arbitrary()?,
            outcome: u.arbitrary()?,
            speed: u.arbitrary()?,
            rating_mode: u.arbitrary()?,
            setup: None,
            moves: EncodedMoves::from_bytes(vec![0x80]),
        })
        .map(|mut game| {
            // Random legal game from a random (or the standard) position.
            let start = Option::<VariantPosition>::arbitrary(u)
                .ok()
                .flatten()
                .unwrap_or_else(|| VariantPosition::new(Variant::Chess));
            game.setup = (start.variant() != Variant::Chess
                || start != VariantPosition::new(Variant::Chess))
            .then(|| {
                PackedSetup::pack_variant_normalized(
                    &start.to_setup(shakmaty::EnPassantMode::Legal),
                    start.variant(),
                )
                .expect("can losslessly pack position")
            });
            let mut pos = game.initial_position().expect("packed setup roundtrip");
            let mut encoder = MoveEncoder::new();
            let plies = u.int_in_range(0..=100).unwrap_or(0);
            for _ in 0..plies {
                let legals = pos.legal_moves();
                if legals.is_empty() {
                    break;
                }
                let Ok(m) = u.choose(&legals) else { break };
                encoder.push(&pos, *m).expect("encodable legal move");
                pos.play_unchecked(*m);
            }
            game.moves = encoder.finish();
            game
        })
    }
}

#[derive(Debug, Error)]
#[error("invalid setup")]
pub struct InvalidSetup;

#[cfg(test)]
mod tests {
    use arbtest::arbtest;

    use super::*;

    #[test]
    fn test_speed_str_roundtrip() {
        for speed in [
            Speed::UltraBullet,
            Speed::Bullet,
            Speed::Blitz,
            Speed::Rapid,
            Speed::Classical,
            Speed::Correspondence,
        ] {
            assert_eq!(Speed::from_ascii(speed.as_str().as_bytes()).unwrap(), speed);
        }
    }

    #[test]
    fn test_rating_mode_str_roundtrip() {
        for rating_mode in [RatingMode::Unrated, RatingMode::Rated] {
            assert_eq!(
                RatingMode::from_ascii(rating_mode.as_str().as_bytes()).unwrap(),
                rating_mode
            );
        }
    }

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

    #[test]
    fn test_pack_roundtrip() {
        arbtest(|u| {
            let game: LichessGame = u.arbitrary()?;

            let mut buf = Vec::new();
            game.pack_entire(&mut buf);
            assert_eq!(LichessGame::unpack_entire(&mut &buf[..]), game);
            Ok(())
        });
    }
}
