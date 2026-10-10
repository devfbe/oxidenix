//! A native server running its program anew (`oxrt::sys::EXEC`): the
//! calling process gets a fresh address space with the server's program
//! (the image the kernel read at boot; the kernel has no paths) and the
//! given arguments, and stops being the server. For the kernel that is
//! the incarnation's end (its services and interrupt lines go, the restart
//! policy counts it, the channels it served lose their service), and the
//! new program gets no hardware access. A Linux program's execve is its
//! server's (ADR 0010).

use super::address_space::Mm;
use super::errno::*;
use super::sched::current;
use super::syscall::Frame;
use super::{cmdline_of, loader, task, tlb, with_current};
use alloc::string::ToString;
use core::sync::atomic::Ordering;
use x86_64::registers::model_specific::FsBase;
use x86_64::VirtAddr;

/// exec(argv): `argv` is a NULL-terminated array of C strings in the
/// caller's memory. Only the server's single thread may (natives make no
/// threads); on success `frame` starts the new program.
pub fn exec(frame: &mut Frame, argv: u64) -> Result<(), i64> {
    let server = with_current(|p| p.server.clone()).ok_or(EPERM)?;
    let me = current();
    if me.group.info.lock().threads.len() != 1 {
        return Err(EBUSY);
    }
    let args = super::uaccess::read_cstr_array(argv)?;
    let image = loader::load(&server.image, None, None, &args, &super::start_env())?;
    let mm = Mm::new(image.space).ok_or(ENOMEM)?;
    // The point of no return: from here on the old program is gone.
    let name = super::basename(server.path).to_string();
    {
        let mut info = me.group.info.lock();
        info.name = name.clone();
        info.cmdline = cmdline_of(&args);
        info.exe = server.path.to_string();
        // The high-water mark survives exec.
        if let Some(old) = info.mem.replace(mm.stats.clone()) {
            info.peak_pages = info.peak_pages.max(old.peak_pages.load(Ordering::Relaxed));
        }
    }
    *me.comm.lock() = name.chars().take(15).collect();
    me.group.privileged.store(false, Ordering::Relaxed);
    let (old_mm, old_server) = with_current(|p| {
        tlb::switch(p.mm.as_ref().map(|m| &*m.tlb), Some(&mm.tlb), false);
        let old_mm = p.mm.replace(mm);
        p.io_bitmap = None;
        p.copy_fixup = None;
        crate::smp::cpu().tables().set_io_bitmap(None);
        (old_mm, p.server.take())
    });
    drop(old_server);
    drop(server);
    // The server's program is gone: its incarnation ended (the restart
    // policy, ADR 0006), its services die (the new program gets none of
    // their requests) and so do its interrupt lines, as when it exits.
    let pid = me.tgid();
    super::server_exited(pid);
    super::ipc::on_exit(pid);
    super::irq::on_exit(pid);
    // No CPU has the old address space loaded for this task any more.
    drop(old_mm);
    // The channels the old program served lose their service: the new
    // program must not reach their grants.
    super::channel::service_exited(pid);
    FsBase::write(VirtAddr::new(0));
    let initial = task::FpuState::initial();
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) initial.0.as_ptr(), options(nostack)) };
    *frame = Frame::user_start(image.entry, image.sp);
    Ok(())
}
