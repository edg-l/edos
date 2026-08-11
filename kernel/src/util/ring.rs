//! A growable byte FIFO.
//!
//! Every character device in the kernel holds bytes that arrive at one end and
//! leave at the other: a pipe, and both directions of a PTY. A `Vec` serves
//! that shape only by memmoving everything still queued on every read, and by
//! allocating a fresh `Vec` to hand the drained bytes back in. A ring does
//! neither: each end costs the bytes it actually moves, and the buffer is kept
//! across calls, so two processes passing single bytes settle on one allocation
//! and never make another.

use alloc::vec::Vec;

#[derive(Debug)]
pub struct ByteRing {
    /// The ring. Its length *is* the capacity; the live bytes are the `len`
    /// starting at `head`, wrapping around the end.
    ring: Vec<u8>,
    head: usize,
    len: usize,
}

impl ByteRing {
    /// First allocation. A page, so a stream carrying a byte at a time never
    /// grows again and one carrying a `BufWriter`'s output grows twice.
    const MIN_CAPACITY: usize = 4096;

    pub const fn new() -> Self {
        Self {
            ring: Vec::new(),
            head: 0,
            len: 0,
        }
    }

    /// Live under `sched-test`, where the ring's own check reads it.
    #[cfg_attr(not(feature = "sched-test"), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn capacity(&self) -> usize {
        self.ring.len()
    }

    /// Make room for `extra` more bytes, re-laying the live ones out from zero.
    fn grow_for(&mut self, extra: usize) {
        let needed = self.len + extra;
        if needed <= self.capacity() {
            return;
        }
        let mut capacity = self.capacity().max(Self::MIN_CAPACITY);
        while capacity < needed {
            capacity *= 2;
        }
        let mut next = alloc::vec![0u8; capacity];
        let (front, back) = self.slices();
        next[..front.len()].copy_from_slice(front);
        next[front.len()..front.len() + back.len()].copy_from_slice(back);
        self.ring = next;
        self.head = 0;
    }

    /// The live bytes in order, as the one or two runs the ring holds them in.
    fn slices(&self) -> (&[u8], &[u8]) {
        if self.len == 0 {
            return (&[], &[]);
        }
        let end = self.head + self.len;
        if end <= self.capacity() {
            (&self.ring[self.head..end], &[])
        } else {
            (&self.ring[self.head..], &self.ring[..end - self.capacity()])
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.grow_for(data.len());
        let capacity = self.capacity();
        let tail = (self.head + self.len) % capacity;
        let first = data.len().min(capacity - tail);
        self.ring[tail..tail + first].copy_from_slice(&data[..first]);
        if first < data.len() {
            self.ring[..data.len() - first].copy_from_slice(&data[first..]);
        }
        self.len += data.len();
    }

    pub fn push_byte(&mut self, byte: u8) {
        self.push(&[byte]);
    }

    /// Move up to `out.len()` bytes out of the ring, returning how many.
    pub fn pop(&mut self, out: &mut [u8]) -> usize {
        let taken = out.len().min(self.len);
        if taken == 0 {
            return 0;
        }
        let capacity = self.capacity();
        let first = taken.min(capacity - self.head);
        out[..first].copy_from_slice(&self.ring[self.head..self.head + first]);
        if first < taken {
            out[first..taken].copy_from_slice(&self.ring[..taken - first]);
        }
        self.head = (self.head + taken) % capacity;
        self.len -= taken;
        // A drained ring restarts at zero, which keeps the next write in one
        // run rather than split across the end.
        if self.len == 0 {
            self.head = 0;
        }
        taken
    }
}

#[cfg(feature = "sched-test")]
pub mod tests {
    use super::ByteRing;

    /// Exercise the ring where its arithmetic can be wrong: a read that leaves
    /// the head ahead of zero, a write that wraps around the end, and a growth
    /// that has to linearise a wrapped ring. Panics on the first disagreement,
    /// which is how every test in this kernel reports a failure.
    pub fn check() {
        let mut ring = ByteRing::new();
        let mut out = [0u8; 8];

        // Empty: a read takes nothing and gives nothing.
        assert!(ring.is_empty(), "a fresh ring is not empty");
        assert!(ring.pop(&mut out) == 0, "an empty ring gave bytes");

        // A short read leaves the rest behind, in order.
        ring.push(b"abcdef");
        assert!(ring.len() == 6, "six bytes in, {} queued", ring.len());
        assert!(
            ring.pop(&mut out[..2]) == 2 && &out[..2] == b"ab",
            "short read"
        );
        assert!(
            ring.pop(&mut out) == 4 && &out[..4] == b"cdef",
            "remainder after a short read"
        );
        assert!(ring.is_empty(), "ring not empty after draining it");

        // A ring still holding bytes when a write reaches its end wraps around
        // it. Capacity is a page, so leave 100 bytes live near the top and
        // write past them.
        let mut sink = alloc::vec![0u8; 6000];
        ring.push(&[b'a'; 4000]);
        assert!(ring.pop(&mut sink[..3900]) == 3900, "drain to the top");
        ring.push(&[b'b'; 300]);
        let taken = ring.pop(&mut sink);
        assert!(taken == 400, "a wrapped ring held {taken} of 400 bytes");
        assert!(
            sink[..100].iter().all(|b| *b == b'a') && sink[100..400].iter().all(|b| *b == b'b'),
            "a wrapped write came back out of order"
        );
        assert!(ring.is_empty(), "ring not empty after a wrapped read");

        // Growing while wrapped has to re-lay the live bytes out in order.
        ring.push(&[b'a'; 4000]);
        ring.pop(&mut sink[..3900]);
        ring.push(&[b'b'; 300]);
        ring.push(&[b'c'; 5000]);
        let taken = ring.pop(&mut sink);
        assert!(taken == 5400, "after growing while wrapped, {taken} bytes");
        assert!(
            sink[..100].iter().all(|b| *b == b'a')
                && sink[100..400].iter().all(|b| *b == b'b')
                && sink[400..5400].iter().all(|b| *b == b'c'),
            "growing a wrapped ring reordered its bytes"
        );
        assert!(ring.is_empty(), "ring not empty at the end");
    }
}
