//! The EXI string table for one stream.
//!
//! Every string literal is added to the global partition and to the local partition of the
//! qualified name it appeared under. A later occurrence of a value is coded as a hit: first
//! the local partition (`uint 0`, then an index of `ceil(log2(size))` bits), else the global
//! one (`uint 1`, same index rule), else as a literal (`uint len + 2`, then the code points).
//! Hits leave the table unchanged. All of this was checked against EXIficient.

use core::mem::MaybeUninit;
use core::slice;

use crate::layout::ceil_log2;

/// Distinct strings per message. A message with more is refused, not truncated.
pub const CAPACITY: usize = 64;

pub struct Table<'s> {
    /// `(qname id, value)` in order of first appearance; the first `len` are initialized.
    /// Left uninitialized otherwise: a table is made for every message and most messages
    /// have a handful of strings, so filling 64 entries up front was a quarter of the time.
    entries: [MaybeUninit<(u8, &'s str)>; CAPACITY],
    len: usize,
    /// False for `valuePartitionCapacity = 0` (DIN SPEC 70121): values are never added, so every
    /// string is a literal.
    keep: bool,
}

impl Clone for Table<'_> {
    /// Copies the used entries only.
    fn clone(&self) -> Self {
        let mut copy = Self::new();
        // Entry by entry: a `copy_from_slice` of variable length is a call to `memmove`, and most
        // tables (a stream without value partition, a message with few strings) are empty or nearly.
        for i in 0..self.len {
            copy.entries[i] = self.entries[i];
        }
        copy.len = self.len;
        copy.keep = self.keep;
        copy
    }
}

pub enum Found {
    Local(u32, u32),
    Global(u32, u32),
    Miss,
}

impl<'s> Table<'s> {
    pub fn new() -> Self {
        // SAFETY: an array of `MaybeUninit` needs no initialization (the documented way to make
        // one; the repeat expression `[MaybeUninit::uninit(); N]` compiles to a memset).
        let entries = unsafe { MaybeUninit::<[MaybeUninit<(u8, &'s str)>; CAPACITY]>::uninit().assume_init() };
        Self { entries, len: 0, keep: true }
    }

    /// A table for an EXI stream with `valuePartitionCapacity` 0: nothing is ever stored.
    pub fn without_values() -> Self {
        Self { keep: false, ..Self::new() }
    }

    /// Number of values in the table.
    pub(crate) fn count(&self) -> usize {
        self.len
    }

    /// Forgets the values added after the first `count`.
    pub(crate) fn truncate(&mut self, count: usize) {
        self.len = self.len.min(count);
    }

    fn used(&self) -> &[(u8, &'s str)] {
        // SAFETY: `push` initializes entry `len - 1` before raising `len`, and `clone` copies
        // the initialized prefix, so the first `len` entries are initialized.
        unsafe { slice::from_raw_parts(self.entries.as_ptr().cast(), self.len) }
    }

    /// Looks `value` up for qname `q`: `(index, bits)` of a hit.
    pub fn find(&self, q: u8, value: &str) -> Found {
        let mut local_size = 0u64;
        let mut local = None;
        for (eq, ev) in self.used() {
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
        match self.used().iter().position(|(_, ev)| *ev == value) {
            Some(i) => Found::Global(i as u32, ceil_log2(self.len as u64)),
            None => Found::Miss,
        }
    }

    pub fn push(&mut self, q: u8, value: &'s str) -> bool {
        if !self.keep {
            return true;
        }
        if self.len == CAPACITY {
            return false;
        }
        self.entries[self.len] = MaybeUninit::new((q, value));
        self.len += 1;
        true
    }

    /// Bits for a local index when the partition of `q` currently has this many entries.
    pub fn local_bits(&self, q: u8) -> u32 {
        ceil_log2(self.used().iter().filter(|(eq, _)| *eq == q).count() as u64)
    }

    pub fn global_bits(&self) -> u32 {
        ceil_log2(self.len as u64)
    }

    /// The value at local index `i` of qname `q`.
    pub fn local(&self, q: u8, i: usize) -> Option<&'s str> {
        self.used().iter().filter(|(eq, _)| *eq == q).nth(i).map(|(_, v)| *v)
    }

    pub fn global(&self, i: usize) -> Option<&'s str> {
        self.used().get(i).map(|(_, v)| *v)
    }
}

impl Default for Table<'_> {
    fn default() -> Self {
        Self::new()
    }
}
