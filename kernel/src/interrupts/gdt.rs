//! Per-CPU GDT and TSS. Every CPU needs its own TSS (kernel stack for
//! interrupts from ring 3, I/O permission bitmap, double-fault stack) and
//! therefore its own GDT with a descriptor for it. The segment layout is the
//! same on every CPU, so the selectors are constants.

use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::{PrivilegeLevel, VirtAddr};

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_SS: u16 = 0x10;
/// Order is dictated by sysret: user data directly before user code.
pub const USER_SS: u16 = 0x1b;
pub const USER_CS: u16 = 0x23;
const TSS_SELECTOR: u16 = 0x28;

/// Size of the I/O permission bitmap: one bit per port, 0 = allowed.
pub const IOMAP_BYTES: usize = 8192;
const DOUBLE_FAULT_STACK: usize = 4096 * 5;

#[repr(C, align(16))]
struct Stack([u8; DOUBLE_FAULT_STACK]);

/// The TSS with its I/O permission bitmap right behind it (where
/// `iomap_base` points). The extra byte must stay 0xff.
#[repr(C)]
struct TssWithIoMap {
    tss: TaskStateSegment,
    iomap: [u8; IOMAP_BYTES + 1],
}

/// One CPU's descriptor tables. Lives in that CPU's `Cpu` block, which is
/// never freed, so the CPU may keep pointing at it.
#[repr(C)]
pub struct CpuTables {
    gdt: GlobalDescriptorTable,
    tss: TssWithIoMap,
    /// Whether the bitmap currently grants any port (to skip resetting it).
    iomap_open: bool,
    double_fault_stack: Stack,
}

impl CpuTables {
    pub const fn new() -> Self {
        CpuTables {
            gdt: GlobalDescriptorTable::new(),
            tss: TssWithIoMap { tss: TaskStateSegment::new(), iomap: [0xff; IOMAP_BYTES + 1] },
            iomap_open: false,
            double_fault_stack: Stack([0; DOUBLE_FAULT_STACK]),
        }
    }

    /// Initializes tables in zeroed memory without building them on the
    /// stack (they are large): TSS (whose I/O map base must point behind
    /// it), GDT and a closed I/O bitmap; the rest stays zero.
    ///
    /// SAFETY: `this` points to zeroed, writable memory for a `CpuTables`.
    pub unsafe fn init_in_place(this: *mut CpuTables) {
        unsafe {
            core::ptr::addr_of_mut!((*this).gdt).write(GlobalDescriptorTable::new());
            core::ptr::addr_of_mut!((*this).tss.tss).write(TaskStateSegment::new());
            core::ptr::addr_of_mut!((*this).tss.iomap).cast::<u8>().write_bytes(0xff, IOMAP_BYTES + 1);
        }
    }

    /// Builds the GDT and TSS and loads them on the calling CPU.
    pub fn load(&'static mut self) {
        let df_top = VirtAddr::from_ptr(&self.double_fault_stack) + DOUBLE_FAULT_STACK as u64;
        self.tss.tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = df_top;
        let tss: &'static TssWithIoMap = unsafe { &*(&raw const self.tss) };
        let gdt = &mut self.gdt;
        let selectors = [
            gdt.append(Descriptor::kernel_code_segment()),
            gdt.append(Descriptor::kernel_data_segment()),
            gdt.append(Descriptor::user_data_segment()),
            gdt.append(Descriptor::user_code_segment()),
            gdt.append(Descriptor::tss_segment_with_iomap(&tss.tss, &tss.iomap).expect("TSS I/O bitmap layout")),
        ];
        assert_eq!(selectors.map(|s| s.0), [KERNEL_CS, KERNEL_SS, USER_SS, USER_CS, TSS_SELECTOR]);
        let gdt: &'static GlobalDescriptorTable = unsafe { &*(&raw const self.gdt) };
        gdt.load();
        unsafe {
            CS::set_reg(SegmentSelector::new(KERNEL_CS >> 3, PrivilegeLevel::Ring0));
            SS::set_reg(SegmentSelector::new(KERNEL_SS >> 3, PrivilegeLevel::Ring0));
            DS::set_reg(SegmentSelector::new(KERNEL_SS >> 3, PrivilegeLevel::Ring0));
            ES::set_reg(SegmentSelector::new(KERNEL_SS >> 3, PrivilegeLevel::Ring0));
            load_tss(SegmentSelector::new(TSS_SELECTOR >> 3, PrivilegeLevel::Ring0));
        }
    }

    /// Stack the CPU switches to on interrupts from ring 3.
    pub fn set_kernel_stack(&mut self, top: u64) {
        self.tss.tss.privilege_stack_table[0] = VirtAddr::new(top);
    }

    /// Installs the I/O permission bitmap of the process about to run;
    /// `None` denies every port.
    pub fn set_io_bitmap(&mut self, bitmap: Option<&[u8; IOMAP_BYTES]>) {
        match bitmap {
            Some(b) => {
                self.tss.iomap[..IOMAP_BYTES].copy_from_slice(b);
                self.iomap_open = true;
            }
            None if self.iomap_open => {
                self.tss.iomap[..IOMAP_BYTES].fill(0xff);
                self.iomap_open = false;
            }
            None => {}
        }
    }
}

pub fn kernel_code() -> SegmentSelector {
    SegmentSelector::new(KERNEL_CS >> 3, PrivilegeLevel::Ring0)
}

pub fn kernel_data() -> SegmentSelector {
    SegmentSelector::new(KERNEL_SS >> 3, PrivilegeLevel::Ring0)
}

pub fn user_code() -> SegmentSelector {
    SegmentSelector::new(USER_CS >> 3, PrivilegeLevel::Ring3)
}

pub fn user_data() -> SegmentSelector {
    SegmentSelector::new(USER_SS >> 3, PrivilegeLevel::Ring3)
}
