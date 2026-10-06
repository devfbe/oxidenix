#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(allocator_ext)]

extern crate alloc;

use bootloader_api::config::{BootloaderConfig, Mapping};
use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;

mod drivers;
mod fs;
mod interrupts;
mod memory;
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

/// I/O ports of the primary ATA channel (0x1f0-0x1f7, control at 0x3f6).
const DISKFS_PORTS: &[core::ops::Range<u64>] = &[0x1f0..0x1f8, 0x3f6..0x3f7];

/// Starts the user-space servers and mounts what they provide. Drivers
/// and filesystems live in these processes, not in the kernel.
fn start_servers() {
    let server = match process::Server::load("diskfs", "/sbin/diskfs", DISKFS_PORTS) {
        Ok(server) => server,
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
