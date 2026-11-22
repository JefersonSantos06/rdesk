//! The host's half of the client's tile cache ([`rdesk_proto::TileData::Cached`]).
//!
//! The host decides which slot keeps what, so both sides always agree: a tile sent losslessly
//! may be stored in a slot, and a slot is only referred to once the frame that stored it was
//! acknowledged. A slot that a frame in flight refers to is never reused until that frame
//! lands or is lost.

use std::collections::HashMap;

use rdesk_common::tiles::TileHash;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Free,
    /// Stored by a frame that is not acknowledged yet.
    Pending(TileHash),
    Ready(TileHash),
}

#[derive(Clone, Copy)]
struct Slot {
    state: State,
    /// Frames in flight that refer to this slot.
    refs: u32,
}

pub struct TileCache {
    slots: Vec<Slot>,
    ready: HashMap<TileHash, u32>,
    pending: HashMap<TileHash, u32>,
    /// Per frame in flight: slots it stores, slots it refers to.
    stores: HashMap<u64, Vec<u32>>,
    uses: HashMap<u64, Vec<u32>>,
    /// Where the search for a slot to reuse starts (first in, first out).
    hand: usize,
}

impl TileCache {
    pub fn new(slots: u32) -> Self {
        Self {
            slots: vec![Slot { state: State::Free, refs: 0 }; slots.min(1 << 16) as usize],
            ready: HashMap::new(),
            pending: HashMap::new(),
            stores: HashMap::new(),
            uses: HashMap::new(),
            hand: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        !self.slots.is_empty()
    }

    /// The slot that holds `hash` on the client, taken for frame `frame`.
    pub fn lookup(&mut self, hash: &TileHash, frame: u64) -> Option<u32> {
        let s = *self.ready.get(hash)?;
        self.slots[s as usize].refs += 1;
        self.uses.entry(frame).or_default().push(s);
        Some(s)
    }

    /// A slot for frame `frame` to store `hash` in, if it is worth it (not stored already)
    /// and one is free to take.
    pub fn store(&mut self, hash: &TileHash, frame: u64) -> Option<u32> {
        if self.slots.is_empty() || self.ready.contains_key(hash) || self.pending.contains_key(hash) {
            return None;
        }
        let n = self.slots.len();
        let s = (0..n).map(|k| (self.hand + k) % n).find(|&i| {
            let slot = &self.slots[i];
            slot.refs == 0 && !matches!(slot.state, State::Pending(_))
        })?;
        self.hand = (s + 1) % n;
        if let State::Ready(old) = self.slots[s].state {
            self.ready.remove(&old);
        }
        self.slots[s].state = State::Pending(*hash);
        self.pending.insert(*hash, s as u32);
        self.stores.entry(frame).or_default().push(s as u32);
        Some(s as u32)
    }

    /// The client applied frame `frame`: what it stored is there.
    pub fn acked(&mut self, frame: u64) {
        for s in self.stores.remove(&frame).unwrap_or_default() {
            if let State::Pending(h) = self.slots[s as usize].state {
                self.pending.remove(&h);
                self.slots[s as usize].state = State::Ready(h);
                self.ready.insert(h, s);
            }
        }
        self.release(frame);
    }

    /// Frame `frame` will not be applied (or may not have been): what it stored is unknown.
    pub fn lost(&mut self, frame: u64) {
        for s in self.stores.remove(&frame).unwrap_or_default() {
            if let State::Pending(h) = self.slots[s as usize].state {
                self.pending.remove(&h);
                self.slots[s as usize].state = State::Free;
            }
        }
        self.release(frame);
    }

    fn release(&mut self, frame: u64) {
        for s in self.uses.remove(&frame).unwrap_or_default() {
            let r = &mut self.slots[s as usize].refs;
            *r = r.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(v: u8) -> TileHash {
        [v; 16]
    }

    #[test]
    fn a_slot_is_used_only_once_acknowledged() {
        let mut c = TileCache::new(2);
        assert_eq!(c.store(&h(1), 1), Some(0));
        assert_eq!(c.lookup(&h(1), 2), None, "frame 1 still in flight");
        assert_eq!(c.store(&h(1), 2), None, "already being stored");
        c.acked(1);
        assert_eq!(c.lookup(&h(1), 3), Some(0));
    }

    #[test]
    fn lost_frames_free_their_slots() {
        let mut c = TileCache::new(1);
        assert_eq!(c.store(&h(1), 1), Some(0));
        c.lost(1);
        assert_eq!(c.lookup(&h(1), 2), None);
        assert_eq!(c.store(&h(2), 2), Some(0));
    }

    #[test]
    fn slots_in_use_are_not_reused() {
        let mut c = TileCache::new(2);
        c.store(&h(1), 1);
        c.store(&h(2), 1);
        c.acked(1);
        // Frame 2 refers to slot 0; storing a third tile must take slot 1.
        assert_eq!(c.lookup(&h(1), 2), Some(0));
        assert_eq!(c.store(&h(3), 2), Some(1));
        assert_eq!(c.lookup(&h(2), 3), None, "slot 1 was given to another tile");
        // Every slot is busy now.
        assert_eq!(c.store(&h(4), 3), None);
        c.acked(2);
        assert_eq!(c.lookup(&h(3), 4), Some(1));
        assert_eq!(c.store(&h(4), 4), Some(0), "slot 0 is free of references again");
    }

    #[test]
    fn disabled_cache_stores_nothing() {
        let mut c = TileCache::new(0);
        assert!(!c.enabled());
        assert_eq!(c.store(&h(1), 1), None);
    }
}
