//! Local APIC (one per CPU: timer, end of interrupt, inter-processor
//! interrupts) and I/O APIC (routes device interrupts to CPUs). They replace
//! the 8259 PICs and the PIT, which only ever reach the bootstrap CPU.
//!
//! The local APIC runs in x2APIC mode where the CPU has it (CPUID 1 ECX bit
//! 21), and always when the firmware handed it over in that mode
//! (IA32_APIC_BASE bit 10; there is no way back but through disabling it):
//! its registers are MSRs (0x800 + offset / 16), the interrupt command is one
//! 64-bit MSR write with a 32-bit destination id, and the ids are 32 bits
//! wide. Otherwise it is the xAPIC, whose registers are memory-mapped. All
//! CPUs use the bootstrap CPU's choice. The I/O APIC is programmed with
//! physical 8-bit destinations, so it reaches the bootstrap CPU only while
//! that CPU's id is below 255 (beyond, interrupt remapping would be needed).

use crate::drivers::acpi::{self, Override};
use crate::memory::{self, Caching};
use crate::sync::IrqSpinLock;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use x86_64::registers::model_specific::Msr;

/// Vectors: the local timer, the I/O APIC lines (GSI n at IRQ_BASE + n),
/// the devices' message-signalled interrupts (MSI_BASE + n, `MSI_COUNT` of
/// them: `process::irq`) and the spurious vector; IPIs are in `ipi`.
pub const TIMER_VECTOR: u8 = 0x20;
pub const IRQ_BASE: u8 = 0x30;
pub const GSI_COUNT: u32 = 24;
pub const MSI_BASE: u8 = 0x60;
pub const MSI_COUNT: u8 = 16;
pub const SPURIOUS_VECTOR: u8 = 0xff;

// Local APIC registers (byte offsets in the xAPIC's page; the x2APIC's MSR
// is 0x800 + offset / 16).
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
const TIMER_TSC_DEADLINE: u32 = 0b10 << 17;
const MSR_TSC_DEADLINE: u32 = 0x6e0;

const IA32_APIC_BASE: u32 = 0x1b;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const X2APIC_MSR_BASE: u32 = 0x800;
/// CPUID 1 ECX: the CPU has the x2APIC.
const CPUID_X2APIC: u32 = 1 << 21;

/// How the local APICs are driven, chosen once on the bootstrap CPU.
const MODE_OFF: u8 = 0;
const MODE_XAPIC: u8 = 1;
const MODE_X2APIC: u8 = 2;
static MODE: AtomicU8 = AtomicU8::new(MODE_OFF);
/// The xAPIC's registers (virtual address; unused in x2APIC mode).
static LAPIC: AtomicU64 = AtomicU64::new(0);

fn x2apic() -> bool {
    MODE.load(Ordering::Relaxed) == MODE_X2APIC
}

/// Whether the local APICs run in x2APIC mode (32-bit ids reachable).
pub fn is_x2apic() -> bool {
    x2apic()
}

fn x2apic_msr(reg: usize) -> Msr {
    Msr::new(X2APIC_MSR_BASE + (reg >> 4) as u32)
}

fn lapic_read(reg: usize) -> u32 {
    if x2apic() {
        unsafe { x2apic_msr(reg).read() as u32 }
    } else {
        unsafe { core::ptr::read_volatile((LAPIC.load(Ordering::Relaxed) as usize + reg) as *const u32) }
    }
}

fn lapic_write(reg: usize, value: u32) {
    if x2apic() {
        unsafe { x2apic_msr(reg).write(value as u64) };
    } else {
        unsafe { core::ptr::write_volatile((LAPIC.load(Ordering::Relaxed) as usize + reg) as *mut u32, value) };
    }
}

/// This CPU's local APIC id (8 bits in xAPIC mode, 32 in x2APIC mode).
pub fn id() -> u32 {
    if x2apic() { lapic_read(ID) } else { lapic_read(ID) >> 24 }
}

pub fn eoi() {
    lapic_write(EOI, 0);
}

/// Whether the APICs are set up (`init` ran).
fn is_up() -> bool {
    MODE.load(Ordering::Relaxed) != MODE_OFF
}

/// Inter-processor interrupts: start-up, rescheduling and halting.
pub mod ipi {
    use super::{lapic_read, lapic_write, x2apic, Msr, X2APIC_MSR_BASE};

    pub const RESCHEDULE_VECTOR: u8 = 0xf0;
    pub const HALT_VECTOR: u8 = 0xf1;
    pub const TLB_VECTOR: u8 = 0xf2;

    const ICR_LOW: usize = 0x300;
    const ICR_HIGH: usize = 0x310;
    const ICR_PENDING: u32 = 1 << 12;
    const ICR_LEVEL_ASSERT: u32 = 1 << 14;
    const ICR_INIT: u32 = 0b101 << 8;
    const ICR_STARTUP: u32 = 0b110 << 8;

    /// Sends an interrupt command. In x2APIC mode it is one MSR write (the
    /// destination in the upper half); that write does not wait for earlier
    /// stores, so a fence first makes what the target is told about (a
    /// queued task, a TLB request) visible before the interrupt arrives. In
    /// xAPIC mode it is two register writes, then a wait until the local
    /// APIC took it; interrupts stay off meanwhile, as a handler sending an
    /// IPI between the writes would change the destination.
    fn send(apic_id: u32, low: u32) {
        if x2apic() {
            unsafe {
                core::arch::asm!("mfence", "lfence", options(nostack, preserves_flags));
                Msr::new(X2APIC_MSR_BASE + (ICR_LOW >> 4) as u32).write((apic_id as u64) << 32 | low as u64);
            }
            return;
        }
        debug_assert!(apic_id <= 0xff, "xAPIC destination {apic_id} out of range");
        x86_64::instructions::interrupts::without_interrupts(|| {
            lapic_write(ICR_HIGH, apic_id << 24);
            lapic_write(ICR_LOW, low);
            while lapic_read(ICR_LOW) & ICR_PENDING != 0 {
                core::hint::spin_loop();
            }
        })
    }

    pub fn send_vector(apic_id: u32, vector: u8) {
        send(apic_id, vector as u32);
    }

    /// Stops every other CPU (for a panic). Does nothing before the APIC
    /// is up.
    pub fn halt_others() {
        const ALL_EXCLUDING_SELF: u32 = 0b11 << 18;
        if super::is_up() {
            send(0, ALL_EXCLUDING_SELF | HALT_VECTOR as u32);
        }
    }

    /// INIT, then STARTUP: the target CPU begins in real mode at `page << 12`.
    pub fn send_init(apic_id: u32) {
        send(apic_id, ICR_INIT | ICR_LEVEL_ASSERT);
    }

    pub fn send_startup(apic_id: u32, page: u8) {
        send(apic_id, ICR_STARTUP | page as u32);
    }
}

/// Puts this CPU's local APIC into the chosen mode (enabled in
/// IA32_APIC_BASE; from disabled, x2APIC mode is reached through xAPIC).
fn enable_mode() {
    let mut base = Msr::new(IA32_APIC_BASE);
    let mut value = unsafe { base.read() };
    if value & APIC_BASE_ENABLE == 0 {
        value |= APIC_BASE_ENABLE;
        unsafe { base.write(value) };
    }
    if x2apic() && value & APIC_BASE_X2APIC == 0 {
        unsafe { base.write(value | APIC_BASE_X2APIC) };
    }
}

/// Enables this CPU's local APIC in the chosen mode; its timer stays off
/// until `timer` programs it.
pub fn init_local() {
    enable_mode();
    lapic_write(TPR, 0);
    lapic_write(LVT_LINT0, LVT_MASKED);
    lapic_write(LVT_LINT1, LVT_MASKED);
    lapic_write(LVT_ERROR, LVT_MASKED);
    lapic_write(SVR, SVR_ENABLE | SPURIOUS_VECTOR as u32);
    lapic_write(TIMER_DIVIDE, 0b0011); // divide by 16
    lapic_write(LVT_TIMER, LVT_MASKED | TIMER_VECTOR as u32);
}

/// Measures this CPU's local APIC timer (one-shot, divider 16) against the
/// TSC over 10 ms: counts per second. Needs `time::init`; leaves the timer
/// masked.
pub fn measure_timer() -> u64 {
    lapic_write(TIMER_DIVIDE, 0b0011);
    lapic_write(LVT_TIMER, LVT_MASKED | TIMER_VECTOR as u32);
    let start = crate::time::now();
    lapic_write(TIMER_INITIAL, u32::MAX);
    crate::time::delay_us(10_000);
    let elapsed = u32::MAX - lapic_read(TIMER_CURRENT);
    let ns = crate::time::now() - start;
    lapic_write(TIMER_INITIAL, 0);
    ((elapsed as u128 * crate::time::NSEC_PER_SEC as u128 / ns.max(1) as u128) as u64).max(1)
}

/// Puts this CPU's timer into TSC-deadline or one-shot mode, unmasked.
pub fn set_timer_mode(tsc_deadline: bool) {
    lapic_write(LVT_TIMER, if tsc_deadline { TIMER_TSC_DEADLINE } else { 0 } | TIMER_VECTOR as u32);
    // The mode must be in effect before the deadline MSR is written.
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
}

/// TSC-deadline mode: interrupt when this CPU's TSC reaches `tsc`.
pub fn set_tsc_deadline(tsc: u64) {
    unsafe { Msr::new(MSR_TSC_DEADLINE).write(tsc) };
}

/// One-shot mode: interrupt after `count` timer counts.
pub fn set_oneshot_count(count: u32) {
    lapic_write(TIMER_INITIAL, count);
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
    // Fixed delivery, physical destination: the bootstrap CPU (8 bits; see
    // `init` for an id beyond).
    io.write(0x11 + 2 * pin, (BSP_APIC_ID.load(Ordering::Relaxed) & 0xff) << 24);
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

static BSP_APIC_ID: AtomicU32 = AtomicU32::new(0);

/// The bootstrap CPU's APIC id: where device interrupts go.
pub fn bsp_id() -> u32 {
    BSP_APIC_ID.load(Ordering::Relaxed)
}

/// Chooses the local APIC's mode and switches the bootstrap CPU from the
/// PICs to the APICs. Every I/O APIC pin starts masked; `route_isa` opens
/// the ones in use.
pub fn init() -> Result<(), &'static str> {
    let madt = acpi::madt();
    let handed_over = unsafe { Msr::new(IA32_APIC_BASE).read() } & APIC_BASE_X2APIC != 0;
    let supported = core::arch::x86_64::__cpuid(1).ecx & CPUID_X2APIC != 0;
    if handed_over || supported {
        MODE.store(MODE_X2APIC, Ordering::Relaxed);
    } else {
        let lapic = memory::map_physical(madt.local_apic, 4096, Caching::Uncached)?;
        LAPIC.store(lapic.as_u64(), Ordering::Relaxed);
        MODE.store(MODE_XAPIC, Ordering::Relaxed);
    }
    // At once: from here on a panic's IPI uses the chosen mode.
    enable_mode();
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
    if madt.has_pics {
        disable_pics();
    }
    init_local();
    let id = id();
    BSP_APIC_ID.store(id, Ordering::Relaxed);
    crate::smp::set_apic_id(id);
    crate::printkln!(
        "[apic] {}",
        match (handed_over, supported) {
            (true, _) => "x2APIC mode (handed over by the firmware)",
            (false, true) => "x2APIC mode (switched from xAPIC)",
            (false, false) => "xAPIC mode (the CPU has no x2APIC)",
        }
    );
    if id > 0xfe {
        crate::printkln!("[apic] the bootstrap CPU's id {} is beyond the I/O APIC's reach: no device interrupts", id);
    }
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
