//! netd: the network server. It drives the virtio network card from user
//! space (if there is one), takes the USB links usbd offers (`linkring`),
//! runs the TCP/IP stack (smoltcp) over all of them, configures each by
//! DHCP, and serves the Linux server instances' sockets over the channels
//! they offer (`service`, the protocol `netring`;
//! docs/design/linux-server.md, R7b). smoltcp sees one interface of IP
//! packets; the links below it (`links`: routing, ARP, DHCP) are netd's.
//!
//! One thread, one event loop: each round takes the channels' requests,
//! polls the links and the stack, and moves the sockets' data between
//! smoltcp and the instances' rings. While rounds make progress it polls;
//! after `SPIN` rounds without any it arms the card's interrupt and every
//! channel's doorbell (the instances' and the links') and sleeps in
//! `ipc_receive` until one of them, a channel offer, or the next timer
//! (smoltcp's, the service's, the links' ARP and DHCP ones).

#![no_std]
#![no_main]

extern crate alloc;

mod links;
mod service;
mod virtio_net;

use alloc::vec;
use alloc::vec::Vec;
use links::Links;
use oxrt::println;
use ring::channel::Offer;
use smoltcp::iface::{Config, Interface, PollIngressSingleResult, PollResult, SocketSet};
use smoltcp::time::Instant;
use smoltcp::wire::HardwareAddress;
use virtio_net::VirtioNet;

/// The heap, sized from the worst case of everything on it
/// (`service::HEAP_WORST`: smoltcp's socket set for `service::MAX_SMOLTCP`
/// sockets, the service's tables at their limits, the links' frames).
/// smoltcp's socket buffers are not in it: each is memory of its own, gone
/// with the socket (`service::Region`).
const HEAP: usize = service::HEAP;
const _: () = assert!(HEAP >= service::HEAP_WORST, "netd's heap holds its worst case");

oxrt::entry!(main, heap = HEAP);

fn now() -> Instant {
    Instant::from_millis(oxrt::uptime_ms() as i64)
}

/// smoltcp's poll, with the connections that arrived given their buffers
/// between taking the packets and sending (`Service::arrivals`): their
/// SYN-ACK offers a window. True if a socket's state may have changed.
fn poll(iface: &mut Interface, links: &mut Links, sockets: &mut SocketSet<'static>, service: &mut service::Service) -> bool {
    let t = now();
    let mut changed = false;
    iface.poll_maintenance(t);
    loop {
        match iface.poll_ingress_single(t, links, sockets) {
            PollIngressSingleResult::None => break,
            PollIngressSingleResult::PacketProcessed => {}
            PollIngressSingleResult::SocketStateChanged => changed = true,
        }
    }
    service.arrivals(sockets);
    while iface.poll_egress(t, links, sockets) == PollResult::SocketStateChanged {
        changed = true;
    }
    changed
}

/// Value of `key=...` among the arguments the kernel passed.
fn arg(args: &[&str], key: &str) -> Option<u64> {
    let v = args.iter().find_map(|a| a.strip_prefix(key)?.strip_prefix('='))?;
    match v.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

/// The virtio card the kernel handed over (`io=`, `iolen=`, `irq=`), set
/// up, with its interrupt line; None without one. Err: there is one, but
/// it cannot be driven.
fn card(args: &[&str]) -> Result<Option<(VirtioNet, u8)>, ()> {
    let (Some(io), Some(iolen), Some(irq)) = (arg(args, "io"), arg(args, "iolen"), arg(args, "irq")) else {
        return Ok(None);
    };
    let (io, irq) = (io as u16, irq as u8);
    if oxrt::ioperm(io, iolen as u16).is_err() {
        println!("netd: no permission for the card's I/O ports");
        return Err(());
    }
    let Ok((dma, phys)) = oxrt::dma_map() else {
        println!("netd: no DMA memory");
        return Err(());
    };
    match VirtioNet::new(io, dma, phys, DMA_BYTES) {
        Ok(card) => Ok(Some((card, irq))),
        Err(e) => {
            println!("netd: {}", e);
            Err(())
        }
    }
}

/// Rounds without progress before netd sleeps (it polls meanwhile: a
/// client's next request or bytes usually come within them).
const SPIN: u32 = 256;
/// While busy, netd looks for channel offers and interrupts every this
/// many rounds.
const OFFERS_EVERY: u32 = 64;
const ENOSYS: i64 = 38;
const EINVAL: i64 = 22;

fn main(args: Vec<&'static str>) -> i32 {
    let Ok(card) = card(&args) else { return 1 };
    // Copies to and from grants fail instead of killing netd when a client
    // revokes one meanwhile.
    if let Err(e) = oxrt::copy::register() {
        println!("netd: cannot register the copy fixup: {}", e);
        return 1;
    }
    let irq = card.as_ref().map(|&(_, irq)| irq);
    if let Some(irq) = irq {
        if oxrt::irq_enable(irq).is_err() {
            println!("netd: cannot enable interrupt {}", irq);
            return 1;
        }
    }
    let mut links = Links::new(card.map(|(card, _)| card));
    let mut config = Config::new(HardwareAddress::Ip);
    // smoltcp's own randomness (its ports when it picks one) from the
    // kernel's generator; TCP's initial sequence numbers are RFC 6528's
    // (`service::isn`), DHCP's transaction ids netd's (`links`).
    let mut seed = [0u8; 8];
    oxrt::getrandom(&mut seed);
    config.random_seed = u64::from_le_bytes(seed);
    let mut iface = Interface::new(config, &mut links, now());
    service::init_secrets();
    iface.set_isn_generator(service::isn);
    iface.set_reuseport_hash(service::reuseport_hash);
    // Room for every socket the service may make from the start: the set
    // never moves to a larger allocation.
    let mut sockets = SocketSet::new(Vec::with_capacity(service::MAX_SMOLTCP));
    // The kernel waits for the registration, so boot messages stay in
    // order: with a card, register once its DHCP is done, or after
    // DHCP_WAIT_MS without it; without one at once (links come later).
    let register_at = oxrt::uptime_ms() + if links.has_card() { DHCP_WAIT_MS } else { 0 };
    let mut registered = false;
    let mut service = service::Service::new();
    // The kernel runs the self-tests: their requests are taken (`RENEW`).
    service.test_mode = args.contains(&"test=1");

    // The only messages netd takes are the kernel's channel offers (a
    // longer one fails in the kernel).
    let mut message = vec![0u8; ring::channel::OFFER_BYTES];
    let mut idle = 0u32;
    let mut busy = 0u32;
    loop {
        links.round_start();
        let mut progress = service.serve(&mut iface, &mut sockets);
        progress |= poll(&mut iface, &mut links, &mut sockets, &mut service);
        progress |= service.pump(&mut sockets);
        service.round_end();
        // The links' timers (ARP, DHCP), carriers, leases, gone links.
        progress |= links.poll();
        // The tests' renewal: the lease goes now, DHCP asks again a second
        // later (so that the instances see it go and come back).
        if service.take_renew() {
            println!("netd: dropping the DHCP lease (test)");
            links.renew(RENEW_DELAY_MS);
            progress = true;
        }
        if links.take_changed() {
            links.apply(&mut iface);
            links.records(&mut service.links);
            // The instances hear of it (netlink's notifications).
            service.links_changed();
            progress = true;
        }
        service.flush();
        links.flush();
        if !registered && (links.card_configured() || oxrt::uptime_ms() >= register_at) {
            if links.has_card() && !links.card_configured() {
                println!("netd: no DHCP answer yet; continuing in the background");
            }
            if let Err(e) = oxrt::ipc_register_with(netring::SERVICE, 0, oxrt::IPC_CHANNELS) {
                println!("netd: cannot register: {}", e);
                return 1;
            }
            registered = true;
        }
        let sleep = if progress || links.has_received() {
            idle = 0;
            busy += 1;
            if busy % OFFERS_EVERY != 0 {
                continue;
            }
            false
        } else {
            idle += 1;
            if idle < SPIN {
                continue;
            }
            idle = 0;
            service.prepare_sleep() && links.prepare_sleep()
        };
        let mut timeout = iface.poll_delay(now(), &sockets).map(|d| d.total_millis());
        // The service's own timers: orphans that make no progress, the end
        // of TIME-WAIT.
        if let Some(at) = service.deadline() {
            let left = (at - now().min(at)).total_millis();
            timeout = Some(timeout.map_or(left, |t| t.min(left)));
        }
        // The links' (ARP's retransmissions, DHCP's messages and lease),
        // and the registration's.
        let at = links.deadline().into_iter().chain((!registered).then_some(register_at)).min();
        if let Some(at) = at {
            let left = at.saturating_sub(oxrt::uptime_ms());
            timeout = Some(timeout.map_or(left, |t| t.min(left)));
        }
        // Marks that keep coming for nothing: a brief sleep only.
        if let Some(cap) = service.sleep_cap() {
            timeout = Some(timeout.map_or(cap, |t| t.min(cap)));
        }
        let event = oxrt::ipc_receive(&mut message, if sleep { timeout } else { Some(0) });
        service.awake();
        links.awake();
        if sleep && service.sleep_cap().is_some() {
            // Woken from a brief sleep that futile marks forced: one round,
            // then (without progress) asleep again, not a whole spin.
            idle = SPIN - 1;
        }
        match event {
            Ok(oxrt::Event::Interrupt(_)) => {
                links.card_interrupt();
                if let Some(irq) = irq {
                    let _ = oxrt::irq_enable(irq);
                }
            }
            Ok(oxrt::Event::Control(id, len)) => {
                // A link driver's offer (a native server's: instance 0), or
                // an instance's.
                let status = match Offer::decode_any(&message[..len]) {
                    Some(offer) if offer.instance == 0 => links.offer(&offer),
                    Some(_) => service.offer(&message[..len], &mut sockets),
                    None => -EINVAL,
                };
                let _ = oxrt::ipc_reply(id, &status.to_le_bytes());
            }
            // No protocol besides the rings.
            Ok(oxrt::Event::Request(id, _)) => {
                let _ = oxrt::ipc_reply(id, &(-ENOSYS).to_le_bytes());
            }
            Ok(oxrt::Event::Doorbell | oxrt::Event::Timeout) | Err(_) => {}
        }
    }
}

const DHCP_WAIT_MS: u64 = 3000;
/// How long a renewal the tests asked for leaves netd without a lease.
const RENEW_DELAY_MS: u64 = 1000;

/// Must match the kernel's NETD_DMA_PAGES.
const DMA_BYTES: usize = 128 * 4096;
