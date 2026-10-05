pub mod address_space;
pub mod elf;
pub mod syscall;

use crate::interrupts::gdt;
use address_space::AddressSpace;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use elf::{Elf, PF_W, PF_X, PT_LOAD, PT_PHDR};
use syscall::Frame;
use x86_64::instructions::interrupts;
use x86_64::registers::model_specific::FsBase;
use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

pub type Pid = u64;

const STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
const STACK_SIZE: u64 = 64 * 1024;
const KSTACK_SIZE: usize = 64 * 1024;

pub static PROGRAMS: &[(&str, &[u8])] = &[
    ("hello", include_bytes!(concat!(env!("OUT_DIR"), "/hello"))),
    ("forktest", include_bytes!(concat!(env!("OUT_DIR"), "/forktest"))),
];

#[repr(C, align(16))]
struct KernelStack([u8; KSTACK_SIZE]);

#[repr(C, align(16))]
struct FpuState([u8; 512]);

impl FpuState {
    fn initial() -> Box<Self> {
        let mut s = Box::new(FpuState([0; 512]));
        s.0[0..2].copy_from_slice(&0x037f_u16.to_le_bytes()); // FCW
        s.0[24..28].copy_from_slice(&0x1f80_u32.to_le_bytes()); // MXCSR
        s
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ready,
    Running,
    WaitChild,
    /// Wait-Status im Linux-Format (exit code << 8 oder Signalnummer).
    Zombie(i32),
}

struct Process {
    pid: Pid,
    ppid: Pid,
    name: String,
    state: State,
    space: Option<AddressSpace>,
    kstack: Option<Box<KernelStack>>,
    kernel_rsp: u64,
    fs_base: u64,
    fpu: Box<FpuState>,
}

impl Process {
    fn kstack_top(&self) -> Option<u64> {
        self.kstack
            .as_ref()
            .map(|s| s.0.as_ptr() as u64 + KSTACK_SIZE as u64)
    }
}

struct Scheduler {
    procs: BTreeMap<Pid, Box<Process>>,
    ready: VecDeque<Pid>,
    current: Pid,
    next_pid: Pid,
}

impl Scheduler {
    fn cur(&mut self) -> &mut Process {
        let pid = self.current;
        self.procs.get_mut(&pid).expect("aktueller Prozess fehlt")
    }

    fn make_ready(&mut self, pid: Pid) {
        if let Some(p) = self.procs.get_mut(&pid) {
            p.state = State::Ready;
            self.ready.push_back(pid);
        }
    }
}

/// Einkern-System: Zugriff nur mit abgeschalteten Interrupts. Ein Mutex
/// ginge nicht, weil ueber Kontextwechsel hinweg kein Lock gehalten werden darf.
struct SchedCell(UnsafeCell<Option<Scheduler>>);
unsafe impl Sync for SchedCell {}
static SCHED: SchedCell = SchedCell(UnsafeCell::new(None));

fn sched() -> &'static mut Scheduler {
    debug_assert!(!interrupts::are_enabled());
    unsafe { (*SCHED.0.get()).as_mut().expect("process::init fehlt") }
}

pub fn init() {
    enable_sse();
    syscall::init();
    let kernel = Process {
        pid: 0,
        ppid: 0,
        name: "kernel".to_string(),
        state: State::Running,
        space: None,
        kstack: None,
        kernel_rsp: 0,
        fs_base: 0,
        fpu: FpuState::initial(),
    };
    let mut procs = BTreeMap::new();
    procs.insert(0, Box::new(kernel));
    unsafe {
        *SCHED.0.get() = Some(Scheduler {
            procs,
            ready: VecDeque::new(),
            current: 0,
            next_pid: 1,
        })
    };
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

pub fn current_pid() -> Pid {
    interrupts::without_interrupts(|| sched().current)
}

pub fn current_ppid() -> Pid {
    interrupts::without_interrupts(|| sched().cur().ppid)
}

/// Neuer Prozess, der beim ersten Einplanen ueber `syscall_return` mit
/// `frame` in den Ring 3 springt.
fn new_process(pid: Pid, ppid: Pid, name: String, space: AddressSpace, frame: Frame) -> Box<Process> {
    // new_zeroed statt Box::new: kein Zwischenobjekt auf dem (kleinen) Kernel-Stack.
    let mut kstack = unsafe { Box::<KernelStack>::new_zeroed().assume_init() };
    let top = kstack.0.as_mut_ptr() as u64 + KSTACK_SIZE as u64;
    let frame_addr = top - core::mem::size_of::<Frame>() as u64;
    unsafe {
        (frame_addr as *mut Frame).write(frame);
        // Von switch_stacks erwartet: r15..rbx (6 Woerter) und Ruecksprungadresse.
        ((frame_addr - 8) as *mut u64).write(syscall::syscall_return as *const () as u64);
        for i in 1..=6 {
            ((frame_addr - 8 - i * 8) as *mut u64).write(0);
        }
    }
    Box::new(Process {
        pid,
        ppid,
        name,
        state: State::Ready,
        space: Some(space),
        kstack: Some(kstack),
        kernel_rsp: frame_addr - 56,
        fs_base: 0,
        fpu: FpuState::initial(),
    })
}

/// Laedt ein eingebettetes Programm in einen frischen Adressraum.
fn load_program(name: &str, args: &[&str]) -> Result<(AddressSpace, u64, u64), i64> {
    let image = PROGRAMS
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or(syscall::ENOENT)?
        .1;
    let elf = Elf::parse(image).map_err(|_| syscall::EINVAL)?;
    let mut space = AddressSpace::new().ok_or(syscall::ENOMEM)?;

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
                space.map_zeroed(ph.vaddr, ph.memsz, flags).map_err(|_| syscall::ENOMEM)?;
                let bytes = elf.segment_bytes(&ph).map_err(|_| syscall::EINVAL)?;
                space.write(ph.vaddr, bytes).map_err(|_| syscall::EINVAL)?;
                if ph.offset == 0 {
                    phdr.get_or_insert(ph.vaddr + elf.phoff);
                }
            }
            PT_PHDR => phdr = Some(ph.vaddr),
            _ => {}
        }
    }

    let auxv = [
        (3, phdr.unwrap_or(0)),    // AT_PHDR
        (4, elf.phentsize as u64), // AT_PHENT
        (5, elf.phnum as u64),     // AT_PHNUM
        (6, 4096),                 // AT_PAGESZ
        (9, elf.entry),            // AT_ENTRY
    ];
    let sp = build_stack(&mut space, args, &auxv).map_err(|_| syscall::ENOMEM)?;
    Ok((space, elf.entry, sp))
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

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Startet ein Programm als Kind des aktuellen Prozesses.
pub fn spawn(name: &str, args: &[&str]) -> Result<Pid, i64> {
    let (space, entry, sp) = load_program(name, args)?;
    Ok(interrupts::without_interrupts(|| {
        let s = sched();
        let pid = s.next_pid;
        s.next_pid += 1;
        let p = new_process(pid, s.current, name.to_string(), space, Frame::user_start(entry, sp));
        s.procs.insert(pid, p);
        s.ready.push_back(pid);
        pid
    }))
}

pub fn fork(frame: &Frame) -> Result<Pid, i64> {
    let s = sched();
    let parent = s.cur();
    let name = parent.name.clone();
    let space = parent
        .space
        .as_ref()
        .ok_or(syscall::EINVAL)?
        .clone_user()
        .map_err(|_| syscall::ENOMEM)?;
    let mut child_frame = frame.clone();
    child_frame.rax = 0;
    let pid = s.next_pid;
    s.next_pid += 1;
    let mut child = new_process(pid, s.current, name, space, child_frame);
    child.fs_base = FsBase::read().as_u64();
    unsafe { fxsave(&mut child.fpu) };
    s.procs.insert(pid, child);
    s.ready.push_back(pid);
    Ok(pid)
}

pub fn exec(frame: &mut Frame, path: &str, args: &[String]) -> Result<(), i64> {
    let name = basename(path);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (space, entry, sp) = load_program(name, &args)?;
    let p = sched().cur();
    space.activate();
    // Alter Adressraum wird hier freigegeben; er ist nicht mehr aktiv.
    p.space = Some(space);
    p.name = name.to_string();
    FsBase::write(VirtAddr::new(0));
    unsafe { fxrstor(&FpuState::initial()) };
    *frame = Frame::user_start(entry, sp);
    Ok(())
}

pub fn exit(status: i32) -> ! {
    interrupts::disable();
    let s = sched();
    let pid = s.current;
    assert!(pid != 0, "Kernel-Task darf nicht exit aufrufen");
    for p in s.procs.values_mut() {
        if p.ppid == pid {
            p.ppid = 0;
        }
    }
    unsafe {
        x86_64::registers::control::Cr3::write(
            crate::memory::kernel_l4(),
            x86_64::registers::control::Cr3Flags::empty(),
        )
    };
    let me = s.cur();
    me.space = None;
    me.state = State::Zombie(status);
    let ppid = me.ppid;
    if s.procs.get(&ppid).is_some_and(|p| p.state == State::WaitChild) {
        s.make_ready(ppid);
    }
    schedule();
    unreachable!("Zombie wurde wieder eingeplant");
}

/// Wartet auf ein Kind (`target` = None: beliebiges). Liefert (pid, status)
/// oder None bei `nohang`, wenn noch keins fertig ist.
fn wait_child(target: Option<Pid>, nohang: bool) -> Result<Option<(Pid, i32)>, i64> {
    loop {
        let s = sched();
        let me = s.current;
        let is_target = |p: &Process| p.ppid == me && p.pid != me && target.is_none_or(|t| t == p.pid);
        if !s.procs.values().any(|p| is_target(p)) {
            return Err(syscall::ECHILD);
        }
        let zombie = s.procs.values().find(|p| is_target(p) && matches!(p.state, State::Zombie(_)));
        if let Some(z) = zombie {
            let (pid, State::Zombie(status)) = (z.pid, z.state) else { unreachable!() };
            s.procs.remove(&pid);
            return Ok(Some((pid, status)));
        }
        if nohang {
            return Ok(None);
        }
        s.cur().state = State::WaitChild;
        schedule();
    }
}

pub fn wait4(pid: i64, status_ptr: u64, options: u64) -> i64 {
    const WNOHANG: u64 = 1;
    let target = if pid > 0 { Some(pid as Pid) } else { None };
    match wait_child(target, options & WNOHANG != 0) {
        Ok(Some((pid, status))) => {
            if status_ptr != 0 {
                if !address_space::user_range_ok(status_ptr, 4, true) {
                    return -syscall::EFAULT;
                }
                unsafe { (status_ptr as *mut i32).write_unaligned(status) };
            }
            pid as i64
        }
        Ok(None) => 0,
        Err(e) => -e,
    }
}

/// Fuer die Kernel-Shell: wartet blockierend auf ein bestimmtes Kind.
pub fn wait_for(pid: Pid) -> Result<i32, i64> {
    interrupts::without_interrupts(|| wait_child(Some(pid), false))
        .map(|r| r.expect("blockierendes Warten liefert immer ein Ergebnis").1)
}

/// Gibt Zombies frei, deren Eltern (Kernel) nicht mehr auf sie warten.
pub fn reap_orphans() {
    interrupts::without_interrupts(|| while let Ok(Some(_)) = wait_child(None, true) {});
}

pub fn yield_now() {
    interrupts::without_interrupts(schedule);
}

/// Waehlt den naechsten lauffaehigen Prozess (Round Robin). Muss mit
/// abgeschalteten Interrupts aufgerufen werden.
pub fn schedule() {
    loop {
        let s = sched();
        let cur = s.current;
        if s.cur().state == State::Running {
            s.make_ready(cur);
        }
        if let Some(next) = s.ready.pop_front() {
            if next == cur {
                s.cur().state = State::Running;
            } else {
                switch_to(next);
            }
            return;
        }
        // Nichts lauffaehig: auf einen Interrupt warten.
        interrupts::enable_and_hlt();
        interrupts::disable();
    }
}

fn switch_to(next: Pid) {
    let s = sched();
    let prev = s.cur();
    prev.fs_base = FsBase::read().as_u64();
    unsafe { fxsave(&mut prev.fpu) };
    let prev_rsp: *mut u64 = &mut prev.kernel_rsp;

    let n = s.procs.get_mut(&next).expect("naechster Prozess fehlt");
    n.state = State::Running;
    match &n.space {
        Some(space) => space.activate(),
        None => unsafe {
            x86_64::registers::control::Cr3::write(
                crate::memory::kernel_l4(),
                x86_64::registers::control::Cr3Flags::empty(),
            )
        },
    }
    if let Some(top) = n.kstack_top() {
        gdt::set_kernel_stack(VirtAddr::new(top));
        syscall::set_kernel_stack(top);
    }
    FsBase::write(VirtAddr::new(n.fs_base));
    unsafe { fxrstor(&n.fpu) };
    let next_rsp = n.kernel_rsp;
    s.current = next;
    unsafe { switch_stacks(prev_rsp, next_rsp) };
}

unsafe fn fxsave(area: &mut FpuState) {
    unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) area.0.as_mut_ptr(), options(nostack)) };
}

unsafe fn fxrstor(area: &FpuState) {
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) area.0.as_ptr(), options(nostack)) };
}

/// Sichert die callee-saved Register auf dem aktuellen Kernel-Stack,
/// speichert rsp nach `*save` und setzt auf dem Stack `next` fort.
#[unsafe(naked)]
unsafe extern "sysv64" fn switch_stacks(save: *mut u64, next: u64) {
    core::arch::naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
    );
}

/// (true, exit code) bei normalem Ende, (false, Signal) bei Abbruch.
pub fn decode_status(status: i32) -> (bool, i32) {
    if status & 0x7f == 0 {
        (true, (status >> 8) & 0xff)
    } else {
        (false, status & 0x7f)
    }
}
