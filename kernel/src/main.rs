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
    printkln!("Interrupts initialisiert.");
    loop {}
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    printkln!("PANIC: {}", info);
    loop {}
}
