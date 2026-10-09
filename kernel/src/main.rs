//! The kernel's entry point: `kernel_main` takes the boot information from the bootloader
//! (UEFI or BIOS) and brings up the console, interrupts, memory, ACPI, time, the VFS, processes,
//! the other CPUs and the servers, then starts the first program.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(allocator_ext)]

extern crate alloc;

use bootloader_api::config::{BootloaderConfig, Mapping};
use bootloader_api::{entry_point, BootInfo};
use alloc::sync::Arc;
use core::panic::PanicInfo;

mod counters;
mod drivers;
mod fs;
mod interrupts;
mod memory;
mod process;
mod shell;
pub mod smp;
pub mod sync;
mod time;
mod timer;

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config.mappings.dynamic_range_start = Some(0xffff_8000_0000_0000);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    smp::init_bsp();
    drivers::serial::init();
    if let Some(fb) = boot_info.framebuffer.as_mut() {
        drivers::console::init(fb);
    }
    interrupts::init();
    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("bootloader did not map physical memory");
    memory::init(&boot_info.memory_regions, phys_offset);
    let ramdisk = boot_info.ramdisk_addr.into_option().map(|addr| unsafe {
        core::slice::from_raw_parts(addr as *const u8, boot_info.ramdisk_len as usize)
    });
    let rsdp = boot_info.rsdp_addr.into_option().expect("bootloader found no ACPI RSDP");
    interrupts::init_controllers(rsdp);
    time::init();
    timer::init(interrupts::apic::timer_hz());
    timer::init_cpu();
    fs::init(ramdisk);
    if fs::resolve("/", "/etc/autorun", true).is_ok() {
        TEST_MODE.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    process::init();
    smp::start_aps();
    // Only now: the timer interrupt needs the scheduler.
    x86_64::instructions::interrupts::enable();
    start_servers();
    // Linux programs run with the Linux server (restricted mode).
    process::linux::init();
    shell::run();
}

/// Starts the user-space servers and mounts what they provide. Drivers
/// and filesystems live in these processes, not in the kernel.
fn start_servers() {
    // Tears down channels whose ends went where the kernel could not sleep.
    if let Err(e) = process::sched::spawn_kernel_thread("channels", process::channel::worker) {
        printkln!("[boot] cannot start the channel worker (errno {})", e);
    }
    start_diskfs();
    start_netd();
    start_procfs();
    if TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) {
        start_ringtest();
    }
}

/// The self-tests' channel service (servers/ringtest): the far end of the
/// channels lxtest has the Linux server open (test mode only).
fn start_ringtest() {
    let server = match process::Server::load("ringtest", "/sbin/ringtest") {
        Ok(server) => Arc::new(server),
        Err(e) => return printkln!("[boot] cannot load /sbin/ringtest (errno {})", e),
    };
    if let Err(e) = server.start() {
        printkln!("[boot] ringtest did not start (errno {})", e);
    }
}

/// procfs: the system-wide part of /proc and /sys, built from the kernel's
/// native information (proc_query), served to the Linux server instances
/// over the I/O rings (each mounts it at /proc and /sys); the kernel starts
/// it again when an instance connects after it died (ADR 0006).
fn start_procfs() {
    let server = match process::Server::load("procfs", "/sbin/procfs") {
        Ok(server) => Arc::new(server),
        Err(e) => return printkln!("[boot] cannot load /sbin/procfs (errno {})", e),
    };
    if let Err(e) = server.start() {
        printkln!("[boot] procfs did not start (errno {}); /proc has only the processes", e);
    }
}

/// DMA memory for diskfs: its virtqueue and request buffers (see
/// servers/diskfs/src/blk.rs).
const DISKFS_DMA_PAGES: u64 = 64;

/// Hands the first virtio block device (legacy interface) to diskfs, which
/// serves its ext2 filesystem to the Linux server instances over the I/O
/// rings (each mounts it at /data).
fn start_diskfs() {
    let Some(disk) = drivers::pci::find(0x1af4, 0x1001) else {
        return printkln!("[boot] no virtio block device; /data is not mounted");
    };
    let Some((io, len)) = disk.io_bar(0) else {
        return printkln!("[boot] the block device has no I/O ports");
    };
    disk.enable_io_and_dma();
    let server = match process::Server::load("diskfs", "/sbin/diskfs") {
        Ok(server) => Arc::new(
            server
                .ports(io as u64..io as u64 + len as u64)
                .dma(DISKFS_DMA_PAGES)
                .arg(alloc::format!("io={io:#x}"))
                .arg(alloc::format!("iolen={len}")),
        ),
        Err(e) => return printkln!("[boot] cannot load /sbin/diskfs (errno {})", e),
    };
    if let Err(e) = server.start() {
        printkln!("[boot] diskfs did not start (errno {}); /data is not served", e);
    }
}

/// DMA memory for netd: two virtqueues and their packet buffers.
const NETD_DMA_PAGES: u64 = 128;

/// Hands the first virtio network card (legacy interface) to netd, which
/// serves the Linux server instances' sockets over the channels they offer
/// it (R7b); the kernel starts it again when an instance connects after it
/// died (ADR 0006).
fn start_netd() {
    let Some(nic) = drivers::pci::find(0x1af4, 0x1000) else {
        return printkln!("[boot] no virtio network card; networking is off");
    };
    let Some((io, len)) = nic.io_bar(0) else {
        return printkln!("[boot] the network card has no I/O ports");
    };
    nic.enable_io_and_dma();
    let server = match process::Server::load("net", "/sbin/netd") {
        Ok(server) => server
            .ports(io as u64..io as u64 + len as u64)
            .irq(nic.irq)
            .dma(NETD_DMA_PAGES)
            .arg(alloc::format!("io={io:#x}"))
            .arg(alloc::format!("iolen={len}"))
            .arg(alloc::format!("irq={}", nic.irq))
            // netd registers once DHCP is done, or after three seconds.
            .start_timeout(5 * time::NSEC_PER_SEC),
        Err(e) => return printkln!("[boot] cannot load /sbin/netd (errno {})", e),
    };
    if let Err(e) = Arc::new(server).start() {
        printkln!("[boot] netd did not start (errno {}); networking is off", e);
    }
}

/// Leaves QEMU through its isa-debug-exit device; the exit status of QEMU
/// becomes `code * 2 + 1`. On real hardware this just halts.
pub fn power_off(code: u32) -> ! {
    printkln!("[kernel] power off");
    unsafe { x86_64::instructions::port::Port::<u32>::new(0xf4).write(code) };
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}

/// Resets the machine through the keyboard controller.
pub fn restart() -> ! {
    printkln!("[kernel] restart");
    unsafe { x86_64::instructions::port::Port::<u8>::new(0x64).write(0xfe) };
    loop {
        x86_64::instructions::hlt();
    }
}

/// Set in test mode (an /etc/autorun exists): a panic then ends QEMU with a
/// failure instead of hanging until the test's timeout.
pub static TEST_MODE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Stop the other CPUs first, so the message is the last word.
    interrupts::apic::ipi::halt_others();
    printkln!("KERNEL PANIC (CPU {}): {}", smp::cpu().index, info);
    if TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) {
        power_off(1);
    }
    loop {
        x86_64::instructions::hlt();
    }
}
