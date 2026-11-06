use crate::player::status::PlayerStatus;

/// When to index a player.
#[derive(Debug, Clone, Copy)]
pub struct RunPolicy {
    /// No new index run within this time after the last one (ms).
    pub cooldown: u64,
    /// Ongoing games are revisited at most this often (ms).
    pub revisit_interval: u64,
}

impl Default for RunPolicy {
    fn default() -> RunPolicy {
        RunPolicy {
            cooldown: 2 * 60 * 1000,
            revisit_interval: 24 * 60 * 60 * 1000,
        }
    }
}

/// An index run: request games created since a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRun {
    /// Request games with `createdAt >= since`.
    pub since: u64,
    /// Revisits ongoing games (and also gets all newer games).
    pub revisit: bool,
}

impl RunPolicy {
    /// The run to do now, if any.
    pub fn next_run(&self, status: Option<&PlayerStatus>, now: u64) -> Option<IndexRun> {
        let Some(status) = status else {
            return Some(IndexRun {
                since: 0,
                revisit: false,
            });
        };
        if now.saturating_sub(status.indexed_at) < self.cooldown {
            return None;
        }
        if let Some(ongoing) = status.revisit_ongoing
            && now.saturating_sub(status.revisited_at) >= self.revisit_interval
        {
            return Some(IndexRun {
                since: ongoing,
                revisit: true,
            });
        }
        Some(IndexRun {
            // Including the latest millisecond, for games created in the
            // same millisecond after the last run. The games received again
            // are known and skipped.
            since: status.latest_created_at,
            revisit: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::UserId;

    #[test]
    fn test_next_run() {
        let policy = RunPolicy::default();
        let alice = UserId::from_name("alice").unwrap();
        assert_eq!(
            policy.next_run(None, 0),
            Some(IndexRun {
                since: 0,
                revisit: false
            })
        );
        let mut status = PlayerStatus::new(alice);
        status.games = 10;
        status.latest_created_at = 1000;
        let t = 100_000_000_000;
        status.indexed_at = t;
        assert_eq!(policy.next_run(Some(&status), t + 60_000), None);
        assert_eq!(
            policy.next_run(Some(&status), t + 120_000),
            Some(IndexRun {
                since: 1000,
                revisit: false
            })
        );
        status.revisit_ongoing = Some(500);
        assert_eq!(
            policy.next_run(Some(&status), t + 120_000),
            Some(IndexRun {
                since: 500,
                revisit: true
            })
        );
        status.revisited_at = t;
        assert_eq!(
            policy.next_run(Some(&status), t + 120_000),
            Some(IndexRun {
                since: 1000,
                revisit: false
            })
        );
    }
}
