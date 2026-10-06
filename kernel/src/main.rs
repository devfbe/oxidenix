#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(allocator_ext)]

extern crate alloc;

use bootloader_api::config::{BootloaderConfig, Mapping};
use bootloader_api::{entry_point, BootInfo};
use alloc::sync::Arc;
use core::panic::PanicInfo;

mod drivers;
mod fs;
mod interrupts;
mod memory;
mod net;
mod process;
mod shell;

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config.mappings.dynamic_range_start = Some(0xffff_8000_0000_0000);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
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
    drivers::rtc::init();
    fs::init(ramdisk);
    process::init();
    // Only now: the timer interrupt needs the scheduler.
    x86_64::instructions::interrupts::enable();
    start_servers();
    shell::run();
}

/// Starts the user-space servers and mounts what they provide. Drivers
/// and filesystems live in these processes, not in the kernel.
fn start_servers() {
    start_diskfs();
    start_netd();
}

fn start_diskfs() {
    // The primary ATA channel: command block 0x1f0-0x1f7, control 0x3f6.
    let server = match process::Server::load("diskfs", "/sbin/diskfs") {
        Ok(server) => Arc::new(server.ports(0x1f0..0x1f8).ports(0x3f6..0x3f7)),
        Err(e) => return printkln!("[boot] cannot load /sbin/diskfs (errno {})", e),
    };
    match process::spawn_server(&server) {
        Ok(pid) => match process::ipc::wait_for(server.name, 3 * process::TIMER_HZ) {
            Some((service, root)) => {
                if let Err(e) = fs::mount_remote(server, service, root as u32, "data", "/dev/hdb") {
                    printkln!("[boot] cannot mount /data (errno {})", e);
                }
            }
            None => printkln!("[boot] diskfs (pid {}) did not register; /data is not mounted", pid),
        },
        Err(e) => printkln!("[boot] cannot start /sbin/diskfs (errno {})", e),
    }
}

/// DMA memory for netd: two virtqueues and their packet buffers.
const NETD_DMA_PAGES: u64 = 128;

/// Hands the first virtio network card (legacy interface) to netd.
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
            .arg(alloc::format!("irq={}", nic.irq)),
        Err(e) => return printkln!("[boot] cannot load /sbin/netd (errno {})", e),
    };
    match process::spawn_server(&Arc::new(server)) {
        // netd registers once DHCP is done (or has given up for now).
        Ok(pid) => {
            if process::ipc::wait_for("net", 5 * process::TIMER_HZ).is_none() {
                printkln!("[boot] netd (pid {}) did not register; networking is off", pid);
            }
        }
        Err(e) => printkln!("[boot] cannot start /sbin/netd (errno {})", e),
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

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    printkln!("KERNEL PANIC: {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
