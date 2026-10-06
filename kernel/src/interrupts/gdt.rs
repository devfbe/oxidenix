use lazy_static::lazy_static;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
/// User selectors for assembly; `init` checks them against the GDT.
pub const USER_SS: u16 = 0x1b;
pub const USER_CS: u16 = 0x23;

const STACK_SIZE: usize = 4096 * 5;

#[repr(align(16))]
struct Stack {
    _bytes: [u8; STACK_SIZE],
}

static mut DOUBLE_FAULT_STACK: Stack = Stack { _bytes: [0; STACK_SIZE] };
static mut RING0_STACK: Stack = Stack { _bytes: [0; STACK_SIZE] };

fn stack_top(stack: *const Stack) -> VirtAddr {
    VirtAddr::from_ptr(stack) + STACK_SIZE as u64
}

/// Size of the I/O permission bitmap: one bit per port, 0 = allowed.
pub const IOMAP_BYTES: usize = 8192;

/// The TSS with its I/O permission bitmap right behind it (where
/// `iomap_base` points). The extra byte must stay 0xff.
#[repr(C)]
struct TssWithIoMap {
    tss: TaskStateSegment,
    iomap: [u8; IOMAP_BYTES + 1],
}

// Mutable because rsp0 and the I/O bitmap change on every process switch.
static mut TSS: TssWithIoMap = TssWithIoMap { tss: TaskStateSegment::new(), iomap: [0xff; IOMAP_BYTES + 1] };
/// Whether the bitmap currently grants any port (to skip resetting it).
static mut IOMAP_OPEN: bool = false;

lazy_static! {
    // Order is dictated by sysret: user data directly before user code.
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss_ref: &'static TssWithIoMap = unsafe { &*(&raw const TSS) };
        let tss = gdt.append(
            Descriptor::tss_segment_with_iomap(&tss_ref.tss, &tss_ref.iomap).expect("TSS I/O bitmap layout"),
        );
        (
            gdt,
            Selectors {
                kernel_code,
                kernel_data,
                user_data,
                user_code,
                tss,
            },
        )
    };
}

#[derive(Clone, Copy)]
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
    tss: SegmentSelector,
}

pub fn selectors() -> Selectors {
    GDT.1
}

pub fn init() {
    use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
    use x86_64::instructions::tables::load_tss;

    unsafe {
        let tss = &mut (*(&raw mut TSS)).tss;
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            stack_top(&raw const DOUBLE_FAULT_STACK);
        tss.privilege_stack_table[0] = stack_top(&raw const RING0_STACK);
    }
    assert_eq!((GDT.1.user_data.0, GDT.1.user_code.0), (USER_SS, USER_CS));
    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.kernel_code);
        SS::set_reg(GDT.1.kernel_data);
        DS::set_reg(GDT.1.kernel_data);
        ES::set_reg(GDT.1.kernel_data);
        load_tss(GDT.1.tss);
    }
}

/// Stack the CPU switches to on interrupts from ring 3.
pub fn set_kernel_stack(top: VirtAddr) {
    unsafe { (*(&raw mut TSS)).tss.privilege_stack_table[0] = top };
}

/// Installs the I/O permission bitmap of the process about to run; `None`
/// denies every port.
pub fn set_io_bitmap(bitmap: Option<&[u8; IOMAP_BYTES]>) {
    unsafe {
        let iomap = &mut (*(&raw mut TSS)).iomap;
        match bitmap {
            Some(b) => {
                iomap[..IOMAP_BYTES].copy_from_slice(b);
                IOMAP_OPEN = true;
            }
            None if IOMAP_OPEN => {
                iomap[..IOMAP_BYTES].fill(0xff);
                IOMAP_OPEN = false;
            }
            None => {}
        }
    }
}
