use roaring::RoaringTreemap;

use crate::packed::GameId;

pub struct GameIdSet {
    inner: RoaringTreemap,
}

impl GameIdSet {
    pub fn new() -> Self {
        Self {
            inner: RoaringTreemap::new(),
        }
    }

    pub fn insert(&mut self, game_id: GameId) -> bool {
        self.inner.insert(game_id.to_u64())
    }
}
