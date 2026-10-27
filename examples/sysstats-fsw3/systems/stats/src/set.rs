//! [`Set`], named devices tracked across samples.

use crate::sample::segment;

/// One device as last sampled: its counters now and at the previous sample,
/// and per-device state `X`.
#[derive(Debug)]
pub struct Entry<C, X> {
    /// The name the OS uses.
    pub name: String,
    /// `name` as a map key, with `.` replaced by `_`.
    pub key: String,
    pub now: C,
    pub prev: C,
    live: bool,
    pub extra: X,
}

/// Devices matched by name across samples. A device missing from a sample
/// is dropped.
#[derive(Debug)]
pub struct Set<C, X> {
    entries: Vec<Entry<C, X>>,
}

impl<C: Copy, X: Default> Set<C, X> {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            entries: Vec::with_capacity(n),
        }
    }

    /// Marks every device unseen before a sample.
    pub fn begin(&mut self) {
        self.entries.iter_mut().for_each(|e| e.live = false);
    }

    /// Records `now` for `name`, adding it if it is new.
    pub fn update(&mut self, name: &str, now: C) -> &mut Entry<C, X> {
        let at = self.entries.iter().position(|e| e.name == name);
        let at = at.unwrap_or_else(|| {
            self.entries.push(entry(name, now));
            self.entries.len() - 1
        });
        let e = &mut self.entries[at];
        e.prev = e.now;
        e.now = now;
        e.live = true;
        e
    }

    /// Drops every device the sample did not see.
    pub fn end(&mut self) {
        self.entries.retain(|e| e.live);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry<C, X>> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn entry<C: Copy, X: Default>(name: &str, now: C) -> Entry<C, X> {
    let mut key = String::new();
    if segment(name, &mut key).is_none() {
        key.push('_');
    }
    Entry {
        name: name.into(),
        key,
        now,
        prev: now,
        live: false,
        extra: X::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracks_by_name() {
        let mut set = Set::<u64, ()>::with_capacity(4);
        set.begin();
        set.update("eth0.100", 100);
        set.update("lo", 5);
        set.end();
        set.begin();
        set.update("eth0.100", 300);
        set.end();
        let entries: Vec<_> = set.iter().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "eth0_100");
        assert_eq!((entries[0].prev, entries[0].now), (100, 300));
    }

    #[test]
    fn test_new_entry_starts_from_now() {
        let mut set = Set::<u64, ()>::with_capacity(1);
        set.update("en0", 1000);
        let e = set.iter().next().expect("added");
        assert_eq!((e.prev, e.now), (1000, 1000));
        set.update("", 1);
        assert_eq!(set.iter().nth(1).map(|e| e.key.as_str()), Some("_"));
    }
}
