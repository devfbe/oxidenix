//! Device drivers the kernel keeps: ACPI tables, the framebuffer console, the keyboard, PCI
//! enumeration, the CMOS clock, the serial port and the terminal layer.

pub mod acpi;
pub mod console;
pub mod glyphs;
pub mod keyboard;
pub mod pci;
pub mod rtc;
pub mod serial;
pub mod tty;
