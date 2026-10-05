pub mod address_space;
pub mod elf;
pub mod syscall;

use crate::interrupts::gdt;
use address_space::AddressSpace;
use alloc::vec::Vec;
use elf::{Elf, PF_W, PF_X, PT_LOAD, PT_PHDR};
use x86_64::structures::paging::PageTableFlags;

const STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
const STACK_SIZE: u64 = 64 * 1024;

pub fn init() {
    enable_sse();
    syscall::init();
}

/// musl nutzt SSE; ohne OSFXSR loest jede SSE-Instruktion #UD aus.
fn enable_sse() {
    use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};
    unsafe {
        Cr0::update(|f| {
            f.remove(Cr0Flags::EMULATE_COPROCESSOR);
            f.insert(Cr0Flags::MONITOR_COPROCESSOR);
        });
        Cr4::update(|f| f.insert(Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE));
    }
}

pub static PROGRAMS: &[(&str, &[u8])] = &[(
    "hello",
    include_bytes!(concat!(env!("OUT_DIR"), "/hello")),
)];

/// Laedt ein eingebettetes Programm, fuehrt es im Ring 3 aus und liefert
/// den Exit-Code. Kehrt zurueck, wenn das Programm `exit` aufruft oder abstuerzt.
pub fn run(name: &str, args: &[&str]) -> Result<i64, &'static str> {
    let image = PROGRAMS
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or("Programm nicht gefunden")?
        .1;
    let elf = Elf::parse(image)?;
    let mut space = AddressSpace::new().ok_or("kein Speicher frei")?;

    let mut phdr = None;
    for ph in elf.program_headers() {
        match ph.kind {
            PT_LOAD => {
                let mut flags = PageTableFlags::empty();
                if ph.flags & PF_W != 0 {
                    flags |= PageTableFlags::WRITABLE;
                }
                if ph.flags & PF_X == 0 {
                    flags |= PageTableFlags::NO_EXECUTE;
                }
                space.map_zeroed(ph.vaddr, ph.memsz, flags)?;
                space.write(ph.vaddr, elf.segment_bytes(&ph)?)?;
                if ph.offset == 0 {
                    phdr.get_or_insert(ph.vaddr + elf.phoff);
                }
            }
            PT_PHDR => phdr = Some(ph.vaddr),
            _ => {}
        }
    }

    let auxv = [
        (3, phdr.unwrap_or(0)),       // AT_PHDR
        (4, elf.phentsize as u64),    // AT_PHENT
        (5, elf.phnum as u64),        // AT_PHNUM
        (6, 4096),                    // AT_PAGESZ
        (9, elf.entry),               // AT_ENTRY
    ];
    let sp = build_stack(&mut space, args, &auxv)?;

    let sel = gdt::selectors();
    space.activate();
    let code = unsafe {
        enter_user(elf.entry, sp, sel.user_code.0 as u64, sel.user_data.0 as u64)
    };
    drop(space);
    Ok(code)
}

/// Linux-Startstack: argc, argv[], NULL, envp[] (leer), NULL, auxv-Paare, AT_NULL.
fn build_stack(space: &mut AddressSpace, args: &[&str], auxv: &[(u64, u64)]) -> Result<u64, &'static str> {
    space.map_zeroed(
        STACK_TOP - STACK_SIZE,
        STACK_SIZE,
        PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
    )?;
    let mut sp = STACK_TOP;
    let mut argv = Vec::new();
    for arg in args {
        sp -= arg.len() as u64 + 1;
        space.write(sp, arg.as_bytes())?;
        argv.push(sp);
    }
    sp -= 16;
    let random = sp;
    let seed = unsafe { core::arch::x86_64::_rdtsc() };
    space.write(random, &[seed.to_le_bytes(), seed.rotate_left(29).to_le_bytes()].concat())?;

    let mut words: Vec<u64> = Vec::new();
    words.push(argv.len() as u64);
    words.extend(&argv);
    words.push(0);
    words.push(0);
    for &(key, val) in auxv {
        words.extend([key, val]);
    }
    words.extend([25, random, 0, 0]); // AT_RANDOM, AT_NULL

    sp = (sp - words.len() as u64 * 8) & !0xf;
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    space.write(sp, &bytes)?;
    Ok(sp)
}

#[unsafe(no_mangle)]
static mut KERNEL_RETURN_RSP: u64 = 0;

/// Sichert die callee-saved Register und springt per iretq in den Ring 3.
/// "Kehrt zurueck", wenn `return_to_kernel` aufgerufen wird.
#[unsafe(naked)]
unsafe extern "sysv64" fn enter_user(entry: u64, user_rsp: u64, user_cs: u64, user_ss: u64) -> i64 {
    core::arch::naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rip + KERNEL_RETURN_RSP], rsp",
        "push rcx",
        "push rsi",
        "push 0x202",
        "push rdx",
        "push rdi",
        "xor eax, eax",
        "xor ebx, ebx",
        "xor ecx, ecx",
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor ebp, ebp",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "xor r11d, r11d",
        "xor r12d, r12d",
        "xor r13d, r13d",
        "xor r14d, r14d",
        "xor r15d, r15d",
        "iretq",
    );
}

/// Verlaesst den Userspace endgueltig und kehrt aus `enter_user` mit `code` zurueck.
#[unsafe(naked)]
pub unsafe extern "sysv64" fn return_to_kernel(code: i64) -> ! {
    core::arch::naked_asm!(
        "mov rsp, [rip + KERNEL_RETURN_RSP]",
        "mov ax, {kds}",
        "mov ss, ax",
        "mov ds, ax",
        "mov es, ax",
        "mov rax, rdi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "sti",
        "ret",
        kds = const gdt::KERNEL_DATA_SELECTOR,
    );
}
