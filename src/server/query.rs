use std::{cmp::min, fmt};

use serde::Deserialize;
use shakmaty::{
    CastlingMode, EnPassantMode, Position, PositionError, Setup,
    fen::Fen,
    uci::UciMove,
    variant::{Variant, VariantPosition},
};

use crate::{
    packed::{Month, RatingGroup, Speed},
    server::openings::{Opening, Openings},
};

/// Months of lichess queries are restricted to this range, like in the
/// previous version of the explorer.
pub const MIN_YEAR: u16 = 1952;
pub const MAX_YEAR: u16 = 3000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRequest(pub String);

impl fmt::Display for BadRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad request: {}", self.0)
    }
}

impl From<PositionError<VariantPosition>> for BadRequest {
    fn from(err: PositionError<VariantPosition>) -> BadRequest {
        BadRequest(err.to_string())
    }
}

/// Raw query parameters, before validation.
#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct RawQuery {
    pub variant: Option<String>,
    pub fen: Option<String>,
    pub play: Option<String>,
    pub speeds: Option<String>,
    pub ratings: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub moves: Option<String>,
    pub top_games: Option<String>,
    pub recent_games: Option<String>,
    pub history: Option<String>,
    /// Client that sent the request (ignored).
    pub source: Option<String>,
}

/// The root position and the moves played from it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Play {
    pub variant: Variant,
    pub setup: Setup,
    pub play: Vec<UciMove>,
}

pub struct PlayPosition {
    pub pos: VariantPosition,
    pub opening: Option<Opening>,
    /// Plies from the root position of the variant (approximately: from the
    /// move counters of the root setup).
    pub ply: u32,
}

impl Play {
    fn parse(raw: &RawQuery) -> Result<Play, BadRequest> {
        let variant = match &raw.variant {
            Some(variant) => variant
                .parse::<Variant>()
                .map_err(|_| BadRequest(format!("unknown variant {variant:?}")))?,
            None => Variant::Chess,
        };
        let setup = match raw.fen.as_deref().filter(|fen| !fen.is_empty()) {
            Some(fen) => fen
                .parse::<Fen>()
                .map_err(|err| BadRequest(format!("invalid fen: {err}")))?
                .into_setup(),
            None => VariantPosition::new(variant).to_setup(EnPassantMode::Always),
        };
        let play = parse_list(raw.play.as_deref(), |uci| {
            uci.parse::<UciMove>()
                .map_err(|err| BadRequest(format!("invalid move {uci:?}: {err}")))
        })?
        .unwrap_or_default();
        Ok(Play {
            variant,
            setup,
            play,
        })
    }

    pub fn position(&self, openings: &Openings) -> Result<PlayPosition, BadRequest> {
        let mut pos =
            VariantPosition::from_setup(self.variant, self.setup.clone(), CastlingMode::Chess960)
                .or_else(PositionError::ignore_invalid_castling_rights)
                .or_else(PositionError::ignore_invalid_ep_square)
                .or_else(PositionError::ignore_too_much_material)?;
        let mut opening = openings.classify_exact(&pos);
        for uci in &self.play {
            let m = uci
                .to_move(&pos)
                .map_err(|err| BadRequest(format!("illegal move {uci}: {err}")))?;
            pos.play_unchecked(m);
            opening = openings.classify_exact(&pos).or(opening);
        }
        let ply = (u32::from(pos.fullmoves()) - 1)
            .saturating_mul(2)
            .saturating_add(pos.turn().fold_wb(0, 1));
        Ok(PlayPosition {
            pos,
            opening: opening.cloned(),
            ply,
        })
    }
}

fn parse_list<T>(
    value: Option<&str>,
    mut parse: impl FnMut(&str) -> Result<T, BadRequest>,
) -> Result<Option<Vec<T>>, BadRequest> {
    match value {
        None => Ok(None),
        Some(value) => value
            .split(',')
            .filter(|part| !part.is_empty())
            .map(|part| parse(part.trim()))
            .collect::<Result<Vec<T>, _>>()
            .map(Some),
    }
}

fn parse_limit(value: Option<&str>, default: usize) -> Result<usize, BadRequest> {
    match value {
        None => Ok(default),
        Some(value) => value
            .trim()
            .parse::<usize>()
            .map_err(|_| BadRequest(format!("invalid limit {value:?}"))),
    }
}

fn parse_month(value: &str) -> Result<Month, BadRequest> {
    let (year, month) = value
        .split_once(['-', '/'])
        .ok_or_else(|| BadRequest(format!("invalid month {value:?}")))?;
    let year: u16 = year
        .parse()
        .map_err(|_| BadRequest(format!("invalid month {value:?}")))?;
    let month: u16 = month
        .parse()
        .map_err(|_| BadRequest(format!("invalid month {value:?}")))?;
    if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
        return Err(BadRequest(format!("year out of range: {value:?}")));
    }
    Month::from_year_and_month(year, month)
        .map_err(|_| BadRequest(format!("invalid month {value:?}")))
}

fn parse_year(value: &str) -> Result<u16, BadRequest> {
    let year: u16 = value
        .trim()
        .parse()
        .map_err(|_| BadRequest(format!("invalid year {value:?}")))?;
    if (MIN_YEAR..=MAX_YEAR).contains(&year) {
        Ok(year)
    } else {
        Err(BadRequest(format!("year out of range: {value:?}")))
    }
}

/// Rating group of a query parameter, with the semantics of the previous
/// explorer: the value is the lower bound of the group, and anything from
/// 2800 maps to the top group.
fn rating_group_of_param(value: &str) -> Result<RatingGroup, BadRequest> {
    let avg: u16 = value
        .parse()
        .map_err(|_| BadRequest(format!("invalid rating group {value:?}")))?;
    Ok(if avg < 1000 {
        RatingGroup::Low
    } else if avg < 1200 {
        RatingGroup::Group1000
    } else if avg < 1400 {
        RatingGroup::Group1200
    } else if avg < 1600 {
        RatingGroup::Group1400
    } else if avg < 1800 {
        RatingGroup::Group1600
    } else if avg < 2000 {
        RatingGroup::Group1800
    } else if avg < 2200 {
        RatingGroup::Group2000
    } else if avg < 2500 {
        RatingGroup::Group2200
    } else if avg < 2800 {
        RatingGroup::Group2500
    } else {
        RatingGroup::Group3200
    })
}

pub fn rating_group_index(group: RatingGroup) -> u8 {
    match group {
        RatingGroup::Low => 0,
        RatingGroup::Group1000 => 1,
        RatingGroup::Group1200 => 2,
        RatingGroup::Group1400 => 3,
        RatingGroup::Group1600 => 4,
        RatingGroup::Group1800 => 5,
        RatingGroup::Group2000 => 6,
        RatingGroup::Group2200 => 7,
        RatingGroup::Group2500 => 8,
        RatingGroup::Group2800 => 9,
        RatingGroup::Group3200 => 10,
    }
}

pub const ALL_RATING_GROUPS: [RatingGroup; 11] = [
    RatingGroup::Low,
    RatingGroup::Group1000,
    RatingGroup::Group1200,
    RatingGroup::Group1400,
    RatingGroup::Group1600,
    RatingGroup::Group1800,
    RatingGroup::Group2000,
    RatingGroup::Group2200,
    RatingGroup::Group2500,
    RatingGroup::Group2800,
    RatingGroup::Group3200,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LichessFilter {
    /// Bit `Speed::bits()` set for each selected speed, or `None` for all.
    pub speeds: Option<u8>,
    /// Bit `rating_group_index()` set for each selected group, or `None`
    /// for all.
    pub ratings: Option<u16>,
    pub since: Option<Month>,
    pub until: Option<Month>,
}

impl LichessFilter {
    pub fn contains_speed(&self, speed: Option<Speed>) -> bool {
        match self.speeds {
            None => true,
            Some(mask) => speed.is_some_and(|speed| mask & (1 << speed.bits()) != 0),
        }
    }

    /// Like the previous explorer, the groups from 2500 are selected
    /// together.
    pub fn contains_rating_group(&self, group: RatingGroup) -> bool {
        match self.ratings {
            None => true,
            Some(mask) => mask & (1 << rating_group_index(min(group, RatingGroup::Group2500))) != 0,
        }
    }

    pub fn contains_month(&self, month: Month) -> bool {
        self.since.is_none_or(|since| since <= month)
            && self.until.is_none_or(|until| month <= until)
    }

    /// The lowest of the selected top rating groups, if the selection
    /// includes the top groups contiguously down to at least 2000.
    pub fn top_group(&self) -> Option<RatingGroup> {
        let mut top_group = None;
        for group in ALL_RATING_GROUPS.into_iter().rev() {
            if !self.contains_rating_group(group) || group < RatingGroup::Group2000 {
                break;
            }
            top_group = Some(group);
        }
        top_group
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Limits {
    pub moves: usize,
    pub top_games: usize,
    pub recent_games: usize,
}

impl Limits {
    pub const DEFAULT_MOVES: usize = 12;

    pub fn games_wanted(&self) -> bool {
        self.top_games > 0 || self.recent_games > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LichessQuery {
    pub play: Play,
    pub filter: LichessFilter,
    pub limits: Limits,
    pub history: bool,
}

impl LichessQuery {
    pub fn parse(raw: &RawQuery) -> Result<LichessQuery, BadRequest> {
        let speeds = parse_list(raw.speeds.as_deref(), |speed| {
            Speed::from_ascii(speed.as_bytes())
                .map_err(|_| BadRequest(format!("unknown speed {speed:?}")))
        })?
        .map(|speeds| {
            speeds
                .into_iter()
                .fold(0, |mask, speed| mask | 1 << speed.bits())
        });
        let ratings = parse_list(raw.ratings.as_deref(), rating_group_of_param)?.map(|groups| {
            groups
                .into_iter()
                .fold(0, |mask, group| mask | 1 << rating_group_index(group))
        });
        let history = match raw.history.as_deref().map(str::trim) {
            None | Some("" | "false" | "False" | "off" | "0" | "no") => false,
            Some("true" | "True" | "on" | "1" | "yes") => true,
            Some(other) => return Err(BadRequest(format!("invalid history {other:?}"))),
        };
        Ok(LichessQuery {
            play: Play::parse(raw)?,
            filter: LichessFilter {
                speeds,
                ratings,
                since: raw.since.as_deref().map(parse_month).transpose()?,
                until: raw.until.as_deref().map(parse_month).transpose()?,
            },
            limits: Limits {
                moves: parse_limit(raw.moves.as_deref(), Limits::DEFAULT_MOVES)?,
                top_games: parse_limit(raw.top_games.as_deref(), usize::MAX)?,
                recent_games: parse_limit(raw.recent_games.as_deref(), usize::MAX)?,
            },
            history,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MastersQuery {
    pub play: Play,
    pub since: u16,
    pub until: u16,
    pub limits: Limits,
}

impl MastersQuery {
    pub fn parse(raw: &RawQuery) -> Result<MastersQuery, BadRequest> {
        Ok(MastersQuery {
            play: Play::parse(raw)?,
            since: raw.since.as_deref().map_or(Ok(MIN_YEAR), parse_year)?,
            until: raw.until.as_deref().map_or(Ok(MAX_YEAR), parse_year)?,
            limits: Limits {
                moves: parse_limit(raw.moves.as_deref(), Limits::DEFAULT_MOVES)?,
                top_games: parse_limit(raw.top_games.as_deref(), usize::MAX)?,
                recent_games: 0,
            },
        })
    }

    /// The month filter equivalent to the year range.
    pub fn filter(&self) -> LichessFilter {
        LichessFilter {
            speeds: None,
            ratings: None,
            since: Some(Month::from_year_and_month(self.since, 1).expect("valid since")),
            until: Some(Month::from_year_and_month(self.until, 12).expect("valid until")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_top_group() {
        let filter = |ratings: &str| {
            LichessQuery::parse(&RawQuery {
                ratings: Some(ratings.to_owned()),
                ..RawQuery::default()
            })
            .unwrap()
            .filter
        };
        assert_eq!(filter("2500").top_group(), Some(RatingGroup::Group2500));
        assert_eq!(
            filter("2000,2200,2500").top_group(),
            Some(RatingGroup::Group2000)
        );
        assert_eq!(
            filter("1800,2000,2200,2500").top_group(),
            Some(RatingGroup::Group2000)
        );
        assert_eq!(filter("2000,2200").top_group(), None);
        assert_eq!(filter("1600").top_group(), None);
        assert!(filter("2500").contains_rating_group(RatingGroup::Group3200));
        assert!(!filter("2800").contains_rating_group(RatingGroup::Group2800));
        assert!(filter("0").contains_rating_group(RatingGroup::Low));
    }

    #[test]
    fn test_months() {
        assert_eq!(
            parse_month("2020-03").unwrap(),
            Month::from_year_and_month(2020, 3).unwrap()
        );
        assert!(parse_month("1900-01").is_err());
        assert!(parse_month("2020-13").is_err());
        assert!(parse_month("2020").is_err());
    }

    #[test]
    fn test_play() {
        let q = LichessQuery::parse(&RawQuery {
            variant: Some("standard".to_owned()),
            fen: Some("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".to_owned()),
            play: Some("e2e4,e7e5".to_owned()),
            ..RawQuery::default()
        })
        .unwrap();
        let pos = q.play.position(&Openings::default()).unwrap();
        assert_eq!(pos.ply, 2);
        assert!(
            LichessQuery::parse(&RawQuery {
                play: Some("e2e5".to_owned()),
                ..RawQuery::default()
            })
            .unwrap()
            .play
            .position(&Openings::default())
            .is_err()
        );
    }
}
