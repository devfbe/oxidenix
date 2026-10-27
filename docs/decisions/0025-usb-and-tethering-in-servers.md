# ADR 0025: USB in a server per controller, its network functions as links of netd

Date: 2026-10-27. Status: accepted.

## Context

The goal was USB tethering: a phone on a USB port gives the machine its network (Android's
"USB tethering": RNDIS, or CDC-NCM on newer devices). Nothing of USB existed: the kernel
handed servers only I/O ports, a DMA area and a legacy interrupt line (virtio's legacy
interface needs nothing else), netd drove exactly one network card, and on real machines USB
is xHCI, which is memory-mapped, wants message-signalled interrupts (a legacy line on a
real machine needs ACPI's `_PRT`, which needs AML) and sits behind firmware that owns it
until the operating system takes it over.

Seven questions shaped the design:

1. Where the USB host stack runs: in the kernel, in netd (the only user of tethering), or in a
   server of its own.
2. What the kernel must give a driver of a memory-mapped PCI device.
3. How frames get from that server to netd, which runs TCP/IP.
4. How netd, which drives one card through one smoltcp interface, carries several links.
5. Which USB network functions, and how a device's configuration is chosen.
6. What taking the controller over breaks, and what the server must do about it.
7. What happens when the USB server or netd dies, and how all of it is tested.

## Decision

1. **A server per xHCI controller (`servers/usbd`), the protocols in a host-tested crate
   (`crates/usb`).** The kernel implements no driver it can leave to user space (ADR 0011);
   USB parses whatever a device sends (descriptors, notifications, transfer blocks), which is
   the hostile input a server should take, not netd (whose bugs would take every socket
   down) and not the kernel. One process per controller: a controller's failure is its own.
   The kernel finds the controllers (PCI class 0c/03/30) and starts "usb0", "usb1", ...
   (`kernel/src/main.rs::start_usbd`). usbd is one thread and one event loop like netd and
   diskfs: calls that need an answer (a command, a control transfer) wait for their event
   and keep the others for the loop. Everything a device says is parsed in `crates/usb`
   (descriptors, the choice of configuration and function, RNDIS, NCM, CDC notifications,
   hubs, HID boot reports), tested on the host against real devices' descriptors and
   malformed input.

2. **A server gets a PCI function: its memory BARs (`mmio_map`) and a message-signalled
   interrupt.** `mmio_map(bar)` maps a BAR assigned to the server uncached
   (`Backing::Mmio`: PCD|PWT, an MMIO bit in the entries so that unmapping frees no frame;
   never moved, protected otherwise, or inherited). The kernel programs the device's MSI
   capability, or entry 0 of its MSI-X table, with a vector of its own (0x60-0x6f) and turns
   the legacy line off; `irq` calls these lines 16-31 (message-signalled: nothing to mask,
   an edge per event; pending before the server first enables it, so nothing is lost while
   it starts). A controller without either gets its legacy line, one without that is
   polled. Configuration space stays the kernel's (it also switches the ports of Intel's
   7/8-series chipsets from EHCI to xHCI, as Linux does). The DMA area is the server's
   (1024 pages), handed out in runs aligned to their size (no buffer crosses 64 KiB).

3. **A link is a channel from usbd to netd (`crates/linkring`).** A native server may now
   be a channel's client (`channel::native_create`, `native_connect`, `native_close`, a
   client-side doorbell watch on the completion ring: `client_watch`); its offer carries
   `Offer::instance` 0, which only `decode_any` accepts, so the services that serve Linux
   instances (diskfs, procfs) refuse it unchanged. One channel per USB Ethernet function:
   the shared area holds the link's description (MAC, MTU, driver), the carrier, and two
   frame rings (records of an 8-byte header and the frame, never wrapping: a pad record
   ends the ring), one each way. The channel's ring tails are the two sides' doorbell
   counters: a side that sleeps announces it and arms its watch with the counter it read; a
   side that made work advances the other's counter and wakes it only if the other
   announced a sleep, so no wakeup is lost (tested with threads on the host). A link goes
   with its channel: the device's removal closes it, netd sees `CLIENT_GONE`.

4. **netd routes between links itself** (see the netd commit that implements it): one
   smoltcp interface with `Medium::Ip` over all links, ARP and a DHCP client per link in
   netd (`crates/linklayer`), routes by destination subnet, by source address (a socket
   bound to a link's address leaves through it) and else by the default link (PCI cards
   before USB functions). Linux programs see one interface per link: `eth0`... for cards,
   `usb0`... for USB functions, each with its lease.

5. **CDC-NCM, CDC-ECM and RNDIS; the best function wins, then the first configuration.**
   Android tethers with RNDIS (and NCM on newer devices), Linux gadgets and many embedded
   devices with ECM or NCM, QEMU's usb-net is ECM (first configuration) and RNDIS
   (second). usbd reads every configuration and takes the best function of all (NCM before
   ECM before RNDIS: the standard ones before Microsoft's), in the first configuration that
   has it. ECM sends a frame per transfer, NCM blocks of frames (NTB16), RNDIS
   `PACKET_MSG`s; a transfer that is a multiple of the packet size gets a pad byte, as
   Linux's usbnet sends. The carrier comes from the interrupt endpoint's notifications
   (RNDIS: its media indications) and starts up where a function sends none. Hubs (USB 2,
   with their transaction translators, and USB 3) are driven too: phones hang on hubs and
   docks. Apple's iPhone tethering (ipheth) is not: the phone enables its Ethernet
   interface only for a host it paired with (usbmux and lockdown's TLS pairing), which is
   a protocol of its own.

6. **Taking the controller over takes the firmware's USB legacy emulation away: usbd drives
   boot keyboards.** The firmware's SMM code had made a USB keyboard look like a PS/2 one;
   after the handoff (USBLEGSUP, its SMIs off) a desktop's USB keyboard would be dead. usbd
   turns boot-protocol reports into set 1 scancodes that the kernel queues as the PS/2
   keyboard's (`kbd_input`, for servers allowed to type), with the host's autorepeat (USB
   keyboards do not repeat) and the console's LEDs and typematic settings (`kbd_state`).

7. **Both servers come back; the tests boot a machine without a network card.** Nobody
   connects to usbd, so no use would revive it: the kernel's keeper thread starts a "kept"
   server again when it dies, by the restart policy (ADR 0006); the new usbd resets the
   controller and finds the devices anew. When netd dies, its links' channels die with it
   (`SERVICE_GONE` rings usbd's doorbell) and usbd offers new ones, which also starts netd
   again. The kernel's shutdown waits only for the Linux instances' channels. The self-tests
   get a third boot (`OXIDENIX_TEST=1`, or `OXIDENIX_TEST=usb` alone): no virtio-net, an ECM
   function on a root port and an RNDIS one behind a hub, each on a user network of its
   own, into `/etc/usbtests.sh` (`usbnettest`: leases, names, addresses, bulk TCP both ways
   through each link and both at once, usbd's and netd's deaths); every boot has an xHCI
   controller.

## Consequences

- Real machines' USB ports work for tethering and keyboards; a phone can be tried in QEMU
  too, passed through with `-device usb-host`.
- Without an IOMMU (docs/design/iommu.md) usbd's controller can reach all memory, as the
  disk's and the card's can: usbd is as trusted as the kernel until the IOMMU design lands.
- usbd drives no mass storage, no other HID devices than boot keyboards, no isochronous
  endpoints and no USB 3 streams; the controller's interrupter 0 serves everything.
