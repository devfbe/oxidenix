//! The controller's rings (xHCI 1.2, 4.9): a producer ring (the command
//! ring, each endpoint's transfer ring), one page of TRBs closed by a Link
//! TRB back to its start, and the event ring the controller produces
//! into (one segment, one page).

use super::trb::{self, Trb};
use crate::dma::{self, Buf, Dma, PAGE};

/// TRBs per ring page; the last is the Link TRB.
pub const TRBS: usize = PAGE / trb::BYTES;

/// A ring software produces into.
pub struct Ring {
    buf: Buf,
    enqueue: usize,
    cycle: bool,
    /// TRBs of TDs not completed yet (padding included).
    used: usize,
}

impl Ring {
    pub fn new(dma: &mut Dma) -> Option<Ring> {
        let mut buf = dma.alloc(1)?;
        let link = Trb::link(buf.phys());
        write(&mut buf, TRBS - 1, link.with_cycle(false));
        Some(Ring { buf, enqueue: 0, cycle: true, used: 0 })
    }

    /// The ring's start (with cycle state 1: where the controller begins).
    pub fn base(&self) -> u64 {
        self.buf.phys()
    }

    /// Where the next TD goes and its cycle state (Set TR Dequeue Pointer
    /// to skip what is queued).
    pub fn enqueue_pointer(&self) -> (u64, bool) {
        (self.buf.phys() + (self.enqueue * trb::BYTES) as u64, self.cycle)
    }

    /// Room for a TD of `n` TRBs (the TRBs left before the link may go to
    /// padding).
    pub fn has_room(&self, n: usize) -> bool {
        let before_link = TRBS - 1 - self.enqueue;
        let need = if n <= before_link { n } else { before_link + n };
        self.used + need <= TRBS - 1
    }

    /// Puts a TD in: its TRBs (the cycle bits are the ring's) one after the
    /// other, never across the link (the rest of the page is filled with
    /// no-ops first). The first TRB's cycle bit is written last, so the
    /// controller sees the TD whole or not at all. Returns the device
    /// addresses of the TD's first and last TRBs and its TRBs counted
    /// (padding included), or None without room.
    pub fn push(&mut self, td: &[Trb]) -> Option<(u64, u64, usize)> {
        if td.is_empty() || td.len() > TRBS - 1 || !self.has_room(td.len()) {
            return None;
        }
        let mut taken = 0;
        if TRBS - 1 - self.enqueue < td.len() {
            while self.enqueue < TRBS - 1 {
                // (Only transfer rings pad: a command is one TRB, which
                // always fits before the link.)
                let noop = Trb([0, 0, 0, trb::NOOP << 10]);
                let at = self.enqueue;
                write(&mut self.buf, at, noop.with_cycle(self.cycle));
                self.enqueue += 1;
                taken += 1;
            }
            self.wrap();
        }
        let first = self.enqueue;
        for (i, t) in td.iter().enumerate() {
            // The first one with the wrong cycle bit for now.
            let cycle = if i == 0 { !self.cycle } else { self.cycle };
            write(&mut self.buf, first + i, t.with_cycle(cycle));
        }
        dma::release();
        write(&mut self.buf, first, td[0].with_cycle(self.cycle));
        dma::release();
        self.enqueue += td.len();
        taken += td.len();
        let last = self.buf.phys() + ((first + td.len() - 1) * trb::BYTES) as u64;
        if self.enqueue == TRBS - 1 {
            self.wrap();
        }
        self.used += taken;
        Some((self.buf.phys() + (first * trb::BYTES) as u64, last, taken))
    }

    /// The Link TRB passes the controller to the start, the cycle toggles.
    fn wrap(&mut self) {
        let link = Trb::link(self.buf.phys()).with_cycle(self.cycle);
        write(&mut self.buf, TRBS - 1, link);
        dma::release();
        self.enqueue = 0;
        self.cycle = !self.cycle;
    }

    /// A TD of `n` TRBs (as `push` counted them) is done.
    pub fn completed(&mut self, n: usize) {
        self.used = self.used.saturating_sub(n);
    }

    /// Everything queued was dropped (after Set TR Dequeue Pointer to the
    /// enqueue pointer).
    pub fn reset_used(&mut self) {
        self.used = 0;
    }

    pub fn free(self, dma: &mut Dma) {
        dma.free(self.buf);
    }
}

fn write(buf: &mut Buf, index: usize, t: Trb) {
    // Dwords 0-2 first, the control dword (with the cycle bit) last.
    for (i, &w) in t.0.iter().enumerate() {
        buf.write32(index * trb::BYTES + 4 * i, w);
    }
}

fn read(buf: &Buf, index: usize) -> Trb {
    Trb([0, 1, 2, 3].map(|i| buf.read32(index * trb::BYTES + 4 * i)))
}

/// The event ring of interrupter 0: one segment, and its segment table.
pub struct EventRing {
    buf: Buf,
    table: Buf,
    dequeue: usize,
    cycle: bool,
}

impl EventRing {
    pub fn new(dma: &mut Dma) -> Option<EventRing> {
        let buf = dma.alloc(1)?;
        let mut table = dma.alloc(1)?;
        // One entry: the segment's address and size in TRBs.
        table.write64(0, buf.phys());
        table.write32(8, TRBS as u32);
        dma::release();
        Some(EventRing { buf, table, dequeue: 0, cycle: true })
    }

    pub fn table(&self) -> u64 {
        self.table.phys()
    }

    /// The software dequeue pointer, for ERDP.
    pub fn dequeue_pointer(&self) -> u64 {
        self.buf.phys() + (self.dequeue * trb::BYTES) as u64
    }

    /// The next event, if the controller wrote one.
    pub fn next(&mut self) -> Option<Trb> {
        let t = read(&self.buf, self.dequeue);
        if t.cycle() != self.cycle {
            return None;
        }
        dma::acquire();
        // (Read again after the fence: the cycle bit came last.)
        let t = read(&self.buf, self.dequeue);
        self.dequeue += 1;
        if self.dequeue == TRBS {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        Some(t)
    }
}
