use std::collections::{HashMap, VecDeque};

use gpui::Entity;
use metor_proto::types::ComponentId;

/// Materializes one live `Entity<V>` per [`ComponentId`] on demand and keeps a
/// bounded set alive across renders.
///
/// Views paint through gpui's virtualized `uniform_list`, but each row's data
/// subscription — a streaming entity that spawns a task and registers a WAL
/// reader — is expensive. This cache makes subscriptions track the *visible*
/// rows instead of every component: hosts create entities lazily for the rows
/// they paint and let the LRU drop ones scrolled far out of view. Keying by
/// `ComponentId` also means navigating away and back reuses the live entity,
/// so revisiting a node is instant rather than re-spawning its watcher.
pub struct VisibleEntityCache<V: 'static> {
    entities: HashMap<ComponentId, Entity<V>>,
    /// Least-recently-used at the front, most-recently-used at the back.
    lru: VecDeque<ComponentId>,
    cap: usize,
}

impl<V: 'static> VisibleEntityCache<V> {
    /// `cap` bounds how many entities stay alive; size it well above the
    /// visible row count so off-screen-but-nearby rows survive a scroll.
    pub fn new(cap: usize) -> Self {
        Self {
            entities: HashMap::new(),
            lru: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Entity for `id`, created via `make` on a miss. Marks `id` as
    /// most-recently-used. Call [`prune`](Self::prune) once after the render
    /// pass to evict entries that aged out.
    pub fn get_or_create(&mut self, id: ComponentId, make: impl FnOnce() -> Entity<V>) -> Entity<V> {
        let entity = match self.entities.get(&id) {
            Some(entity) => entity.clone(),
            None => {
                let entity = make();
                self.entities.insert(id, entity.clone());
                entity
            }
        };
        self.touch(id);
        entity
    }

    fn touch(&mut self, id: ComponentId) {
        if let Some(pos) = self.lru.iter().position(|c| *c == id) {
            self.lru.remove(pos);
        }
        self.lru.push_back(id);
    }

    /// Drop the least-recently-used entities beyond the cap, releasing their
    /// tasks and WAL readers. The rows touched this frame sit at the back of
    /// the LRU, so they survive as long as `cap` exceeds the visible count.
    pub fn prune(&mut self) {
        while self.lru.len() > self.cap {
            if let Some(id) = self.lru.pop_front() {
                self.entities.remove(&id);
            }
        }
    }
}
