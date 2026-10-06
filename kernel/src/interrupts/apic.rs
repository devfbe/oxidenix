//! Local APIC (one per CPU: timer, end of interrupt, inter-processor
//! interrupts) and I/O APIC (routes device interrupts to CPUs). They replace
//! the 8259 PICs and the PIT, which only ever reach the bootstrap CPU.

use crate::drivers::acpi::{self, Override};
use crate::memory::{self, Caching};
use crate::sync::IrqSpinLock;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use x86_64::instructions::port::Port;

/// Vectors: the local timer, the I/O APIC lines (GSI n at IRQ_BASE + n)
/// and the spurious vector; IPIs are in `ipi`.
pub const TIMER_VECTOR: u8 = 0x20;
pub const IRQ_BASE: u8 = 0x30;
pub const GSI_COUNT: u32 = 24;
pub const SPURIOUS_VECTOR: u8 = 0xff;

// Local APIC registers (byte offsets).
const ID: usize = 0x20;
const TPR: usize = 0x80;
const EOI: usize = 0xb0;
const SVR: usize = 0xf0;
const LVT_TIMER: usize = 0x320;
const LVT_LINT0: usize = 0x350;
const LVT_LINT1: usize = 0x360;
const LVT_ERROR: usize = 0x370;
const TIMER_INITIAL: usize = 0x380;
const TIMER_CURRENT: usize = 0x390;
const TIMER_DIVIDE: usize = 0x3e0;

const SVR_ENABLE: u32 = 1 << 8;
const LVT_MASKED: u32 = 1 << 16;
const TIMER_PERIODIC: u32 = 1 << 17;

static LAPIC: AtomicU64 = AtomicU64::new(0);
/// Local APIC timer counts per scheduler tick (divider 16), measured once.
static TIMER_COUNT: AtomicU32 = AtomicU32::new(0);

fn lapic_read(reg: usize) -> u32 {
    unsafe { core::ptr::read_volatile((LAPIC.load(Ordering::Relaxed) as usize + reg) as *const u32) }
}

fn lapic_write(reg: usize, value: u32) {
    unsafe { core::ptr::write_volatile((LAPIC.load(Ordering::Relaxed) as usize + reg) as *mut u32, value) };
}

/// This CPU's local APIC id.
pub fn id() -> u8 {
    (lapic_read(ID) >> 24) as u8
}

pub fn eoi() {
    lapic_write(EOI, 0);
}

/// Inter-processor interrupts. They come into use when the other CPUs
/// start (step 5 in docs/design/smp.md).
#[allow(dead_code)]
pub mod ipi {
    use super::{lapic_read, lapic_write};

    pub const RESCHEDULE_VECTOR: u8 = 0xf0;
    pub const HALT_VECTOR: u8 = 0xf1;

    const ICR_LOW: usize = 0x300;
    const ICR_HIGH: usize = 0x310;
    const ICR_PENDING: u32 = 1 << 12;
    const ICR_LEVEL_ASSERT: u32 = 1 << 14;
    const ICR_INIT: u32 = 0b101 << 8;
    const ICR_STARTUP: u32 = 0b110 << 8;

    /// Sends an interrupt command and waits until the local APIC took it.
    fn send(apic_id: u8, low: u32) {
        lapic_write(ICR_HIGH, (apic_id as u32) << 24);
        lapic_write(ICR_LOW, low);
        while lapic_read(ICR_LOW) & ICR_PENDING != 0 {
            core::hint::spin_loop();
        }
    }

    pub fn send_vector(apic_id: u8, vector: u8) {
        send(apic_id, vector as u32);
    }

    /// INIT, then STARTUP: the target CPU begins in real mode at `page << 12`.
    pub fn send_init(apic_id: u8) {
        send(apic_id, ICR_INIT | ICR_LEVEL_ASSERT);
    }

    pub fn send_startup(apic_id: u8, page: u8) {
        send(apic_id, ICR_STARTUP | page as u32);
    }
}

/// Enables this CPU's local APIC and starts its periodic timer.
pub fn init_local() {
    lapic_write(TPR, 0);
    lapic_write(LVT_LINT0, LVT_MASKED);
    lapic_write(LVT_LINT1, LVT_MASKED);
    lapic_write(LVT_ERROR, LVT_MASKED);
    lapic_write(SVR, SVR_ENABLE | SPURIOUS_VECTOR as u32);
    lapic_write(TIMER_DIVIDE, 0b0011); // divide by 16
    lapic_write(LVT_TIMER, TIMER_PERIODIC | TIMER_VECTOR as u32);
    lapic_write(TIMER_INITIAL, TIMER_COUNT.load(Ordering::Relaxed));
}

/// Busy-waits about `us` microseconds with PIT channel 2 (no interrupts).
pub fn pit_delay_us(us: u64) {
    const PIT_HZ: u64 = 1_193_182;
    let mut remaining = us * PIT_HZ / 1_000_000;
    unsafe {
        let mut gate: Port<u8> = Port::new(0x61);
        let mut cmd: Port<u8> = Port::new(0x43);
        let mut ch2: Port<u8> = Port::new(0x42);
        while remaining > 0 {
            let chunk = remaining.min(0xffff) as u16;
            // Gate low, mode 0 (interrupt on terminal count), then gate high.
            let g = gate.read() & 0xfc;
            gate.write(g);
            cmd.write(0b1011_0000);
            ch2.write(chunk as u8);
            ch2.write((chunk >> 8) as u8);
            gate.write(g | 1);
            // OUT2 (bit 5) goes high at terminal count.
            while gate.read() & 0x20 == 0 {
                core::hint::spin_loop();
            }
            remaining -= chunk as u64;
        }
    }
}

/// Measures the local APIC timer against the PIT: counts per tick at `hz`.
fn calibrate(hz: u64) -> u32 {
    lapic_write(TIMER_DIVIDE, 0b0011);
    lapic_write(LVT_TIMER, LVT_MASKED);
    lapic_write(TIMER_INITIAL, u32::MAX);
    pit_delay_us(10_000);
    let elapsed = u32::MAX - lapic_read(TIMER_CURRENT);
    lapic_write(TIMER_INITIAL, 0);
    ((elapsed as u64 * 100 / hz) as u32).max(1)
}

/// The I/O APIC and the ISA overrides.
struct IoApic {
    base: usize,
    gsi_base: u32,
}

static IO_APIC: IrqSpinLock<IoApic> = IrqSpinLock::new(IoApic { base: 0, gsi_base: 0 });

impl IoApic {
    fn read(&self, reg: u32) -> u32 {
        unsafe {
            core::ptr::write_volatile(self.base as *mut u32, reg);
            core::ptr::read_volatile((self.base + 0x10) as *const u32)
        }
    }

    fn write(&self, reg: u32, value: u32) {
        unsafe {
            core::ptr::write_volatile(self.base as *mut u32, reg);
            core::ptr::write_volatile((self.base + 0x10) as *mut u32, value);
        }
    }
}

/// How an ISA interrupt reaches the I/O APIC (defaults: same number, edge,
/// active high).
fn isa_route(irq: u8) -> Override {
    acpi::madt()
        .overrides
        .iter()
        .find(|o| o.irq == irq)
        .copied()
        .unwrap_or(Override { irq, gsi: irq as u32, level_triggered: false, active_low: false })
}

/// The GSI an ISA (or PCI legacy) interrupt line arrives on.
pub fn gsi_of(irq: u8) -> u32 {
    isa_route(irq).gsi
}

/// Routes ISA interrupt `irq` to the bootstrap CPU, masked or not.
pub fn route_isa(irq: u8, masked: bool) {
    let route = isa_route(irq);
    let io = IO_APIC.lock();
    let Some(pin) = route.gsi.checked_sub(io.gsi_base).filter(|&p| p < GSI_COUNT) else { return };
    let mut low = (IRQ_BASE as u32 + route.gsi) & 0xff;
    if route.level_triggered {
        low |= 1 << 15;
    }
    if route.active_low {
        low |= 1 << 13;
    }
    if masked {
        low |= LVT_MASKED;
    }
    // Fixed delivery, physical destination: the bootstrap CPU.
    io.write(0x11 + 2 * pin, (BSP_APIC_ID.load(Ordering::Relaxed) as u32) << 24);
    io.write(0x10 + 2 * pin, low);
}

/// Masks or unmasks the I/O APIC pin of ISA interrupt `irq`.
pub fn set_masked(irq: u8, masked: bool) {
    let route = isa_route(irq);
    let io = IO_APIC.lock();
    let Some(pin) = route.gsi.checked_sub(io.gsi_base).filter(|&p| p < GSI_COUNT) else { return };
    let low = io.read(0x10 + 2 * pin);
    io.write(0x10 + 2 * pin, if masked { low | LVT_MASKED } else { low & !LVT_MASKED });
}

static BSP_APIC_ID: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Switches the bootstrap CPU from the PICs to the APICs. Every I/O APIC
/// pin starts masked; `route_isa` opens the ones in use.
pub fn init(hz: u64) -> Result<(), &'static str> {
    let madt = acpi::madt();
    let lapic = memory::map_physical(madt.local_apic, 4096, Caching::Uncached)?;
    LAPIC.store(lapic.as_u64(), Ordering::Relaxed);
    let io = memory::map_physical(madt.io_apic, 4096, Caching::Uncached)?;
    {
        let mut ioapic = IO_APIC.lock();
        ioapic.base = io.as_u64() as usize;
        ioapic.gsi_base = madt.io_apic_gsi_base;
        let pins = ((ioapic.read(1) >> 16) & 0xff) + 1;
        for pin in 0..pins.min(GSI_COUNT) {
            ioapic.write(0x10 + 2 * pin, LVT_MASKED | (IRQ_BASE as u32 + madt.io_apic_gsi_base + pin));
        }
    }
    disable_pics();
    BSP_APIC_ID.store(id(), Ordering::Relaxed);
    TIMER_COUNT.store(calibrate(hz), Ordering::Relaxed);
    init_local();
    Ok(())
}

/// Remaps the PICs away from the exception vectors and masks them for good.
fn disable_pics() {
    unsafe {
        let mut pics = pic8259::ChainedPics::new(0x50, 0x58);
        pics.initialize();
        pics.write_masks(0xff, 0xff);
    }
}
