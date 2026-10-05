# Rust Kernel — Design Spec

**Datum:** 2026-10-06  
**Ziel:** Bare-metal x86_64-Kernel in Rust, der in QEMU bootet und eine interaktive Minishell mit VGA-Ausgabe und PS/2-Keyboard-Input bietet.

---

## 1. Überblick

Ein Betriebssystemkern ohne Standardbibliothek (`no_std`), der:
- via `bootloader`-Crate (BIOS, x86_64) in QEMU startet
- VGA-Textmodus (80×25) für Ausgabe nutzt
- PS/2-Keyboard-Input über Interrupts verarbeitet
- eine Minishell mit Command-Parser und eingebauten Befehlen bietet

Kein Heap-Allocator, kein Dateisystem, kein Userspace — reines Bare-Metal.

---

## 2. Architektur

### Schichten (von unten nach oben)

```
┌─────────────────────────────┐
│         shell/              │  Command-Loop, Parser, Befehle
├─────────────────────────────┤
│         drivers/            │  vga.rs · keyboard.rs
├─────────────────────────────┤
│         interrupts/         │  IDT, PIC (8259), Interrupt-Handler
├─────────────────────────────┤
│         main.rs             │  Kernel-Entry, Subsystem-Init
├─────────────────────────────┤
│     bootloader-Crate        │  Bootsektor, Protected→Long Mode
└─────────────────────────────┘
```

### Projektlayout

```
rust-kernel/
├── Cargo.toml
├── rust-toolchain.toml        # nightly + llvm-tools-preview
├── x86_64-kernel.json         # custom target spec (no_std, no red zone)
├── .cargo/config.toml         # runner = qemu, build-target
├── Makefile                   # make run, make clean
└── src/
    ├── main.rs                # _start(), Panic-Handler
    ├── drivers/
    │   ├── mod.rs
    │   ├── vga.rs             # Writer, printk!-Makro
    │   └── keyboard.rs        # Scancode-Queue, PS/2-Dekodierung
    ├── interrupts/
    │   ├── mod.rs             # IDT-Init, PIC-Init
    │   └── handlers.rs        # Keyboard-IRQ, Double-Fault, etc.
    └── shell/
        ├── mod.rs             # Read-Eval-Print Loop, Parser
        └── commands.rs        # help, clear, echo, halt, info
```

---

## 3. Abhängigkeiten (Cargo.toml)

| Crate | Version | Zweck |
|---|---|---|
| `bootloader` | `0.11` | BIOS-Boot, Memory-Map |
| `x86_64` | `0.15` | Port-I/O, IDT, GDT, TSS |
| `pic8259` | `0.10` | PIC-8259-Abstraktion |
| `pc-keyboard` | `0.7` | Scancode-Set-1 → KeyEvent |
| `spin` | `0.9` | Spinlock für globale Treiber-State |
| `volatile` | `0.5` | Sicherer VGA-Buffer-Zugriff |
| `heapless` | `0.8` | Stack-allozierte String/Vec (kein Heap) |

---

## 4. Boot-Sequenz

1. `bootloader`-Crate übernimmt: Real Mode → Protected Mode → Long Mode
2. Setzt page tables, lädt Kernel-ELF
3. Springt zu `_start()` in `main.rs` mit `BootInfo`-Pointer
4. `main.rs` initialisiert in Reihenfolge:
   - GDT (Global Descriptor Table) + TSS (für Double-Fault-Stack)
   - IDT (Interrupt Descriptor Table)
   - PIC 8259 (Master-Offset 32, Slave-Offset 48)
   - Interrupts aktivieren (`sti`)
   - Shell-Loop starten

---

## 5. VGA-Treiber (`drivers/vga.rs`)

- Buffer: `0xb8000`, 80×25 `ScreenChar` (ASCII-Byte + Farb-Attribut-Byte)
- Zugriff über `volatile::Volatile` — verhindert Compiler-Optimierungen weg
- Globaler `WRITER: spin::Mutex<Writer>` — thread-safe schreibbar
- `printk!(fmt, args…)` Makro als Kernel-weite Print-Funktion
- Scrolling: bei Zeile 25 alle Zeilen um 1 nach oben memcopy, letzte leeren
- Cursor-Steuerung: VGA I/O-Ports `0x3D4`/`0x3D5` für Hardware-Cursor

---

## 6. Keyboard-Treiber (`drivers/keyboard.rs`)

- IRQ1 feuert bei Tastendruck → Handler liest Scancode von Port `0x60`
- Scancode wird in statische `heapless::spsc::Queue<u8, 128>` (Ringpuffer) geschrieben
- Shell-Loop liest Queue, dekodiert via `pc_keyboard::Keyboard` (Scancode Set 1, US-Layout)
- `DecodedKey::Unicode(c)` → Zeichen in Eingabepuffer; `DecodedKey::RawKey` für Sondertasten (Backspace)
- Queue entkoppelt IRQ-Context und Shell-Loop — kein Blocking im Handler

---

## 7. Interrupt-Handling (`interrupts/`)

### IDT-Einträge

| Interrupt | Handler |
|---|---|
| 3 (Breakpoint) | Debug-Print, weiter |
| 8 (Double Fault) | Panic-Meldung, `hlt`-Loop |
| 14 (Page Fault) | Panic mit Fehleradresse, `hlt`-Loop |
| 33 (IRQ1, Keyboard) | Scancode lesen, in Queue schreiben, EOI senden |

### PIC-Konfiguration

- Master PIC: Basis-Offset 32 (IRQ0–7 → Interrupts 32–39)
- Slave PIC: Basis-Offset 40 (IRQ8–15 → Interrupts 40–47)
- Nur IRQ1 (Keyboard) und IRQ2 (Cascade) demaskiert

### Double-Fault-Stack

- TSS definiert IST-Eintrag 1 mit dediziertem 4 KiB Stack
- Double-Fault-Handler nutzt IST 1 → überlebt Stack-Overflows

---

## 8. Shell (`shell/`)

### Read-Eval-Print Loop (`mod.rs`)

```
loop {
    print!("> ");
    let line = read_line();   // busy-wait mit hlt auf Keyboard-Queue
    let (cmd, args) = parse(&line);
    execute(cmd, args);
}
```

`read_line()`:
- Liest zeichenweise aus der Keyboard-Queue
- `heapless::String<256>` als Eingabepuffer
- Backspace: letztes Zeichen entfernen, VGA-Cursor zurücksetzen
- Enter: Zeile abschließen und zurückgeben

### Parser (`mod.rs`)

- Whitespace-Split: erstes Token = Command, Rest = Args
- Args als `heapless::Vec<&str, 8>` — bis zu 8 Argumente, stack-alloziert

### Eingebaute Befehle (`commands.rs`)

| Befehl | Beschreibung |
|---|---|
| `help` | Listet alle verfügbaren Befehle |
| `clear` | Leert den VGA-Buffer |
| `echo <text…>` | Gibt alle Argumente aus |
| `halt` | Stoppt QEMU via `isa-debug-exit`-Device |
| `info` | Zeigt CPU-Vendor-String via `cpuid` |

Unbekannte Befehle: `unknown command: <cmd>` ausgeben, kein Panic.

---

## 9. QEMU-Konfiguration

```
qemu-system-x86_64
  -drive format=raw,file=<kernel.img>
  -device isa-debug-exit,iobase=0xf4,iosize=0x04
  -display gtk                    # oder -nographic für headless
  -m 128M
```

`isa-debug-exit` erlaubt `halt`-Befehl: Schreiben auf Port `0xf4` beendet QEMU sauber.

---

## 10. Nicht im Scope

- Heap-Allocator / dynamische Speicherverwaltung
- Dateisystem
- Userspace / Prozesse
- Netzwerk
- Maus-Input
- Andere Architekturen als x86_64
