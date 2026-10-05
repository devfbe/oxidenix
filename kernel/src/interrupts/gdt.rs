use lazy_static::lazy_static;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
/// Fuer Assembler-Code, der keine Laufzeit-Selektoren lesen kann.
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;

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

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            stack_top(&raw const DOUBLE_FAULT_STACK);
        // Stack fuer Interrupts, die im Ring 3 eintreffen.
        tss.privilege_stack_table[0] = stack_top(&raw const RING0_STACK);
        tss
    };

    // Reihenfolge ist durch sysret vorgegeben: user data direkt vor user code.
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss = gdt.append(Descriptor::tss_segment(&TSS));
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

    assert_eq!(GDT.1.kernel_data.0, KERNEL_DATA_SELECTOR);
    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.kernel_code);
        SS::set_reg(GDT.1.kernel_data);
        DS::set_reg(GDT.1.kernel_data);
        ES::set_reg(GDT.1.kernel_data);
        load_tss(GDT.1.tss);
    }
}
