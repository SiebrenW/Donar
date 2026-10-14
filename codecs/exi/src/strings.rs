//! The EXI string table for one stream.
//!
//! Every string literal is added to the global partition and to the local partition of the
//! qualified name it appeared under. A later occurrence of a value is coded as a hit: first
//! the local partition (`uint 0`, then an index of `ceil(log2(size))` bits), else the global
//! one (`uint 1`, same index rule), else as a literal (`uint len + 2`, then the code points).
//! Hits leave the table unchanged. All of this was checked against EXIficient.

use crate::layout::ceil_log2;

/// Distinct strings per message. A message with more is refused, not truncated.
pub const CAPACITY: usize = 64;

#[derive(Clone)]
pub struct Table<'s> {
    /// `(qname id, value)` in order of first appearance.
    entries: [(u8, &'s str); CAPACITY],
    len: usize,
}

pub enum Found {
    Local(u32, u32),
    Global(u32, u32),
    Miss,
}

impl<'s> Table<'s> {
    pub fn new() -> Self {
        Self { entries: [(0, ""); CAPACITY], len: 0 }
    }

    /// Looks `value` up for qname `q`: `(index, bits)` of a hit.
    pub fn find(&self, q: u8, value: &str) -> Found {
        let mut local_size = 0u64;
        let mut local = None;
        for (eq, ev) in &self.entries[..self.len] {
            if *eq == q {
                if local.is_none() && *ev == value {
                    local = Some(local_size);
                }
                local_size += 1;
            }
        }
        // The local index is the rank among entries of this qname; its width needs the final size.
        if let Some(index) = local {
            return Found::Local(index as u32, ceil_log2(local_size));
        }
        match self.entries[..self.len].iter().position(|(_, ev)| *ev == value) {
            Some(i) => Found::Global(i as u32, ceil_log2(self.len as u64)),
            None => Found::Miss,
        }
    }

    pub fn push(&mut self, q: u8, value: &'s str) -> bool {
        if self.len == CAPACITY {
            return false;
        }
        self.entries[self.len] = (q, value);
        self.len += 1;
        true
    }

    /// Bits for a local index when the partition of `q` currently has this many entries.
    pub fn local_bits(&self, q: u8) -> u32 {
        ceil_log2(self.entries[..self.len].iter().filter(|(eq, _)| *eq == q).count() as u64)
    }

    pub fn global_bits(&self) -> u32 {
        ceil_log2(self.len as u64)
    }

    /// The value at local index `i` of qname `q`.
    pub fn local(&self, q: u8, i: usize) -> Option<&'s str> {
        self.entries[..self.len].iter().filter(|(eq, _)| *eq == q).nth(i).map(|(_, v)| *v)
    }

    pub fn global(&self, i: usize) -> Option<&'s str> {
        self.entries[..self.len].get(i).map(|(_, v)| *v)
    }
}

impl Default for Table<'_> {
    fn default() -> Self {
        Self::new()
    }
}
