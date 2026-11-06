//! Indexing against the mock lila.

use std::{sync::Arc, time::Duration};

use shakmaty::{Color, variant::VariantPosition};

use crate::{
    mock_lila::{
        Faults, MockConfig, MockLila, Noise,
        fixture::{FixtureGame, write_synthetic},
    },
    player::{
        PlayerDb, PlayerDbConfig, UserId,
        indexer::{Indexer, IndexerConfig},
        lila::{LilaClient, LilaClientConfig},
        status::RunPolicy,
        tests::TempDir,
    },
    util::civil::{MS_PER_DAY, days_from_civil},
};

struct Setup {
    _fixture_dir: TempDir,
    _db_dir: TempDir,
    mock: MockLila,
    indexer: Arc<Indexer>,
    players: Vec<UserId>,
    games: Vec<Vec<FixtureGame>>,
    indexers: usize,
}

async fn setup(
    players: usize,
    games: impl Fn(usize) -> usize,
    indexers: usize,
    faults: Faults,
    config: IndexerConfig,
) -> Setup {
    let fixture_dir = TempDir::new("fixture");
    let mut users: Vec<UserId> = (0..players)
        .map(|i| UserId::from_name(&format!("player{i}")).unwrap())
        .collect();
    users.sort();
    let start = days_from_civil(2026, 6, 1) as u64 * MS_PER_DAY;
    let fixture_games = write_synthetic(&fixture_dir.0, &users, games, start, 42).unwrap();
    let mock = MockLila::start(MockConfig {
        fixture: fixture_dir.0.clone(),
        data_end: days_from_civil(2026, 11, 1) as u64 * MS_PER_DAY,
        max_streams: indexers,
        casual: 0.5,
        noise: Noise::default(),
        faults,
        games_per_second: 20_000.0,
        ..MockConfig::default()
    })
    .await
    .unwrap();
    let db_dir = TempDir::new("indexer-db");
    let db = Arc::new(
        PlayerDb::open(
            &db_dir.0,
            PlayerDbConfig {
                cache_bytes: 16 << 20,
                ..PlayerDbConfig::default()
            },
        )
        .unwrap(),
    );
    let lila = Arc::new(LilaClient::new(LilaClientConfig {
        url: mock.url(),
        streams: indexers,
        read_timeout: Duration::from_millis(500),
        ..LilaClientConfig::default()
    }));
    let indexer = Indexer::start(
        db,
        lila,
        IndexerConfig {
            indexers,
            retry_delay: Duration::from_millis(20),
            retries: 20,
            ..config
        },
    );
    Setup {
        _fixture_dir: fixture_dir,
        _db_dir: db_dir,
        mock,
        indexer,
        players: users,
        games: fixture_games,
        indexers,
    }
}

impl Setup {
    /// Requests all players and waits until indexing is complete.
    async fn index_all(&self) {
        let mut tickets = Vec::new();
        for user in &self.players {
            tickets.push(self.indexer.request(user).await.unwrap());
        }
        for mut ticket in tickets {
            while !ticket.progress().done {
                ticket.changed().await;
            }
        }
        // Wait for rebuilds.
        for _ in 0..500 {
            if self.indexer.queue().stats().busy == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn check(&self) {
        let db = self.indexer.db();
        let reader = db.reader();
        for (user, games) in self.players.iter().zip(&self.games) {
            let status = reader.status(db.user_hash(user)).unwrap().expect("status");
            assert_eq!(status.games as usize, games.len(), "{user}");
            assert!(status.name.eq_ignore_ascii_case(user.as_str()));
            for color in Color::ALL {
                let expected = games
                    .iter()
                    .filter(|g| !g.sans.is_empty() && user.is_name(&g.players.get(color).name))
                    .count() as u64;
                let (entry, _) = reader
                    .position(
                        &status,
                        color,
                        &VariantPosition::new(shakmaty::variant::Variant::Chess),
                    )
                    .unwrap();
                let mut buf = Vec::new();
                let mut entry = entry;
                entry.pack_entire(&mut buf);
                let total: u64 =
                    crate::packed::EntryView::<crate::packed::PlayerGroupHeader>::unpack_entire(
                        &buf,
                    )
                    .cells()
                    .map(|cell| cell.stats.games())
                    .sum();
                assert_eq!(total, expected, "{user} {color}");
            }
        }
        let stats = self.mock.stats();
        assert_eq!(stats.violations, 0, "{stats:?}");
        assert!(
            stats.max_active_streams <= self.indexers as u64,
            "{stats:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_index_all() {
    let setup = setup(
        12,
        |p| [0, 1, 7, 300, 2500][p % 5],
        3,
        Faults::default(),
        IndexerConfig {
            chunk_games: 100,
            rebuild_min_games: 200,
            ..IndexerConfig::default()
        },
    )
    .await;
    setup.index_all().await;
    setup.check();
    let m = &setup.indexer.metrics;
    assert_eq!(
        m.runs_complete.load(std::sync::atomic::Ordering::Relaxed),
        12
    );
    assert!(
        setup
            .indexer
            .db()
            .counters
            .rebuilds
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 3
    );

    // Within the cooldown, no new runs.
    let ticket = setup.indexer.request(&setup.players[0]).await.unwrap();
    assert!(ticket.progress().done);
    assert_eq!(setup.mock.stats().max_active_streams, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_index_with_faults() {
    let setup = setup(
        8,
        |p| [50, 400, 1200, 3000][p % 4],
        2,
        Faults {
            stall: 0.2,
            stall_duration: Duration::from_millis(1500),
            disconnect: 0.3,
            server_error: 0.1,
            malformed: 0.001,
            ..Faults::default()
        },
        IndexerConfig {
            chunk_games: 50,
            max_run: Some(Duration::from_millis(100)),
            rebuild_min_games: 500,
            policy: RunPolicy {
                cooldown: 0,
                ..RunPolicy::default()
            },
            ..IndexerConfig::default()
        },
    )
    .await;
    setup.index_all().await;
    // Malformed lines lose their games, so request again until all are in
    // (the mock picks faults per request).
    for _ in 0..10 {
        let db = setup.indexer.db();
        let missing = setup.players.iter().zip(&setup.games).any(|(user, games)| {
            db.status(db.user_hash(user))
                .unwrap()
                .is_none_or(|s| (s.games as usize) < games.len())
        });
        if !missing {
            break;
        }
        // Games lost to malformed lines are behind the checkpoint: reset
        // the players to get them again.
        for (user, games) in setup.players.iter().zip(&setup.games) {
            if db
                .status(db.user_hash(user))
                .unwrap()
                .is_none_or(|s| (s.games as usize) < games.len())
            {
                let mut status = db.status(db.user_hash(user)).unwrap().unwrap();
                status.latest_created_at = 0;
                db.put_status(&status).unwrap();
            }
        }
        setup.index_all().await;
    }
    setup.check();
    let m = &setup.indexer.metrics;
    assert!(m.stream_retries.load(std::sync::atomic::Ordering::Relaxed) > 0);
    assert!(m.runs_requeued.load(std::sync::atomic::Ordering::Relaxed) > 0);
}
