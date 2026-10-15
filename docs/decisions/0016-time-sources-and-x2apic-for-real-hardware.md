# ADR 0016: Time sources and x2APIC for real hardware

Date: 2026-10-15. Status: accepted.

## Context

The kernel had only run in QEMU, and two of its assumptions do not hold on the first real
target, a Framework Laptop (12th Gen Intel i5-1240P, UEFI, no serial port; `docs/todo.md`,
"Real hardware"):

1. **The PIT was the reference for everything that is not stated by the hardware.** The TSC's
   frequency came from CPUID leaf 0x15 when it gave all three values, else from kvmclock,
   else from two intervals of PIT channel 2; the local APIC timer was measured against the PIT
   unconditionally, and the INIT/STARTUP pauses of the CPU start-up were PIT busy-waits.
   Recent Intel chipsets may gate the PIT's clock (it is off on some platforms when the
   firmware has no use for it, and it is not required by the ACPI hardware-reduced profile):
   a busy-wait on its OUT2 bit then never ends, and the boot hangs before the first CPU
   starts.
2. **The local APIC was driven through its memory-mapped xAPIC page.** Firmware may hand over
   in x2APIC mode (IA32_APIC_BASE bit 10; always when an APIC id is 255 or more, sometimes by
   policy, and some CPUs lock it there), where the xAPIC page does not respond and there is no
   way back short of disabling the APIC. The ids were bytes, and the MADT's local x2APIC
   entries (type 9), which firmware uses for large ids and some for every CPU, were ignored.

A third gap: without a serial port, every message, the panic's included, must reach the
framebuffer; a panic while another CPU (or the panicking one) held the console's lock would
have been silent.

## Decision

1. **The TSC's frequency comes from the most authoritative source available, in this order**
   (`kernel/src/time.rs`, one boot line names it: `[time] clock source: TSC at ... (source)`):
   1. *A hypervisor's statement* (only with CPUID 1 ECX bit 31): kvmclock's published TSC
      scale, else the timing leaf 0x4000_0010 (TSC kHz in EAX; VMware's, offered by QEMU and
      others). In a virtual machine the hypervisor alone knows what the guest's TSC runs at
      (it may scale it), so it beats the CPUID leaves it passes through.
   2. *The CPU's statement* (Intel, as Linux's `native_calibrate_tsc`): leaf 0x15 gives the
      ratio of the TSC to the core crystal and, on current CPUs, the crystal's frequency
      (the i5-1240P: 38.4 MHz × 110 / 2 = 2112 MHz, exactly what Linux reports there). Where
      ECX is 0, the crystal is known from the CPU model (Skylake and Kaby Lake: 24 MHz;
      Goldmont: 19.2 or 25 MHz), else derived from leaf 0x16's base frequency. A stated
      frequency is exact; a measured one is off by a few ppm at best.
   3. *A measurement* (`kernel/src/drivers/refclock.rs`) against a reference clock of known
      frequency found in the ACPI tables: the HPET (the HPET table; its 64- or 32-bit main
      counter, enabled for the measurement if the firmware left it off), else the ACPI PM
      timer (the FADT; 24 or 32 bits, an I/O port; none on hardware-reduced ACPI), and only
      as the last resort the PIT. Each run counts TSC cycles over 50 ms of the reference; each
      end pairs the reference reading with the middle of the shortest of five TSC brackets
      around it (the reference read itself costs about a microsecond); the result is the
      median of three runs and must be plausible (0.1 to 20 GHz). Every wait on a reference
      clock gives up after 2^33 TSC cycles, so a gated PIT or a dead HPET fails over instead
      of hanging. Without any source the boot stops with a panic that says so.

   Whether the TSC is invariant (CPUID 0x8000_0007 EDX bit 8) is logged on the same line; the
   kernel uses the TSC either way, as it has no other clock to fall back on, and a CPU without
   an invariant TSC is not a target.
2. **Nothing waits on the PIT any more.** `time::delay_us` busy-waits on the TSC (after
   `time::init`), and the CPU start-up's INIT and STARTUP pauses use it. The local APIC
   timer runs in TSC-deadline mode where CPUID 1 ECX bit 24 says it exists (no calibration
   needed: the deadline is a TSC value); otherwise it runs one-shot, and its frequency is
   measured against the TSC over 10 ms (`apic::measure_timer`, after `time::init`, which
   therefore moved out of `apic::init`). The PIT code that remains is the last-resort
   reference in `refclock`.
3. **x2APIC whenever the CPU has it, and always when the firmware handed over in it**
   (`kernel/src/interrupts/apic.rs`, one boot line: `[apic] x2APIC mode (...)` or
   `[apic] xAPIC mode (...)`). Its registers are MSRs (0x800 + offset / 16), the interrupt
   command is one 64-bit MSR write with a 32-bit destination, preceded by `mfence; lfence`
   (an x2APIC MSR write is not serializing, and the target must see what it is told about),
   and the EOI is an MSR write: cheaper than the MMIO page, and under KVM an MSR exit
   instead of an emulated MMIO access. Each CPU switches its own local APIC (from disabled,
   through xAPIC). APIC ids are 32 bits throughout (`Cpu::apic_id`, the MADT's CPU list,
   the IPI calls); the MADT's local APIC and local x2APIC entries are merged, each id once.
   Without x2APIC, a CPU whose id is 255 or more stays off with a message.

   The I/O APIC keeps physical 8-bit destinations (to the bootstrap CPU): without interrupt
   remapping (VT-d) it cannot address an id of 255 or more. Linux's policy is the same (x2APIC
   without remapping only while every id fits 8 bits). A bootstrap CPU with such an id gets a
   boot message saying device interrupts cannot reach it; on the target the ids are small.
   Interrupt remapping is left until a machine needs it.
4. **The screen is the console of last resort.** The serial port is probed at start-up (its
   scratch register must keep what is written; an absent port reads 0xff) and left alone
   when absent, with a boot message saying the messages are on the screen only. A panic,
   after stopping the other CPUs, waits about a tenth of a second for the console's and the
   serial port's locks and then frees them by force (`console::take_over_for_panic`), so its
   message is drawn even when the panicking CPU or a stopped one held them.

## Consequences

- The kernel boots without a working PIT, HPET or PM timer as long as one source of the list
  is there; on the target (Intel, leaf 0x15 complete) the frequency is stated, exact, and
  costs no boot time (the PIT measurement took about 165 ms).
- Measured sources cost about 150 ms of boot time (three 50 ms runs), which only machines
  without a stated frequency pay (QEMU without KVM, older CPUs).
- Under KVM the guest uses x2APIC by default now (QEMU's `-cpu max` and `host` offer it),
  and the xAPIC path is exercised with `-cpu ...,-x2apic`; both are tested.
- In QEMU, leaf 0x15 is never offered, so the CPUID path is checked only against the host's
  leaves (the development machine is the target); every other path runs in QEMU:
  kvmclock (`-accel kvm`), leaf 0x4000_0010 (`-cpu host,+invtsc,-kvmclock`), the HPET
  (`-cpu max,-kvmclock` or emulation, `OXIDENIX_ACCEL=tcg`), the PM timer (with
  `-machine hpet=off`), x2APIC handed over by the firmware (OVMF does it with
  `-smp 4,maxcpus=288`, which needs `-device intel-iommu,intremap=on,eim=on` and
  `-machine q35,kernel-irqchip=split`).

## Alternatives considered

- **Calibrating the TSC even when CPUID states it**, as a cross-check: costs boot time, and a
  measurement is less precise than the statement it would check. Linux trusts leaf 0x15 on
  Intel (it marks the frequency "known"); so does this kernel.
- **Preferring CPUID 0x15 over the hypervisor**: a hypervisor that passes the host's leaf
  through while scaling the guest's TSC would make it wrong; the hypervisor's own statement
  cannot be.
- **The HPET or the PM timer as the clock source** (instead of only a reference): reading
  them costs about a microsecond and a trip out of the CPU (an exit under a hypervisor),
  where the TSC costs a few cycles and is readable in user mode (the vDSO, ADR 0014).
- **Keeping xAPIC when the firmware leaves the choice**: works on the target, but x2APIC is
  where firmware is heading (and the only mode for ids of 255 and more), cheaper per IPI and
  EOI, and using it by default keeps that path tested.
