#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;

mod drivers;
mod interrupts;

entry_point!(kernel_main);

fn kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    interrupts::init();
    printkln!("Druecke eine Taste...");
    loop {
        if let Some(sc) = drivers::keyboard::pop_scancode() {
            printkln!("Scancode: {:#x}", sc);
        }
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    printkln!("PANIC: {}", info);
    loop {}
}
