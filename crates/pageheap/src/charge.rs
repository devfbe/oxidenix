//! The kernel's account of one heap area's commitment (`SYS_SHARED_COMMIT`,
//! `SYS_SHARED_DECOMMIT`): its mapped pages and its reserve (commitment held
//! beyond them, so that a server can allocate when the commit limit is
//! reached), and a pool bounding the reserves of all instances together.
//! Pure arithmetic, host-tested: the kernel keeps a `Charge` under the
//! area's lock and calls the commit limit only with the amounts it returns.
//!
//! **Invariant.** What the area holds of the commit limit is exactly
//! `mapped + reserve`, `reserve <= target`, and `reserve` pages are taken
//! from the `Pool` (so the reserves of all areas together never exceed its
//! bound). Every method keeps it: each says how many pages the caller must
//! commit (charge) or uncommit (refund), and nothing else changes the
//! commit limit's count. No subtraction can underflow: a refund is never
//! more than was charged (an unmap of more pages than are mapped is refused,
//! `Error`), so dropping the area refunds `charged()` and the books balance.

use core::sync::atomic::{AtomicU64, Ordering};

/// The reserves of all areas together: at most `max` pages.
pub struct Pool {
    used: AtomicU64,
    max: AtomicU64,
}

impl Pool {
    pub const fn new() -> Pool {
        Pool { used: AtomicU64::new(0), max: AtomicU64::new(0) }
    }

    /// Sets the bound (pages); reserves taken beyond a lower one stay until
    /// given back.
    pub fn set_max(&self, max: u64) {
        self.max.store(max, Ordering::Relaxed);
    }

    /// Up to `n` pages of the pool; how many.
    pub fn take(&self, n: u64) -> u64 {
        let mut granted = 0;
        let _ = self.used.try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            granted = n.min(self.max.load(Ordering::Relaxed).saturating_sub(used));
            Some(used + granted)
        });
        granted
    }

    /// Gives back `n` pages taken before.
    pub fn give(&self, n: u64) {
        let old = self.used.fetch_sub(n, Ordering::AcqRel);
        assert!(old >= n, "pool: more given back than taken");
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
}

impl Default for Pool {
    fn default() -> Pool {
        Pool::new()
    }
}

/// An unmap reported more pages than are mapped (a kernel bug: the
/// heap area is mapped only through `Charge`).
#[derive(Debug, PartialEq, Eq)]
pub struct Error;

/// A commit of `missing` pages, planned by `Charge::begin`: `reserve` of
/// them come from the reserve, `fresh` must be committed by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub missing: u64,
    pub reserve: u64,
    pub fresh: u64,
}

#[derive(Debug, Default)]
pub struct Charge {
    mapped: u64,
    reserve: u64,
}

impl Charge {
    pub const fn new() -> Charge {
        Charge { mapped: 0, reserve: 0 }
    }

    /// What the area holds of the commit limit.
    pub fn charged(&self) -> u64 {
        self.mapped + self.reserve
    }

    pub fn mapped(&self) -> u64 {
        self.mapped
    }

    pub fn reserve(&self) -> u64 {
        self.reserve
    }

    /// Pages the reserve lacks of `target`.
    pub fn deficit(&self, target: u64) -> u64 {
        target.saturating_sub(self.reserve)
    }

    /// Grows the reserve by `n` pages the caller just committed and took
    /// from `pool` (at most the deficit).
    pub fn grow_reserve(&mut self, n: u64, target: u64) {
        assert!(n <= self.deficit(target), "reserve beyond its target");
        self.reserve += n;
    }

    /// Plans mapping `missing` pages: from the reserve first; the rest
    /// (`fresh`) the caller commits. The reserve pages are spoken for until
    /// `end` (or `abort`).
    pub fn begin(&mut self, missing: u64, pool: &Pool) -> Plan {
        let reserve = missing.min(self.reserve);
        self.reserve -= reserve;
        pool.give(reserve);
        Plan { missing, reserve, fresh: missing - reserve }
    }

    /// The fresh commit failed: the reserve gets back what was taken (as
    /// far as the pool lets; what it does not is returned: the pages to
    /// uncommit).
    pub fn abort(&mut self, plan: Plan, pool: &Pool, target: u64) -> u64 {
        self.refill(plan.reserve, pool, target)
    }

    /// `done` of the plan's pages were mapped (all were charged): the rest
    /// of the charge goes to the reserve as far as it and the pool allow;
    /// returns the pages to uncommit.
    pub fn end(&mut self, plan: Plan, done: u64, pool: &Pool, target: u64) -> u64 {
        assert!(done <= plan.missing, "more mapped than planned");
        self.mapped += done;
        self.refill(plan.missing - done, pool, target)
    }

    /// `gone` mapped pages were unmapped: their charge refills the reserve
    /// first; returns the pages to uncommit. Refused (nothing changed) if
    /// fewer are mapped.
    pub fn unmapped(&mut self, gone: u64, pool: &Pool, target: u64) -> Result<u64, Error> {
        self.mapped = self.mapped.checked_sub(gone).ok_or(Error)?;
        Ok(self.refill(gone, pool, target))
    }

    /// `pages` of this area's charge come back: as reserve up to the target
    /// and as the pool grants, the rest to be uncommitted (returned).
    fn refill(&mut self, pages: u64, pool: &Pool, target: u64) -> u64 {
        let keep = pool.take(pages.min(self.deficit(target)));
        self.reserve += keep;
        pages - keep
    }

    /// The area goes: everything it holds is to be uncommitted (returned),
    /// its reserve given back to the pool.
    pub fn close(&mut self, pool: &Pool) -> u64 {
        let all = self.charged();
        pool.give(self.reserve);
        *self = Charge::new();
        all
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    /// The commit limit's count as the kernel keeps it for one area.
    struct Books {
        committed: u64,
    }

    #[test]
    fn every_path_balances() {
        let pool = Pool::new();
        pool.set_max(600);
        let (mut a, mut b) = (Charge::new(), Charge::new());
        let mut books = Books { committed: 0 };
        let target = 512;
        // Area a gets its reserve; b only what is left of the pool.
        for c in [&mut a, &mut b] {
            let want = pool.take(c.deficit(target));
            books.committed += want;
            c.grow_reserve(want, target);
        }
        assert_eq!((a.reserve(), b.reserve(), pool.used()), (512, 88, 600));
        // A commit within the reserve: nothing fresh.
        let plan = a.begin(100, &pool);
        assert_eq!(plan.fresh, 0);
        books.committed = books.committed.checked_sub(a.end(plan, 100, &pool, target)).expect("refund beyond charge");
        assert_eq!((a.mapped(), a.reserve()), (100, 412));
        // Beyond it, half mapped: the rest refills the reserve.
        let plan = a.begin(1000, &pool);
        assert_eq!((plan.reserve, plan.fresh), (412, 588));
        books.committed += plan.fresh;
        books.committed = books.committed.checked_sub(a.end(plan, 500, &pool, target)).expect("refund beyond charge");
        assert_eq!(a.charged(), books.committed - b.charged());
        // A failed fresh commit: the reserve back.
        let plan = b.begin(200, &pool);
        books.committed = books.committed.checked_sub(b.abort(plan, &pool, target)).expect("refund beyond charge");
        assert_eq!(b.reserve(), 88);
        // Unmaps refill, then refund; too many are refused.
        books.committed = books.committed.checked_sub(a.unmapped(600, &pool, target).unwrap()).expect("refund beyond charge");
        assert_eq!(a.unmapped(1, &pool, target), Err(Error));
        assert_eq!(a.charged() + b.charged(), books.committed);
        assert!(pool.used() <= 600);
        books.committed = books.committed.checked_sub(a.close(&pool)).expect("refund beyond charge");
        books.committed = books.committed.checked_sub(b.close(&pool)).expect("refund beyond charge");
        assert_eq!((books.committed, pool.used()), (0, 0));
    }

    #[test]
    fn random_operations_never_lose_a_page() {
        let pool = Pool::new();
        pool.set_max(1000);
        let target = 512;
        let mut areas: std::vec::Vec<Charge> = (0..4).map(|_| Charge::new()).collect();
        let mut committed = 0u64;
        let mut seed = 0x1234_5678_9abc_def0u64;
        for _ in 0..100_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let a = &mut areas[(seed % 4) as usize];
            let n = (seed >> 8) % 700;
            match (seed >> 4) % 4 {
                0 => {
                    let got = pool.take(n.min(a.deficit(target)));
                    committed += got;
                    a.grow_reserve(got, target);
                }
                1 => {
                    let plan = a.begin(n, &pool);
                    if (seed >> 20) % 3 == 0 {
                        committed = committed.checked_sub(a.abort(plan, &pool, target)).expect("refund beyond charge");
                    } else {
                        committed += plan.fresh;
                        let done = (seed >> 24) % (n + 1);
                        committed = committed.checked_sub(a.end(plan, done, &pool, target)).expect("refund beyond charge");
                    }
                }
                2 => match a.unmapped(n, &pool, target) {
                    Ok(back) => committed = committed.checked_sub(back).expect("refund beyond charge"),
                    Err(Error) => assert!(n > a.mapped()),
                },
                _ => committed = committed.checked_sub(a.close(&pool)).expect("refund beyond charge"),
            }
            assert_eq!(areas.iter().map(Charge::charged).sum::<u64>(), committed);
            assert_eq!(areas.iter().map(Charge::reserve).sum::<u64>(), pool.used());
            assert!(pool.used() <= 1000 && areas.iter().all(|a| a.reserve() <= target));
        }
    }
}
