#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;

mod drivers;

entry_point!(kernel_main);

fn kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    printkln!("Kernel gestartet!");
    loop {}
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    printkln!("PANIC: {}", info);
    loop {}
}
