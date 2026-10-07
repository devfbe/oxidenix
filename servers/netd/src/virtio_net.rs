//! virtio-net driver on the shared virtio transport (crates/virtio).
//!
//! Every descriptor points at its own fixed packet buffer in the DMA area,
//! so a descriptor index identifies a buffer.

use virtio::{Device, Dma, Queue, DESC_F_WRITE, PAGE};

const FEATURE_MAC: u32 = 1 << 5;
/// The MAC address in the device configuration.
const CONFIG_MAC: u16 = 0;

const RX: u16 = 0;
const TX: u16 = 1;

/// Buffers per queue (the device's queue may be larger; the rest stays unused).
const BUFFERS: usize = 64;
const BUFFER_SIZE: usize = 2048;
/// The legacy `virtio_net_hdr` in front of every frame (no mergeable buffers).
const NET_HDR: usize = 10;
pub const MTU: usize = 1514;

/// A queue with one fixed buffer per descriptor.
struct Buffers {
    queue: Queue,
    virt: *mut u8,
    phys: u64,
}

impl Buffers {
    fn new(queue: Queue, dma: &mut Dma) -> Result<Buffers, &'static str> {
        if queue.len() < BUFFERS {
            return Err("virtqueue too small");
        }
        let (virt, phys) = dma.alloc(BUFFERS * BUFFER_SIZE, PAGE)?;
        Ok(Buffers { queue, virt, phys })
    }

    fn set(&mut self, i: usize, len: usize, flags: u16) {
        self.queue.set_desc(i, self.phys + (i * BUFFER_SIZE) as u64, len as u32, flags, None);
    }

    fn buffer(&mut self, i: usize) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.virt.add(i * BUFFER_SIZE), BUFFER_SIZE) }
    }
}

pub struct VirtioNet {
    device: Device,
    rx: Buffers,
    tx: Buffers,
    /// Transmit descriptors not in use by the device.
    tx_free: alloc::vec::Vec<usize>,
    pub mac: [u8; 6],
}

impl VirtioNet {
    /// Resets and sets up the device at I/O port `io`; `dma`/`phys` is the
    /// DMA area.
    pub fn new(io: u16, dma: *mut u8, phys: u64, dma_len: usize) -> Result<VirtioNet, &'static str> {
        let (device, _) = unsafe { Device::new(io, 0, FEATURE_MAC) }?;
        let mut dma = unsafe { Dma::new(dma, phys, dma_len) };
        let rx = device.queue(RX, BUFFERS, &mut dma)?;
        let mut rx = Buffers::new(rx, &mut dma)?;
        let tx = device.queue(TX, BUFFERS, &mut dma)?;
        let tx = Buffers::new(tx, &mut dma)?;
        // Every receive buffer goes to the device right away.
        for i in 0..BUFFERS {
            rx.set(i, BUFFER_SIZE, DESC_F_WRITE);
            rx.queue.push_avail(i);
        }
        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = device.config8(CONFIG_MAC + i as u16);
        }
        device.driver_ok();
        device.notify(RX);
        Ok(VirtioNet { device, rx, tx, tx_free: (0..BUFFERS).collect(), mac })
    }

    /// Acknowledges an interrupt (reading the ISR clears it).
    pub fn ack_interrupt(&mut self) {
        self.device.ack_interrupt();
    }

    /// Passes the next received frame to `f`, then returns the buffer to
    /// the device.
    pub fn receive<R>(&mut self, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let (i, len) = self.rx.queue.pop_used()?;
        let len = len.clamp(NET_HDR, BUFFER_SIZE);
        let result = f(&self.rx.buffer(i)[NET_HDR..len]);
        self.rx.queue.push_avail(i);
        self.device.notify(RX);
        Some(result)
    }

    pub fn has_received(&mut self) -> bool {
        self.rx.queue.has_used()
    }

    /// Takes back transmit buffers the device has sent.
    fn reclaim(&mut self) {
        while let Some((i, _)) = self.tx.queue.pop_used() {
            self.tx_free.push(i);
        }
    }

    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        !self.tx_free.is_empty()
    }

    /// Sends a frame of `len` bytes that `f` fills in.
    pub fn send<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        self.reclaim();
        let i = self.tx_free.pop().expect("can_send checked before");
        let len = len.min(MTU);
        let buf = self.tx.buffer(i);
        buf[..NET_HDR].fill(0);
        let result = f(&mut buf[NET_HDR..NET_HDR + len]);
        self.tx.set(i, NET_HDR + len, 0);
        self.tx.queue.push_avail(i);
        self.device.notify(TX);
        result
    }
}
