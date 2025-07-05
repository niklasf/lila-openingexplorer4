use crate::packed::GameId;
use crate::packed::LaxDate;
use bincode::{Decode, Encode};
use shakmaty::Outcome;
use shakmaty::packed::PackedSetup;
use shakmaty::packed::PackedUciMove;

#[derive(Debug, Clone, Encode, Decode)]
struct GamePlayer {
    name: String,
    title: String,
    rating: u16,
}

#[derive(Debug, Clone, Encode, Decode)]
struct GameMeta {
    id: GameId,
    outcome: Outcome,
    white: GamePlayer,
    black: GamePlayer,
    broadcast_name: String,
    date: LaxDate,
}

#[derive(Debug, Clone, Encode, Decode)]
struct Game {
    meta: GameMeta,
    setup: Option<PackedSetup>,
    uci: Vec<PackedUciMove>,
}
