# Rust Kernel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bare-metal x86_64 Rust-Kernel der in QEMU bootet, VGA-Textausgabe hat, PS/2-Keyboard-Input verarbeitet und eine interaktive Minishell mit Command-Parser anbietet.

**Architecture:** Kein `std`, kein Heap. `bootloader`-Crate übernimmt den BIOS-Boot. Treiber (VGA, Keyboard) sind globale Singletons hinter `spin::Mutex`. Interrupts via IDT + PIC 8259. Shell ist ein busy-wait REPL über eine `heapless` Scancode-Queue.

**Tech Stack:** Rust nightly, `bootloader 0.11`, `x86_64 0.15`, `pic8259 0.10`, `pc-keyboard 0.7`, `spin 0.9`, `volatile 0.5`, `heapless 0.8`, QEMU `qemu-system-x86_64`

**Spec:** `docs/superpowers/specs/2026-10-06-rust-kernel-design.md`

## Global Constraints

- Target: `x86_64-unknown-none` (custom JSON target spec, kein Red Zone, kein SSE)
- Rust-Channel: nightly (für `#![no_std]`, `#![no_main]`, inline-asm)
- Kein `alloc`-Crate, kein Heap-Allocator
- QEMU: `qemu-system-x86_64` muss installiert sein
- PIC Master-Offset: 32, Slave-Offset: 40
- Keyboard-Scancode-Queue: Größe 128 (`heapless::spsc::Queue`)
- Shell-Eingabepuffer: max 256 Zeichen (`heapless::String<256>`)
- Shell-Argumente: max 8 (`heapless::Vec<&str, 8>`)

## Review Focus

- **Keyboard-Queue voll:** Wenn IRQ feuert und Queue voll ist, Scancode stillschweigend verwerfen (kein Panic im IRQ-Handler)
- **Backspace auf leerem Puffer:** `read_line()` muss Underflow abfangen — kein Entfernen wenn String leer
- **Unbekannte Scancodes:** `pc-keyboard`-Dekodierung gibt `None` zurück — muss graceful ignoriert werden, kein Unwrap
- **`halt` in QEMU:** Port `0xf4` schreiben beendet QEMU mit Exit-Code 33 — nur testen wenn `isa-debug-exit`-Device konfiguriert ist
- **Double-Fault ohne IST-Stack:** Wenn TSS/GDT vor IDT geladen wird, kein Triple-Fault — Reihenfolge in `main.rs` muss GDT vor IDT initialisieren

---

## Task 1: Projekt-Scaffolding

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `x86_64-kernel.json`
- Create: `.cargo/config.toml`
- Create: `Makefile`
- Create: `src/main.rs`

**Interfaces:**
- Produces: `_start()` Entry-Point, `panic_handler`, lauffähiges `cargo build`

- [ ] **Step 1: `rust-toolchain.toml` schreiben**

```toml
[toolchain]
channel = "nightly"
components = ["rust-src", "llvm-tools-preview"]
```

- [ ] **Step 2: `x86_64-kernel.json` schreiben** (custom target — kein Red Zone, kein SSE, soft-float)

```json
{
  "llvm-target": "x86_64-unknown-none",
  "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-f80:128-n8:16:32:64-S128",
  "arch": "x86_64",
  "target-endian": "little",
  "target-pointer-width": "64",
  "target-c-int-width": "32",
  "os": "none",
  "executables": true,
  "linker-flavor": "ld.lld",
  "linker": "rust-lld",
  "panic-strategy": "abort",
  "disable-redzone": true,
  "features": "-mmx,-sse,+soft-float"
}
```

- [ ] **Step 3: `Cargo.toml` schreiben**

```toml
[package]
name = "rust-kernel"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "rust-kernel"
test = false
bench = false

[dependencies]
bootloader = { version = "0.11", features = ["map_physical_memory"] }
x86_64 = "0.15"
pic8259 = "0.10"
pc-keyboard = "0.7"
spin = "0.9"
volatile = "0.5"
heapless = "0.8"

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"
```

- [ ] **Step 4: `.cargo/config.toml` schreiben**

```toml
[unstable]
build-std = ["core", "compiler_builtins"]
build-std-features = ["compiler-builtins-mem"]

[build]
target = "x86_64-kernel.json"

[target.'cfg(target_arch = "x86_64")']
runner = "bootimage runner"
```

- [ ] **Step 5: `src/main.rs` Skeleton schreiben**

```rust
#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[no_mangle]
pub extern "C" fn _start(boot_info: &'static bootloader::BootInfo) -> ! {
    loop {}
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
```

- [ ] **Step 6: `bootimage`-Tool installieren**

```bash
cargo install bootimage
```

- [ ] **Step 7: Erstes Build testen**

```bash
cargo build
```
Erwartet: Kompiliert ohne Fehler. Noch kein QEMU-Fenster.

- [ ] **Step 8: `Makefile` schreiben**

```makefile
.PHONY: run build clean

build:
	cargo build

run:
	cargo run

clean:
	cargo clean
```

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml rust-toolchain.toml x86_64-kernel.json .cargo/config.toml Makefile src/main.rs
git commit -m "feat: project scaffolding, boots to empty loop"
```

---

## Task 2: VGA-Treiber + `printk!`-Makro

**Files:**
- Create: `src/drivers/mod.rs`
- Create: `src/drivers/vga.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Produces:
  - `drivers::vga::WRITER: spin::Mutex<Writer>`
  - `printk!(fmt, args…)` — kernel-weites Print-Makro
  - `drivers::vga::clear_screen()`

- [ ] **Step 1: `src/drivers/mod.rs` schreiben**

```rust
pub mod vga;
```

- [ ] **Step 2: `src/drivers/vga.rs` schreiben**

```rust
use core::fmt;
use lazy_static::lazy_static;
use spin::Mutex;
use volatile::Volatile;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0, Blue = 1, Green = 2, Cyan = 3,
    Red = 4, Magenta = 5, Brown = 6, LightGray = 7,
    DarkGray = 8, LightBlue = 9, LightGreen = 10, LightCyan = 11,
    LightRed = 12, Pink = 13, Yellow = 14, White = 15,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct ColorCode(u8);

impl ColorCode {
    fn new(fg: Color, bg: Color) -> Self {
        ColorCode((bg as u8) << 4 | (fg as u8))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct ScreenChar {
    ascii: u8,
    color: ColorCode,
}

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;

#[repr(transparent)]
struct Buffer {
    chars: [[Volatile<ScreenChar>; BUFFER_WIDTH]; BUFFER_HEIGHT],
}

pub struct Writer {
    col: usize,
    row: usize,
    color: ColorCode,
    buffer: &'static mut Buffer,
}

impl Writer {
    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.new_line(),
            b'\r' => { self.col = 0; }
            byte => {
                if self.col >= BUFFER_WIDTH {
                    self.new_line();
                }
                let color = self.color;
                self.buffer.chars[self.row][self.col].write(ScreenChar { ascii: byte, color });
                self.col += 1;
            }
        }
    }

    pub fn write_str(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                0x20..=0x7e | b'\n' | b'\r' => self.write_byte(byte),
                _ => self.write_byte(0xfe),
            }
        }
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            let color = self.color;
            self.buffer.chars[self.row][self.col].write(ScreenChar { ascii: b' ', color });
        }
    }

    pub fn clear_screen(&mut self) {
        for row in 0..BUFFER_HEIGHT {
            self.clear_row(row);
        }
        self.col = 0;
        self.row = 0;
    }

    fn new_line(&mut self) {
        if self.row < BUFFER_HEIGHT - 1 {
            self.row += 1;
        } else {
            for row in 1..BUFFER_HEIGHT {
                for col in 0..BUFFER_WIDTH {
                    let ch = self.buffer.chars[row][col].read();
                    self.buffer.chars[row - 1][col].write(ch);
                }
            }
            self.clear_row(BUFFER_HEIGHT - 1);
        }
        self.col = 0;
    }

    fn clear_row(&mut self, row: usize) {
        let blank = ScreenChar { ascii: b' ', color: self.color };
        for col in 0..BUFFER_WIDTH {
            self.buffer.chars[row][col].write(blank);
        }
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_str(s);
        Ok(())
    }
}

lazy_static::lazy_static! {
    pub static ref WRITER: Mutex<Writer> = Mutex::new(Writer {
        col: 0,
        row: 0,
        color: ColorCode::new(Color::LightGray, Color::Black),
        buffer: unsafe { &mut *(0xb8000 as *mut Buffer) },
    });
}

#[macro_export]
macro_rules! printk {
    ($($arg:tt)*) => ($crate::drivers::vga::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! printkln {
    () => ($crate::printk!("\n"));
    ($($arg:tt)*) => ($crate::printk!("{}\n", format_args!($($arg)*)));
}

pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    WRITER.lock().write_fmt(args).unwrap();
}

pub fn clear_screen() {
    WRITER.lock().clear_screen();
}

pub fn backspace() {
    WRITER.lock().backspace();
}
```

- [ ] **Step 3: `lazy_static` zu `Cargo.toml` hinzufügen**

```toml
lazy_static = { version = "1.0", features = ["spin_no_std"] }
```

- [ ] **Step 4: `main.rs` aktualisieren — VGA benutzen**

```rust
#![no_std]
#![no_main]

use core::panic::PanicInfo;

mod drivers;

#[no_mangle]
pub extern "C" fn _start(_boot_info: &'static bootloader::BootInfo) -> ! {
    printkln!("Kernel gestartet!");
    loop {}
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    printkln!("PANIC: {}", info);
    loop {}
}
```

- [ ] **Step 5: Build + QEMU starten**

```bash
cargo run
```
Erwartet: QEMU-Fenster öffnet sich, schwarzer Hintergrund, "Kernel gestartet!" erscheint in der oberen linken Ecke.

- [ ] **Step 6: Commit**

```bash
git add src/drivers/ src/main.rs Cargo.toml Cargo.lock
git commit -m "feat: VGA text mode driver with printk! macro"
```

---

## Task 3: GDT, TSS und IDT (Interrupts-Grundlage)

**Files:**
- Create: `src/interrupts/mod.rs`
- Create: `src/interrupts/gdt.rs`
- Create: `src/interrupts/handlers.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `printk!`, `printkln!`
- Produces:
  - `interrupts::gdt::init()` — lädt GDT + TSS
  - `interrupts::init()` — lädt IDT, aktiviert Interrupts
  - Double-Fault-Handler mit eigenem Stack

- [ ] **Step 1: `src/interrupts/gdt.rs` schreiben**

```rust
use lazy_static::lazy_static;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            let stack_start = VirtAddr::from_ptr(unsafe { &STACK });
            stack_start + STACK_SIZE
        };
        tss
    };

    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let code = gdt.add_entry(Descriptor::kernel_code_segment());
        let tss_sel = gdt.add_entry(Descriptor::tss_segment(&TSS));
        (gdt, Selectors { code, tss: tss_sel })
    };
}

struct Selectors {
    code: SegmentSelector,
    tss: SegmentSelector,
}

pub fn init() {
    use x86_64::instructions::segmentation::{CS, Segment};
    use x86_64::instructions::tables::load_tss;

    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.code);
        load_tss(GDT.1.tss);
    }
}
```

- [ ] **Step 2: `src/interrupts/handlers.rs` schreiben**

```rust
use x86_64::structures::idt::{InterruptStackFrame, PageFaultErrorCode};

pub extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    crate::printkln!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

pub extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame, _error_code: u64,
) -> ! {
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}

pub extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;
    crate::printkln!("EXCEPTION: PAGE FAULT");
    crate::printkln!("Accessed Address: {:?}", Cr2::read());
    crate::printkln!("Error Code: {:?}", error_code);
    crate::printkln!("{:#?}", stack_frame);
    loop {}
}
```

- [ ] **Step 3: `src/interrupts/mod.rs` schreiben**

```rust
pub mod gdt;
pub mod handlers;

use lazy_static::lazy_static;
use x86_64::structures::idt::InterruptDescriptorTable;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(handlers::breakpoint_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(handlers::double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt.page_fault.set_handler_fn(handlers::page_fault_handler);
        idt
    };
}

pub fn init() {
    gdt::init();
    IDT.load();
}
```

- [ ] **Step 4: `main.rs` aktualisieren**

```rust
mod interrupts;

#[no_mangle]
pub extern "C" fn _start(_boot_info: &'static bootloader::BootInfo) -> ! {
    interrupts::init();
    printkln!("Interrupts initialisiert.");
    loop {}
}
```

- [ ] **Step 5: Build + QEMU testen**

```bash
cargo run
```
Erwartet: Kein Triple-Fault, kein sofortiges Reboot. "Interrupts initialisiert." erscheint. QEMU friert nicht ein.

- [ ] **Step 6: Commit**

```bash
git add src/interrupts/ src/main.rs
git commit -m "feat: GDT, TSS, IDT with double-fault handler"
```

---

## Task 4: PIC 8259 + Keyboard-IRQ-Handler + Scancode-Queue

**Files:**
- Create: `src/drivers/keyboard.rs`
- Modify: `src/drivers/mod.rs`
- Modify: `src/interrupts/mod.rs`
- Modify: `src/interrupts/handlers.rs`

**Interfaces:**
- Consumes: `interrupts::IDT`, `printk!`
- Produces:
  - `drivers::keyboard::SCANCODE_QUEUE: heapless::spsc::Queue<u8, 128>`
  - `drivers::keyboard::pop_scancode() -> Option<u8>`
  - PIC initialisiert, IRQ1 demaskiert, Interrupts aktiviert (`sti`)

- [ ] **Step 1: PIC-Offsets zu `interrupts/mod.rs` hinzufügen**

```rust
use pic8259::ChainedPics;
use spin::Mutex;

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = 40;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,       // 32
    Keyboard = PIC_1_OFFSET + 1, // 33
}

pub static PICS: Mutex<ChainedPics> = Mutex::new(
    unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) }
);
```

- [ ] **Step 2: Keyboard-Queue in `src/drivers/keyboard.rs` schreiben**

```rust
use heapless::spsc::Queue;
use spin::Mutex;

static SCANCODE_QUEUE: Mutex<Queue<u8, 128>> = Mutex::new(Queue::new());

pub fn push_scancode(scancode: u8) {
    let mut q = SCANCODE_QUEUE.lock();
    // voll? stillschweigend verwerfen — kein Panic im IRQ-Handler
    let _ = q.enqueue(scancode);
}

pub fn pop_scancode() -> Option<u8> {
    SCANCODE_QUEUE.lock().dequeue()
}
```

- [ ] **Step 3: Keyboard-IRQ-Handler zu `handlers.rs` hinzufügen**

```rust
use x86_64::instructions::port::Port;

pub extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    crate::drivers::keyboard::push_scancode(scancode);

    // EOI senden
    unsafe {
        crate::interrupts::PICS.lock().notify_end_of_interrupt(
            crate::interrupts::InterruptIndex::Keyboard as u8
        );
    }
}
```

- [ ] **Step 4: IDT in `interrupts/mod.rs` um Keyboard-Eintrag erweitern**

```rust
// In der lazy_static! IDT:
idt[InterruptIndex::Keyboard as usize]
    .set_handler_fn(handlers::keyboard_interrupt_handler);
```

- [ ] **Step 5: PIC initialisieren und Interrupts in `interrupts::init()` aktivieren**

```rust
pub fn init() {
    gdt::init();
    IDT.load();
    unsafe {
        PICS.lock().initialize();
    }
    x86_64::instructions::interrupts::enable(); // sti
}
```

- [ ] **Step 6: `drivers/mod.rs` aktualisieren**

```rust
pub mod keyboard;
pub mod vga;
```

- [ ] **Step 7: `main.rs` testen — Scancodes ausgeben**

Temporär zum Testen: In den `loop {}` in `_start` Scancodes ausgeben:

```rust
#[no_mangle]
pub extern "C" fn _start(_boot_info: &'static bootloader::BootInfo) -> ! {
    interrupts::init();
    printkln!("Druecke eine Taste...");
    loop {
        if let Some(sc) = drivers::keyboard::pop_scancode() {
            printkln!("Scancode: {:#x}", sc);
        }
        x86_64::instructions::hlt();
    }
}
```

- [ ] **Step 8: Build + QEMU testen**

```bash
cargo run
```
Erwartet: Bei Tastendruck erscheinen Hex-Scancodes auf dem Bildschirm.

- [ ] **Step 9: Commit**

```bash
git add src/drivers/keyboard.rs src/drivers/mod.rs src/interrupts/mod.rs src/interrupts/handlers.rs src/main.rs
git commit -m "feat: PIC 8259, keyboard IRQ handler, scancode queue"
```

---

## Task 5: Shell — REPL, Parser, Befehle

**Files:**
- Create: `src/shell/mod.rs`
- Create: `src/shell/commands.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `drivers::keyboard::pop_scancode()`, `printk!`, `printkln!`, `drivers::vga::backspace()`, `drivers::vga::clear_screen()`
- Produces: `shell::run()` — kehrt nie zurück

- [ ] **Step 1: `src/shell/commands.rs` schreiben**

```rust
use heapless::Vec;

pub fn dispatch(cmd: &str, args: &Vec<&str, 8>) {
    match cmd {
        "help" => cmd_help(),
        "clear" => cmd_clear(),
        "echo" => cmd_echo(args),
        "halt" => cmd_halt(),
        "info" => cmd_info(),
        "" => {}
        other => crate::printkln!("unbekannter befehl: {}", other),
    }
}

fn cmd_help() {
    crate::printkln!("Verfuegbare Befehle:");
    crate::printkln!("  help          - diese Hilfe");
    crate::printkln!("  clear         - Bildschirm leeren");
    crate::printkln!("  echo <text>   - Text ausgeben");
    crate::printkln!("  info          - CPU-Infos");
    crate::printkln!("  halt          - System anhalten");
}

fn cmd_clear() {
    crate::drivers::vga::clear_screen();
}

fn cmd_echo(args: &Vec<&str, 8>) {
    for (i, arg) in args.iter().enumerate() {
        if i > 0 { crate::printk!(" "); }
        crate::printk!("{}", arg);
    }
    crate::printkln!();
}

fn cmd_halt() {
    crate::printkln!("Tschuess!");
    // isa-debug-exit: Schreiben auf Port 0xf4 beendet QEMU
    use x86_64::instructions::port::Port;
    unsafe { Port::<u32>::new(0xf4).write(0); }
    loop { x86_64::instructions::hlt(); }
}

fn cmd_info() {
    // CPUID: Vendor-String aus EBX/EDX/ECX
    let cpuid = unsafe {
        let ebx: u32;
        let ecx: u32;
        let edx: u32;
        core::arch::asm!(
            "cpuid",
            in("eax") 0u32,
            out("ebx") ebx,
            out("ecx") ecx,
            out("edx") edx,
        );
        (ebx, edx, ecx)
    };
    let vendor = [
        (cpuid.0 & 0xff) as u8, ((cpuid.0 >> 8) & 0xff) as u8,
        ((cpuid.0 >> 16) & 0xff) as u8, ((cpuid.0 >> 24) & 0xff) as u8,
        (cpuid.1 & 0xff) as u8, ((cpuid.1 >> 8) & 0xff) as u8,
        ((cpuid.1 >> 16) & 0xff) as u8, ((cpuid.1 >> 24) & 0xff) as u8,
        (cpuid.2 & 0xff) as u8, ((cpuid.2 >> 8) & 0xff) as u8,
        ((cpuid.2 >> 16) & 0xff) as u8, ((cpuid.2 >> 24) & 0xff) as u8,
    ];
    crate::printk!("CPU Vendor: ");
    for &b in &vendor {
        if b.is_ascii_graphic() || b == b' ' {
            crate::printk!("{}", b as char);
        }
    }
    crate::printkln!();
}
```

- [ ] **Step 2: `src/shell/mod.rs` schreiben**

```rust
pub mod commands;

use heapless::{String, Vec};
use pc_keyboard::{layouts, DecodedKey, HandleControl, Keyboard, ScancodeSet1};
use spin::Mutex;

lazy_static::lazy_static! {
    static ref KEYBOARD: Mutex<Keyboard<layouts::Us104Key, ScancodeSet1>> =
        Mutex::new(Keyboard::new(
            ScancodeSet1::new(),
            layouts::Us104Key,
            HandleControl::Ignore,
        ));
}

pub fn run() -> ! {
    crate::printkln!("rust-kernel shell v0.1");
    crate::printkln!("Tippe 'help' fuer Hilfe.");
    crate::printkln!();

    loop {
        crate::printk!("> ");
        let line = read_line();
        let (cmd, args) = parse(&line);
        commands::dispatch(cmd, &args);
    }
}

fn read_line() -> String<256> {
    let mut buf: String<256> = String::new();
    loop {
        // hlt bis nächster Interrupt
        x86_64::instructions::hlt();

        while let Some(scancode) = crate::drivers::keyboard::pop_scancode() {
            let mut kb = KEYBOARD.lock();
            if let Ok(Some(key_event)) = kb.add_byte(scancode) {
                if let Some(key) = kb.process_keyevent(key_event) {
                    drop(kb); // Lock freigeben bevor wir printk! aufrufen
                    match key {
                        DecodedKey::Unicode('\n') => {
                            crate::printkln!();
                            return buf;
                        }
                        DecodedKey::Unicode('\x08') => {
                            // Backspace
                            if !buf.is_empty() {
                                buf.pop();
                                crate::drivers::vga::backspace();
                            }
                        }
                        DecodedKey::Unicode(c) if c.is_ascii() && !c.is_control() => {
                            if buf.push(c).is_ok() {
                                crate::printk!("{}", c);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn parse<'a>(line: &'a str) -> (&'a str, Vec<&'a str, 8>) {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let mut args: Vec<&str, 8> = Vec::new();
    for arg in parts {
        let _ = args.push(arg); // ignoriere overflow (max 8 args)
    }
    (cmd, args)
}
```

- [ ] **Step 3: `main.rs` finalisieren**

```rust
#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use core::panic::PanicInfo;

mod drivers;
mod interrupts;
mod shell;

#[no_mangle]
pub extern "C" fn _start(_boot_info: &'static bootloader::BootInfo) -> ! {
    interrupts::init();
    shell::run();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    printkln!("KERNEL PANIC: {}", info);
    loop { x86_64::instructions::hlt(); }
}
```

- [ ] **Step 4: `Makefile` um QEMU-Flags erweitern**

```makefile
QEMU_FLAGS = -device isa-debug-exit,iobase=0xf4,iosize=0x04 -m 128M

run:
	QEMU_ARGS="$(QEMU_FLAGS)" cargo run
```

Alternativ: in `.cargo/config.toml` den runner erweitern:
```toml
runner = "bootimage runner -- -device isa-debug-exit,iobase=0xf4,iosize=0x04 -m 128M"
```

- [ ] **Step 5: Build + QEMU — Shell testen**

```bash
cargo run
```
Erwartet:
- QEMU-Fenster öffnet sich
- "rust-kernel shell v0.1" erscheint
- `> ` Prompt erscheint
- Tippen von `help` + Enter zeigt Befehlsliste
- `echo hallo welt` gibt "hallo welt" aus
- `clear` leert den Bildschirm
- `info` zeigt CPU-Vendor (z.B. "GenuineIntel" oder "AuthenticAMD")
- `halt` schließt QEMU

- [ ] **Step 6: Commit**

```bash
git add src/shell/ src/main.rs .cargo/config.toml
git commit -m "feat: interactive shell with help, clear, echo, info, halt"
```

---

## Fertig

Nach Task 5 ist der Kernel vollständig: QEMU-Fenster, VGA-Ausgabe, PS/2-Keyboard-Input, Minishell mit Command-Parser und 5 eingebauten Befehlen.
