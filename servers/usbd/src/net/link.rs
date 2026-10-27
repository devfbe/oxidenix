//! A network function's link to netd (the link protocol, `linkring`):
//! the channel usbd makes and offers netd, its frame rings and doorbells.

use core::sync::atomic::{AtomicU32, Ordering};
use linkring::{Area, Consumer, Info, Producer};
use ring::channel::{Layout, CLIENT_GONE, SERVICE_GONE, STATE_OFFSET};

/// Doorbells are futex wakes of the word (netd's event loop, or ours,
/// sleeps on the counter through the kernel's watch).
pub struct Futex;

impl ring::Wait for Futex {
    fn wait(&self, word: &AtomicU32, value: u32) {
        let _ = oxrt::futex_wait(word, value, None);
    }

    fn wake(&self, word: &AtomicU32) {
        let _ = oxrt::futex_wake(word, u32::MAX);
    }
}

pub struct Link {
    handle: u64,
    base: *mut u8,
    pub area: Area,
    /// Frames from the wire, into netd.
    pub rx: Producer,
    /// Frames to the wire, from netd.
    pub tx: Consumer,
}

impl Link {
    /// A new channel describing the link as `info`, offered to netd; the
    /// errno if it cannot be made or netd refuses it.
    pub fn open(info: &Info) -> Result<Link, i64> {
        let layout = Layout::with_shared(linkring::SLOTS, linkring::SHARED_PAGES).expect("the link protocol's layout is valid");
        let (handle, base) = oxrt::chan_create(linkring::SLOTS, linkring::SHARED_PAGES)?;
        let area = unsafe { Area::new(base, &layout) }.expect("a channel of the link protocol's layout");
        area.init(info);
        if let Err(e) = oxrt::chan_connect(handle, linkring::SERVICE) {
            let _ = oxrt::chan_close(handle);
            return Err(e);
        }
        let (rx, tx) = (area.rx_producer(), area.tx_consumer());
        Ok(Link { handle, base, area, rx, tx })
    }

    /// Whether netd let go of the link (it died or detached it): the
    /// channel is dead, a new one must be offered.
    pub fn gone(&self) -> bool {
        let state = unsafe { &*(self.base.add(STATE_OFFSET) as *const AtomicU32) };
        state.load(Ordering::Acquire) & (SERVICE_GONE | CLIENT_GONE) != 0
    }

    /// Arms the doorbell watch for a sleep: false if netd put frames in
    /// meanwhile (or the watch cannot be armed: do not sleep then).
    pub fn prepare_sleep(&self) -> bool {
        let Some(value) = self.area.prepare_driver_sleep(&self.tx) else { return false };
        matches!(oxrt::chan_client_watch(self.handle, value), Ok(true))
    }

    pub fn awake(&self) {
        self.area.driver_awake();
    }

    pub fn close(self) {
        let _ = oxrt::chan_close(self.handle);
    }
}
