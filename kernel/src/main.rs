//! The kernel's entry point: `kernel_main` takes the boot information from the bootloader
//! (UEFI or BIOS) and brings up the console, interrupts, memory, ACPI, time, the boot image, processes,
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
mod klog;
mod memory;
mod process;
mod random;
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
    if !drivers::serial::exists_console() {
        printkln!("[boot] no serial port: the kernel's messages are on the screen only");
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
    random::init();
    timer::init();
    timer::init_cpu();
    fs::init(ramdisk);
    time::init_page();
    if fs::program("/etc/autorun").is_some() {
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
    // Keeps free memory above the low watermark and drops what reclaim
    // took (memory::start_reclaimer): without it, allocations that cannot
    // reclaim and reclaim's deferred references would have nobody.
    if let Err(e) = memory::start_reclaimer() {
        panic!("cannot start the background reclaimer (errno {})", e);
    }
    // Frees the OOM killer's victims' memory (memory::oom).
    if let Err(e) = memory::oom::start_reaper() {
        panic!("cannot start the OOM reaper (errno {})", e);
    }
    start_diskfs();
    start_netd();
    // (After netd: the network functions on USB are links of netd's.)
    start_usbd();
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
    // In test mode netd takes the self-tests' requests (a DHCP renewal).
    let test = TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) as u8;
    let server = match process::Server::load("net", "/sbin/netd") {
        Ok(server) => server
            .ports(io as u64..io as u64 + len as u64)
            .irq(nic.irq)
            .dma(NETD_DMA_PAGES)
            .arg(alloc::format!("io={io:#x}"))
            .arg(alloc::format!("iolen={len}"))
            .arg(alloc::format!("irq={}", nic.irq))
            .arg(alloc::format!("test={test}"))
            // netd registers once DHCP is done, or after three seconds.
            .start_timeout(5 * time::NSEC_PER_SEC),
        Err(e) => return printkln!("[boot] cannot load /sbin/netd (errno {})", e),
    };
    if let Err(e) = Arc::new(server).start() {
        printkln!("[boot] netd did not start (errno {}); networking is off", e);
    }
}

/// DMA memory for each usbd: the xHCI controller's tables and rings, the
/// devices' contexts and transfer rings, and the transfers' buffers (see
/// servers/usbd/src/dma.rs).
const USBD_DMA_PAGES: u64 = 1024;

/// Hands every xHCI controller (PCI class 0x0c, subclass 0x03,
/// programming interface 0x30) to a usbd of its own ("usb0", "usb1", ...),
/// which drives it and the devices behind it and offers the network
/// functions it finds (USB tethering) to netd as links. The controller's
/// registers are its memory BAR 0; its interrupt a message-signalled
/// vector where the controller has MSI or MSI-X, else its legacy line.
fn start_usbd() {
    for (n, hc) in drivers::pci::find_class(0x0c, 0x03, 0x30).into_iter().enumerate() {
        let Some(bar) = hc.memory_bar(0) else {
            printkln!("[boot] xHCI controller {:02x}:{:02x}.{} has no register BAR", hc.bus, hc.slot, hc.func);
            continue;
        };
        intel_xhci_take_ports(&hc);
        hc.enable_memory_and_dma();
        let line = match process::irq::alloc_msi() {
            Some((line, vector)) if hc.enable_msi(vector, interrupts::apic::bsp_id()).is_some() => Some(line),
            // A legacy line the I/O APIC routes (0xff, or 0 from firmware
            // that set none: no line).
            _ => (1..16).contains(&hc.irq).then_some(hc.irq),
        };
        let name: &'static str = alloc::boxed::Box::leak(alloc::format!("usb{n}").into_boxed_str());
        let server = match process::Server::load(name, "/sbin/usbd") {
            Ok(server) => {
                let mut server = server
                    .mmio(0, bar)
                    .dma(USBD_DMA_PAGES)
                    // (Its USB keyboards type on the console.)
                    .keyboard()
                    .arg(alloc::format!("name={name}"))
                    .arg(alloc::format!("pci={:02x}:{:02x}.{}", hc.bus, hc.slot, hc.func))
                    .arg(alloc::format!("vendor={:#x}", hc.vendor))
                    // In test mode usbd takes its test devices' hints.
                    .arg(alloc::format!("test={}", TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) as u8));
                if let Some(line) = line {
                    server = server.irq(line).arg(alloc::format!("irq={line}"));
                }
                Arc::new(server)
            }
            Err(e) => return printkln!("[boot] cannot load /sbin/usbd (errno {})", e),
        };
        if let Err(e) = server.start() {
            printkln!("[boot] {} did not start (errno {}); its USB ports are off", name, e);
        }
    }
}

/// Intel's 7 and 8 series chipsets (and some of their successors) route
/// their USB ports to the EHCI controllers until the operating system
/// switches them to the xHCI controller (Linux's
/// `usb_enable_intel_xhci_ports`): the ports the firmware allows
/// (USB3PRM, USB2PRM) are switched (USB3_PSSEN, XUSB2PR).
fn intel_xhci_take_ports(hc: &drivers::pci::Device) {
    const SWITCHABLE: [u16; 6] = [0x1e31, 0x8c31, 0x8cb1, 0x9c31, 0x9cb1, 0x8d31];
    const USB3_PSSEN: u8 = 0xd8;
    const USB3PRM: u8 = 0xdc;
    const XUSB2PR: u8 = 0xd0;
    const USB2PRM: u8 = 0xd4;
    if hc.vendor != 0x8086 || !SWITCHABLE.contains(&hc.device) {
        return;
    }
    hc.write(USB3_PSSEN, hc.read(USB3PRM));
    hc.write(XUSB2PR, hc.read(USB2PRM));
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
    // (The stopped CPUs may hold the log's locks: the message goes to the console only,
    // whose locks it takes over.)
    klog::panic();
    drivers::console::take_over_for_panic();
    printkln!("KERNEL PANIC (CPU {}): {}", smp::cpu().index, info);
    if TEST_MODE.load(core::sync::atomic::Ordering::Relaxed) {
        power_off(1);
    }
    loop {
        x86_64::instructions::hlt();
    }
}
