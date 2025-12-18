use bincode::{Decode, Encode};
use shakmaty::{
    ByColor, KnownOutcome,
    packed::{PackedSetup, PackedUciMove},
};

use crate::packed::{GameId, LaxDate, Rating};

#[derive(Debug, Clone, Encode, Decode)]
pub struct GamePlayer {
    name: String,
    title: String,
    rating: Rating,
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct GameMeta {
    id: GameId,
    outcome: KnownOutcome,
    player: ByColor<GamePlayer>,
    broadcast_name: String,
    date: LaxDate,
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct Game {
    meta: GameMeta,
    setup: Option<PackedSetup>,
    moves: Vec<PackedUciMove>,
}
