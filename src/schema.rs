use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parent {
    Code,
    Language,
    TabSize,
}

#[derive(Clone, Copy)]
struct Span {
    end: u32,
    parent: Parent,
}

/// Provenance survives deletion/GC so references cannot change a root or map key.
/// Entries are bounded by the room's accepted update-block budget and coalesced.
#[derive(Default)]
pub struct Schema {
    spans: BTreeMap<(u32, u32), Span>,
    clocks: BTreeMap<u32, u32>,
}

impl Schema {
    pub fn clock(&self, client: u32) -> u32 {
        self.clocks.get(&client).copied().unwrap_or(0)
    }

    pub fn advance(&mut self, client: u32, end: u32) {
        let clock = self.clocks.entry(client).or_default();
        *clock = (*clock).max(end);
    }

    pub fn parent(&self, id: (u32, u32)) -> Option<Parent> {
        self.spans
            .range(..=id)
            .next_back()
            .filter(|(key, span)| key.0 == id.0 && span.end > id.1)
            .map(|(_, span)| span.parent)
    }

    pub fn compatible(&self, id: (u32, u32), end: u32, parent: Parent) -> bool {
        self.spans
            .range(..=id)
            .next_back()
            .is_none_or(|(key, span)| key.0 != id.0 || span.end <= id.1 || span.parent == parent)
            && self
                .spans
                .range(id..(id.0, end))
                .all(|(_, span)| span.parent == parent)
    }

    pub fn insert(&mut self, id: (u32, u32), mut end: u32, parent: Parent) {
        let mut start = id.1;
        if let Some((&key, &span)) = self.spans.range(..=id).next_back()
            && key.0 == id.0
            && span.end >= start
            && span.parent == parent
        {
            start = key.1;
            end = end.max(span.end);
            self.spans.remove(&key);
        }
        while let Some((&key, &span)) = self.spans.range((id.0, start)..=(id.0, end)).next() {
            if span.parent != parent {
                break;
            }
            end = end.max(span.end);
            self.spans.remove(&key);
        }
        self.spans.insert((id.0, start), Span { end, parent });
    }

    pub fn extend(&mut self, other: Self) {
        for (client, end) in other.clocks {
            self.advance(client, end);
        }
        for (id, span) in other.spans {
            self.insert(id, span.end, span.parent);
        }
    }
}
