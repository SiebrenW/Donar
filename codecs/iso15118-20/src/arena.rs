//! Bump arena over a host-provided byte buffer. Messages are only needed for one protocol
//! step; the host calls [`Arena::reset`] afterwards and the memory is reused.

use core::cell::Cell;
use core::marker::PhantomData;
use core::{mem, slice};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaFull;

pub struct Arena<'a> {
    base: *mut u8,
    cap: usize,
    used: Cell<usize>,
    _buf: PhantomData<&'a mut [u8]>,
}

impl<'a> Arena<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { base: buf.as_mut_ptr(), cap: buf.len(), used: Cell::new(0), _buf: PhantomData }
    }

    /// Invalidates everything allocated so far. Needs `&mut self`, so the borrow checker
    /// guarantees no allocation is still referenced.
    pub fn reset(&mut self) {
        self.used.set(0);
    }

    pub fn used(&self) -> usize {
        self.used.get()
    }

    /// Allocates `n` elements, each produced by `f(index)`. `f` may itself allocate from
    /// this arena (the outer region is reserved first). On error nothing stays allocated.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc_slice_with<T: Copy, E: From<ArenaFull>>(
        &self,
        n: usize,
        mut f: impl FnMut(usize) -> Result<T, E>,
    ) -> Result<&mut [T], E> {
        let before = self.used.get();
        let addr = self.base as usize + before;
        let pad = addr.wrapping_neg() & (mem::align_of::<T>() - 1);
        let start = before.checked_add(pad).ok_or(ArenaFull)?;
        let end = n
            .checked_mul(mem::size_of::<T>())
            .and_then(|bytes| start.checked_add(bytes))
            .filter(|&end| end <= self.cap)
            .ok_or(ArenaFull)?;
        self.used.set(end);
        // SAFETY: `start..end` lies inside the buffer (checked above), is aligned for `T`,
        // and is disjoint from every earlier allocation because `used` only grows until
        // `reset(&mut self)`. The buffer bytes are initialized, and `T: Copy` means
        // overwriting them needs no drop.
        let p = unsafe { self.base.add(start) }.cast::<T>();
        for i in 0..n {
            match f(i) {
                // SAFETY: `i < n`, so the write is inside the region reserved above.
                Ok(v) => unsafe { p.add(i).write(v) },
                Err(e) => {
                    self.used.set(before);
                    return Err(e);
                }
            }
        }
        // SAFETY: all `n` elements were initialized above; the region is exclusively ours.
        Ok(unsafe { slice::from_raw_parts_mut(p, n) })
    }

    #[allow(clippy::mut_from_ref)]
    pub fn alloc_bytes(&self, n: usize) -> Result<&mut [u8], ArenaFull> {
        self.alloc_slice_with(n, |_| Ok::<u8, ArenaFull>(0))
    }
}
