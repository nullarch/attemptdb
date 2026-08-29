//! Write-optimised in-memory table of recent, already-durable events.

use attemptdb_core::{Event, EventId};
use std::collections::HashSet;

#[derive(Default)]
pub struct MemTable {
    events: Vec<Event>,
    ids: HashSet<EventId>,
    approx_bytes: usize,
}

impl MemTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, ev: Event, encoded_len: usize) {
        self.ids.insert(ev.event_id);
        self.approx_bytes += encoded_len;
        self.events.push(ev);
    }

    pub fn contains(&self, id: &EventId) -> bool {
        self.ids.contains(id)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn approx_bytes(&self) -> usize {
        self.approx_bytes
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn drain(&mut self) -> Vec<Event> {
        self.ids.clear();
        self.approx_bytes = 0;
        std::mem::take(&mut self.events)
    }
}
