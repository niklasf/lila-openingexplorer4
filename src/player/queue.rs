//! Queue of players to index: one ticket per player, shared by all requests
//! for the player, with progress notifications.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use tokio::sync::{Notify, watch};

use crate::player::UserId;

/// Progress of the indexing of a player, as seen by the requests waiting
/// for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// Position in the queue (1 for the next player), 0 while indexing.
    pub queue_position: u64,
    /// Incremented after each committed batch of new games.
    pub generation: u64,
    /// Indexing is complete.
    pub done: bool,
}

/// The progress of the indexing of a player.
#[derive(Clone)]
pub struct Ticket {
    rx: watch::Receiver<Progress>,
}

impl Ticket {
    /// A ticket for a player that does not need indexing.
    pub fn completed() -> Ticket {
        let (_, rx) = watch::channel(Progress {
            queue_position: 0,
            generation: 0,
            done: true,
        });
        Ticket { rx }
    }

    pub fn progress(&self) -> Progress {
        *self.rx.borrow()
    }

    /// Waits for the next change of the progress, returning it. Returns the
    /// last progress (as done) if the queue dropped the ticket.
    pub async fn changed(&mut self) -> Progress {
        match self.rx.changed().await {
            Ok(()) => *self.rx.borrow_and_update(),
            Err(_) => Progress {
                done: true,
                ..*self.rx.borrow()
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFull;

struct Entry {
    tx: Arc<watch::Sender<Progress>>,
    running: bool,
    /// Has an index already (an incremental run), queued before first runs.
    known: bool,
}

#[derive(Default)]
struct State {
    entries: HashMap<UserId, Entry>,
    waiting: VecDeque<UserId>,
    /// Players being indexed or rebuilt, who must not be indexed again
    /// meanwhile.
    busy: HashSet<UserId>,
}

impl State {
    /// Updates the queue positions of all waiting players.
    fn update_positions(&self) {
        for (i, user) in self.waiting.iter().enumerate() {
            if let Some(entry) = self.entries.get(user) {
                entry.tx.send_if_modified(|p| {
                    let position = i as u64 + 1;
                    let changed = p.queue_position != position;
                    p.queue_position = position;
                    changed
                });
            }
        }
    }
}

pub struct Queue {
    state: Mutex<State>,
    notify: Notify,
    capacity: usize,
    known_first: bool,
}

/// Counts of the queue, for `/monitor`.
#[derive(Debug, Clone, Copy)]
pub struct QueueStats {
    pub waiting: usize,
    pub running: usize,
    pub busy: usize,
}

impl Queue {
    pub fn new(capacity: usize, known_first: bool) -> Queue {
        Queue {
            state: Mutex::new(State::default()),
            notify: Notify::new(),
            capacity,
            known_first,
        }
    }

    pub fn stats(&self) -> QueueStats {
        let state = self.state.lock().expect("queue");
        QueueStats {
            waiting: state.waiting.len(),
            running: state.entries.values().filter(|e| e.running).count(),
            busy: state.busy.len(),
        }
    }

    /// The ticket of a player that is queued or being indexed.
    pub fn watch(&self, user: &UserId) -> Option<Ticket> {
        let state = self.state.lock().expect("queue");
        state.entries.get(user).map(|entry| Ticket {
            rx: entry.tx.subscribe(),
        })
    }

    /// Queues a player, or joins the ticket of the player if already
    /// queued. With `known_first`, players that are `known` (have an index
    /// already) are queued before the first waiting player that is not.
    pub fn submit(&self, user: UserId, known: bool) -> Result<Ticket, QueueFull> {
        let mut state = self.state.lock().expect("queue");
        if let Some(entry) = state.entries.get(&user) {
            return Ok(Ticket {
                rx: entry.tx.subscribe(),
            });
        }
        if state.waiting.len() >= self.capacity {
            return Err(QueueFull);
        }
        let (tx, rx) = watch::channel(Progress {
            queue_position: state.waiting.len() as u64 + 1,
            generation: 0,
            done: false,
        });
        state.entries.insert(
            user.clone(),
            Entry {
                tx: Arc::new(tx),
                running: false,
                known,
            },
        );
        let position = if known && self.known_first {
            state
                .waiting
                .iter()
                .position(|other| state.entries.get(other).is_some_and(|e| !e.known))
                .unwrap_or(state.waiting.len())
        } else {
            state.waiting.len()
        };
        state.waiting.insert(position, user);
        state.update_positions();
        drop(state);
        self.notify.notify_one();
        Ok(Ticket { rx })
    }

    fn try_acquire(self: &Arc<Self>) -> Option<QueueItem> {
        let mut state = self.state.lock().expect("queue");
        let mut i = 0;
        let mut acquired = None;
        while i < state.waiting.len() {
            let user = &state.waiting[i];
            let entry = state.entries.get(user).expect("entry of waiting player");
            if entry.tx.receiver_count() == 0 {
                // Nobody waits for it any more.
                let user = state.waiting.remove(i).expect("waiting player");
                state.entries.remove(&user);
                continue;
            }
            if state.busy.contains(user) {
                i += 1;
                continue;
            }
            let user = state.waiting.remove(i).expect("waiting player");
            let entry = state.entries.get_mut(&user).expect("entry");
            entry.running = true;
            entry.tx.send_modify(|p| p.queue_position = 0);
            let tx = Arc::clone(&entry.tx);
            state.busy.insert(user.clone());
            acquired = Some(QueueItem {
                user,
                tx,
                queue: Arc::clone(self),
                finished: false,
            });
            break;
        }
        state.update_positions();
        acquired
    }

    /// Waits for the next player to index.
    pub async fn acquire(self: &Arc<Self>) -> QueueItem {
        loop {
            let notified = self.notify.notified();
            if let Some(item) = self.try_acquire() {
                return item;
            }
            notified.await;
        }
    }
}

/// A player taken from the queue for indexing. Dropping it completes the
/// ticket.
pub struct QueueItem {
    user: UserId,
    tx: Arc<watch::Sender<Progress>>,
    queue: Arc<Queue>,
    finished: bool,
}

impl QueueItem {
    pub fn user(&self) -> &UserId {
        &self.user
    }

    /// A handle to notify the requests of committed batches.
    pub fn progress(&self) -> ProgressHandle {
        ProgressHandle {
            tx: Arc::clone(&self.tx),
        }
    }

    /// Whether no request waits for the player any more.
    pub fn abandoned(&self) -> bool {
        self.tx.receiver_count() == 0
    }

    /// Puts the player back at the end of the queue, to continue later.
    pub fn requeue(mut self) {
        self.finished = true;
        let mut state = self.queue.state.lock().expect("queue");
        state.busy.remove(&self.user);
        if let Some(entry) = state.entries.get_mut(&self.user) {
            entry.running = false;
            entry.known = true;
            state.waiting.push_back(self.user.clone());
        }
        state.update_positions();
        drop(state);
        self.queue.notify.notify_one();
    }

    /// Completes the ticket, but keeps the player busy (not indexed again)
    /// until the returned guard is dropped.
    pub fn finish_busy(mut self) -> BusyGuard {
        self.finished = true;
        self.complete(false);
        BusyGuard {
            user: self.user.clone(),
            queue: Arc::clone(&self.queue),
        }
    }

    fn complete(&self, release: bool) {
        let mut state = self.queue.state.lock().expect("queue");
        state.entries.remove(&self.user);
        if release {
            state.busy.remove(&self.user);
        }
        self.tx.send_modify(|p| {
            p.done = true;
            p.queue_position = 0;
        });
        drop(state);
        if release {
            self.queue.notify.notify_one();
        }
    }
}

impl Drop for QueueItem {
    fn drop(&mut self) {
        if !self.finished {
            self.complete(true);
        }
    }
}

/// Keeps a player busy, so that it is not indexed (for rebuilds).
pub struct BusyGuard {
    user: UserId,
    queue: Arc<Queue>,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.queue
            .state
            .lock()
            .expect("queue")
            .busy
            .remove(&self.user);
        self.queue.notify.notify_one();
    }
}

#[derive(Clone)]
pub struct ProgressHandle {
    tx: Arc<watch::Sender<Progress>>,
}

impl ProgressHandle {
    /// Notifies that a batch was committed.
    pub fn committed(&self) {
        self.tx.send_modify(|p| p.generation += 1);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn user(name: &str) -> UserId {
        UserId::from_name(name).unwrap()
    }

    #[tokio::test]
    async fn test_queue() {
        let queue = Arc::new(Queue::new(2, false));
        let a = queue.submit(user("a"), false).unwrap();
        let b = queue.submit(user("b"), false).unwrap();
        assert_eq!(queue.submit(user("c"), false).err(), Some(QueueFull));
        let a2 = queue.submit(user("a"), false).unwrap();
        assert_eq!(a.progress().queue_position, 1);
        assert_eq!(b.progress().queue_position, 2);

        let item = queue.acquire().await;
        assert_eq!(item.user(), &user("a"));
        assert_eq!(a2.progress().queue_position, 0);
        assert_eq!(b.progress().queue_position, 1);
        item.progress().committed();
        assert_eq!(a.progress().generation, 1);

        // Requeued behind b.
        item.requeue();
        assert_eq!(a.progress().queue_position, 2);
        assert_eq!(b.progress().queue_position, 1);

        // Abandoned tickets are dropped.
        drop(b);
        let item = queue.acquire().await;
        assert_eq!(item.user(), &user("a"));
        let busy = item.finish_busy();
        assert!(a.progress().done);
        assert!(queue.watch(&user("a")).is_none());

        // Busy players wait.
        let mut a3 = queue.submit(user("a"), false).unwrap();
        let waiting = tokio::time::timeout(Duration::from_millis(50), queue.acquire()).await;
        assert!(waiting.is_err());
        drop(busy);
        let item = queue.acquire().await;
        assert_eq!(item.user(), &user("a"));
        drop(item);
        assert!(a3.changed().await.done);
        assert_eq!(queue.stats().busy, 0);
    }

    #[tokio::test]
    async fn test_known_first() {
        let queue = Arc::new(Queue::new(10, true));
        let a = queue.submit(user("a"), false).unwrap();
        let b = queue.submit(user("b"), true).unwrap();
        let c = queue.submit(user("c"), false).unwrap();
        let d = queue.submit(user("d"), true).unwrap();
        let order: Vec<u64> = [&a, &b, &c, &d]
            .iter()
            .map(|t| t.progress().queue_position)
            .collect();
        assert_eq!(order, [3, 1, 4, 2]);
    }
}
