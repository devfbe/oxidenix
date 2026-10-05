use lazy_static::lazy_static;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

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

// Veraenderbar, weil rsp0 bei jedem Prozesswechsel neu gesetzt wird.
static mut TSS: TaskStateSegment = TaskStateSegment::new();

lazy_static! {
    // Reihenfolge ist durch sysret vorgegeben: user data direkt vor user code.
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss = gdt.append(Descriptor::tss_segment(unsafe { &*(&raw const TSS) }));
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
        let tss = &mut *(&raw mut TSS);
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            stack_top(&raw const DOUBLE_FAULT_STACK);
        tss.privilege_stack_table[0] = stack_top(&raw const RING0_STACK);
    }
    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.kernel_code);
        SS::set_reg(GDT.1.kernel_data);
        DS::set_reg(GDT.1.kernel_data);
        ES::set_reg(GDT.1.kernel_data);
        load_tss(GDT.1.tss);
    }
}

/// Stack, auf den die CPU bei Interrupts aus dem Ring 3 wechselt.
pub fn set_kernel_stack(top: VirtAddr) {
    unsafe { (*(&raw mut TSS)).privilege_stack_table[0] = top };
}
