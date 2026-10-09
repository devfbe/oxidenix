//! What procfs gives each client (docs/design/io-rings.md, "procfs over the
//! rings"): procfs is one service for every Linux server instance, so no
//! instance may take what the others need. Everything a client can make
//! procfs hold is bounded per client and charged before procfs takes it:
//!
//! - **Channels**: at most `CHANNELS_PER_INSTANCE` per instance (a client
//!   needs one, and a new one while its dead one is let go of) of
//!   `MAX_CHANNELS` in all; so one instance takes at most 2 of 64 and the
//!   others keep the rest. The instance is the kernel's word in the offer
//!   (`ring::channel::Offer::instance`), which no client can forge.
//! - **Grants a channel has mapped in procfs**: at most `GRANTS_PER_CHANNEL`
//!   and `GRANT_PAGES_PER_CHANNEL` pages (the Linux server grants one
//!   scratch buffer of 16 pages); `FORGET` and the channel's end give
//!   them back.
//! - **Memory per request**: a result is made in at most `MAX_RESULT`
//!   bytes, whatever buffer the request names (procfs answers one request
//!   at a time, so that is its working memory).
//! - **Requests**: the rings bound them; procfs takes a channel's requests
//!   only while its completion ring has room, and a bounded number per
//!   round from each channel in turn.

/// Channels attached at once, of all instances.
pub const MAX_CHANNELS: usize = 64;
/// Channels of one instance attached at once.
pub const CHANNELS_PER_INSTANCE: usize = 2;
/// Grants of one channel mapped at once, and their pages.
pub const GRANTS_PER_CHANNEL: usize = 16;
pub const GRANT_PAGES_PER_CHANNEL: u64 = 256;
/// The most bytes of one result (a directory listing's piece).
pub const MAX_RESULT: usize = 64 * 1024;

pub const ENOMEM: i64 = 12;
pub const ENOSPC: i64 = 28;

/// The channel slots and the instance holding each.
pub struct Channels {
    slots: [Option<u64>; MAX_CHANNELS],
}

impl Default for Channels {
    fn default() -> Channels {
        Channels { slots: [None; MAX_CHANNELS] }
    }
}

impl Channels {
    /// A slot for a channel of `instance`: ENOSPC if the instance holds its
    /// share already or every slot is taken.
    pub fn admit(&mut self, instance: u64) -> Result<usize, i64> {
        if self.held_by(instance) >= CHANNELS_PER_INSTANCE {
            return Err(ENOSPC);
        }
        let slot = self.slots.iter().position(Option::is_none).ok_or(ENOSPC)?;
        self.slots[slot] = Some(instance);
        Ok(slot)
    }

    /// Slot `slot` is free again.
    pub fn release(&mut self, slot: usize) {
        if let Some(s) = self.slots.get_mut(slot) {
            *s = None;
        }
    }

    /// How many slots `instance` holds.
    pub fn held_by(&self, instance: u64) -> usize {
        self.slots.iter().filter(|s| **s == Some(instance)).count()
    }
}

/// The grants one channel has mapped in procfs.
#[derive(Default)]
pub struct Grants {
    grants: usize,
    pages: u64,
}

impl Grants {
    /// Whether another grant may be mapped (checked before mapping it).
    pub fn may_map(&self) -> Result<(), i64> {
        if self.grants >= GRANTS_PER_CHANNEL {
            return Err(ENOMEM);
        }
        Ok(())
    }

    /// Charges a grant of `pages` pages just mapped: ENOMEM (the caller
    /// unmaps it) if the channel's pages would exceed their bound.
    pub fn charge(&mut self, pages: u64) -> Result<(), i64> {
        self.may_map()?;
        let total = self.pages.checked_add(pages).filter(|&t| t <= GRANT_PAGES_PER_CHANNEL).ok_or(ENOMEM)?;
        self.grants += 1;
        self.pages = total;
        Ok(())
    }

    /// A grant of `pages` pages is unmapped.
    pub fn uncharge(&mut self, pages: u64) {
        self.grants = self.grants.saturating_sub(1);
        self.pages = self.pages.saturating_sub(pages);
    }
}
