//! Storage tests: whatever the chunk boundaries, placement decisions and
//! rebuild points, reads give the same entries as a naive aggregation.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use arbitrary::Unstructured;
use arbtest::arbtest;
use shakmaty::{
    CastlingMode, Chess, Color, EnPassantMode, Position,
    packed::PackedUciMove,
    variant::{Variant, VariantPosition},
};

use crate::{
    packed::{
        CellKey, GameId, GameIndex, Month, PackedMoves, PlayerEntry, PlayerGroupHeader,
        PositionKey, Rating, RatingMode, Speed,
    },
    player::{
        PlayerDb, PlayerDbConfig, UserId, aggregate::Placement, record::PlayerGame,
        status::PlayerStatus,
    },
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "player-{name}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A random game with a few likely openings, so that positions repeat
/// across games.
fn arbitrary_game(
    u: &mut Unstructured<'_>,
    n: u64,
) -> arbitrary::Result<(PlayerGame, Vec<VariantPosition>)> {
    let mut pos = if u.ratio(1, 20)? {
        VariantPosition::new(Variant::Atomic)
    } else if u.ratio(1, 20)? {
        VariantPosition::from_setup(
            Variant::Chess,
            "rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2"
                .parse::<shakmaty::fen::Fen>()
                .unwrap()
                .into_setup(),
            CastlingMode::Chess960,
        )
        .unwrap()
    } else {
        VariantPosition::Chess(Chess::default())
    };
    let setup = PlayerGame::setup_of(&pos);
    let mut encoder = PackedMoves::encoder();
    let mut positions = vec![pos.clone()];
    let plies = u.int_in_range(0..=60)?;
    for ply in 0..plies {
        let legals = pos.legal_moves();
        if legals.is_empty() {
            break;
        }
        // Mostly the first moves early on, to have common positions, and
        // sometimes back and forth, for loops.
        let m = if ply < 6 && u.ratio(3, 4)? {
            legals[usize::from(u.int_in_range(0..=1u8)?).min(legals.len() - 1)]
        } else {
            *u.choose(&legals)?
        };
        encoder.encode_next(&pos, m);
        pos.play_unchecked(m);
        positions.push(pos.clone());
    }
    let mut id = [0u8; 8];
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut x = n;
    for c in id.iter_mut() {
        *c = digits[(x % 62) as usize];
        x /= 62;
    }
    Ok((
        PlayerGame {
            id: GameId::from_ascii(&id).unwrap(),
            speed: Speed::from_bits(u.int_in_range(1..=6)?).unwrap(),
            mode: if u.arbitrary()? {
                RatingMode::Rated
            } else {
                RatingMode::Unrated
            },
            month: Month::from_months_since_jan_1900(u.int_in_range(1400..=1404)?),
            outcome: u.arbitrary()?,
            color: if u.arbitrary()? {
                Color::White
            } else {
                Color::Black
            },
            player_rating: Rating::try_from(u.int_in_range(800..=2800)?).unwrap(),
            opponent_rating: Rating::try_from(u.int_in_range(800..=2800)?).unwrap(),
            opponent_name: "opponent".to_owned(),
            opponent_title: None,
            player_title: None,
            setup,
            moves: encoder.finish(),
        },
        positions,
    ))
}

/// Naive aggregation: replay every game, keep the last move from each
/// position within the first `max_ply` plies.
fn naive(
    games: &[(PlayerGame, Vec<VariantPosition>)],
    max_ply: usize,
) -> HashMap<(Color, PositionKey), PlayerEntry> {
    let mut entries: HashMap<(Color, PositionKey), PlayerEntry> = HashMap::new();
    for (seq, (game, positions)) in games.iter().enumerate() {
        let mut pos = game.as_borrowed().initial_position();
        let mut decoder = game.moves.decode().unwrap();
        let mut last: BTreeMap<PositionKey, PackedUciMove> = BTreeMap::new();
        let mut ply = 0;
        while ply < max_ply
            && let Some(m) = decoder.decode_next(&pos).unwrap()
        {
            assert_eq!(pos, positions[ply]);
            last.insert(
                PositionKey::new(&pos),
                PackedUciMove::pack(m.to_uci(CastlingMode::Chess960)),
            );
            pos.play_unchecked(m);
            ply += 1;
        }
        for (key, m) in last {
            entries.entry((game.color, key)).or_default().add_game(
                CellKey::new(
                    m,
                    PlayerGroupHeader {
                        speed: game.speed,
                        mode: game.mode,
                    },
                    game.month,
                ),
                game.opponent_rating,
                game.outcome,
                GameIndex::new(seq as u64).unwrap(),
            );
        }
    }
    entries
}

fn pack(mut entry: PlayerEntry) -> Vec<u8> {
    let mut buf = Vec::new();
    entry.pack_entire(&mut buf);
    buf
}

#[test]
fn test_storage_matches_naive() {
    arbtest(|u| {
        let dir = TempDir::new("storage");
        let max_ply = u.int_in_range(1..=60)?;
        let placement = if u.ratio(1, 3)? {
            Placement::Threshold(u.int_in_range(1..=4)?)
        } else {
            Placement::Random(u.arbitrary()?)
        };
        let db = PlayerDb::open(
            &dir.0,
            PlayerDbConfig {
                cache_bytes: 8 << 20,
                max_ply,
                placement,
                // Sometimes ingest even small rebuilds.
                ingest_bytes: if u.arbitrary()? { 0 } else { 64 << 20 },
                batch_bytes: u.int_in_range(100..=10_000)?,
                ..PlayerDbConfig::default()
            },
        )
        .unwrap();

        let users = [
            UserId::from_name("alice").unwrap(),
            UserId::from_name("bob").unwrap(),
        ];
        let mut games: [Vec<(PlayerGame, Vec<VariantPosition>)>; 2] = [Vec::new(), Vec::new()];
        let mut statuses = users
            .clone()
            .map(|user| PlayerStatus::new(user, max_ply as u32));
        let mut n = 0;
        for _ in 0..u.int_in_range(0..=8)? {
            let player = usize::from(u.ratio(1, 4)?);
            let mut chunk = Vec::new();
            for _ in 0..u.int_in_range(0..=30)? {
                // Sometimes a game that is already indexed again.
                if u.ratio(1, 10)?
                    && let Some((game, _)) = u.choose(&games[player]).ok().cloned()
                {
                    chunk.push(game);
                    continue;
                }
                let game = arbitrary_game(u, n)?;
                n += 1;
                chunk.push(game.0.clone());
                games[player].push(game);
            }
            let stats = db.write_chunk(&mut statuses[player], &chunk).unwrap();
            assert_eq!(stats.games + stats.known, chunk.len() as u64);
            if u.ratio(1, 3)? {
                db.rebuild(users[player].hash()).unwrap();
                statuses[player] = db.status(users[player].hash()).unwrap().unwrap();
            }
            if u.ratio(1, 4)? {
                db.flush().unwrap();
            }
        }
        if u.arbitrary()? {
            db.compact();
        }

        let reader = db.reader();
        for (player, user) in users.iter().enumerate() {
            let expected = naive(&games[player], max_ply);
            let Some(status) = reader.status(user.hash()).unwrap() else {
                assert!(games[player].is_empty());
                continue;
            };
            assert_eq!(status.games as usize, games[player].len());
            let mut probes: Vec<(Color, VariantPosition)> = games[player]
                .iter()
                .flat_map(|(game, positions)| positions.iter().map(|pos| (game.color, pos.clone())))
                .collect();
            probes.push((Color::White, VariantPosition::new(Variant::Chess)));
            probes.push((Color::Black, VariantPosition::new(Variant::Chess)));
            for (color, pos) in probes {
                let key = PositionKey::new(&pos);
                let (entry, _) = reader.position(&status, color, &pos).unwrap();
                let expected = expected.get(&(color, key)).cloned().unwrap_or_default();
                assert_eq!(
                    pack(entry),
                    pack(expected),
                    "{user} {color} {}",
                    shakmaty::fen::Fen::from_position(&pos, EnPassantMode::Legal)
                );
            }
        }
        Ok(())
    })
    .budget_ms(10_000);
}

#[test]
fn test_delete_player() {
    let dir = TempDir::new("delete");
    let db = PlayerDb::open(&dir.0, PlayerDbConfig::default()).unwrap();
    let alice = UserId::from_name("alice").unwrap();
    let mut status = PlayerStatus::new(alice.clone(), 50);
    let mut u = Unstructured::new(&[7; 4096]);
    let (game, _) = arbitrary_game(&mut u, 1).unwrap();
    db.write_chunk(&mut status, &[game]).unwrap();
    assert!(db.status(alice.hash()).unwrap().is_some());
    db.delete_player(alice.hash()).unwrap();
    assert!(db.status(alice.hash()).unwrap().is_none());
}

#[test]
fn test_db_paths() {
    let dir = TempDir::new("paths");
    let fast = dir.0.join("fast");
    let slow = dir.0.join("slow");
    std::fs::create_dir_all(&fast).unwrap();
    std::fs::create_dir_all(&slow).unwrap();
    let db = PlayerDb::open(
        &dir.0.join("db"),
        PlayerDbConfig {
            db_paths: vec![(fast.clone(), 1 << 30), (slow, 1 << 40)],
            ..PlayerDbConfig::default()
        },
    )
    .unwrap();
    let alice = UserId::from_name("alice").unwrap();
    let mut status = PlayerStatus::new(alice, 50);
    let mut u = Unstructured::new(&[3; 4096]);
    let (game, _) = arbitrary_game(&mut u, 1).unwrap();
    db.write_chunk(&mut status, &[game]).unwrap();
    db.flush().unwrap();
    let ssts = std::fs::read_dir(&fast)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "sst")
        })
        .count();
    assert!(ssts > 0);
}
