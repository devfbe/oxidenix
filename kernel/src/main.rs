#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;

mod drivers;
mod interrupts;
mod shell;

entry_point!(kernel_main);

fn kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    interrupts::init();
    shell::run();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    printkln!("KERNEL PANIC: {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
