//! Device drivers the kernel keeps: ACPI tables, the framebuffer console and the console
//! device the Linux server's terminals drive, the keyboard, PCI enumeration, the CMOS clock
//! and the serial port.

pub mod acpi;
pub mod console;
pub mod console_device;
pub mod glyphs;
pub mod keyboard;
pub mod pci;
pub mod rtc;
pub mod serial;
