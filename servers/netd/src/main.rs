//! netd: the network server. It drives the virtio network card from user
//! space, runs the TCP/IP stack (smoltcp), configures itself by DHCP, and
//! serves the Linux server instances' sockets over the channels they offer
//! (`service`, the protocol `netring`; docs/design/linux-server.md, R7b).
//!
//! One thread, one event loop: each round takes the channels' requests,
//! polls the card and the stack, and moves the sockets' data between
//! smoltcp and the instances' rings. While rounds make progress it polls;
//! after `SPIN` rounds without any it arms the card's interrupt and every
//! channel's doorbell and sleeps in `ipc_receive` until one of them, a
//! channel offer, or smoltcp's next timer.

#![no_std]
#![no_main]

extern crate alloc;

mod nic;
mod service;
mod virtio_net;

use alloc::vec;
use alloc::vec::Vec;
use nic::Nic;
use oxrt::println;
use smoltcp::iface::{Config, Interface, PollIngressSingleResult, PollResult, SocketSet};
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address, Ipv4Cidr};
use virtio_net::VirtioNet;

/// The heap: the sockets themselves (smoltcp's socket set, made for
/// `service::MAX_SMOLTCP` at once, about 1.8 MiB), frames in flight, the
/// channels' bookkeeping. smoltcp's socket buffers are not in it: each is
/// memory of its own, gone with the socket (`service::Region`).
const HEAP: usize = 4 << 20;

oxrt::entry!(main, heap = HEAP);

fn now() -> Instant {
    Instant::from_millis(oxrt::uptime_ms() as i64)
}

/// smoltcp's poll, with the connections that arrived given their buffers
/// between taking the frames and sending (`Service::arrivals`): their
/// SYN-ACK offers a window. True if a socket's state may have changed.
fn poll(iface: &mut Interface, nic: &mut Nic, sockets: &mut SocketSet<'static>, service: &mut service::Service) -> bool {
    let t = now();
    let mut changed = false;
    iface.poll_maintenance(t);
    loop {
        match iface.poll_ingress_single(t, nic, sockets) {
            PollIngressSingleResult::None => break,
            PollIngressSingleResult::PacketProcessed => {}
            PollIngressSingleResult::SocketStateChanged => changed = true,
        }
    }
    service.arrivals(sockets);
    while iface.poll_egress(t, nic, sockets) == PollResult::SocketStateChanged {
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

/// Rounds without progress before netd sleeps (it polls meanwhile: a
/// client's next request or bytes usually come within them).
const SPIN: u32 = 256;
/// While busy, netd looks for channel offers and interrupts every this
/// many rounds.
const OFFERS_EVERY: u32 = 64;
const ENOSYS: i64 = 38;

fn main(args: Vec<&'static str>) -> i32 {
    let (Some(io), Some(iolen), Some(irq)) = (arg(&args, "io"), arg(&args, "iolen"), arg(&args, "irq")) else {
        println!("netd: started without a network card (io=, iolen=, irq=)");
        return 1;
    };
    let (io, irq) = (io as u16, irq as u8);
    if oxrt::ioperm(io, iolen as u16).is_err() {
        println!("netd: no permission for the card's I/O ports");
        return 1;
    }
    let Ok((dma, phys)) = oxrt::dma_map() else {
        println!("netd: no DMA memory");
        return 1;
    };
    let card = match VirtioNet::new(io, dma, phys, DMA_BYTES) {
        Ok(card) => card,
        Err(e) => {
            println!("netd: {}", e);
            return 1;
        }
    };
    // Copies to and from grants fail instead of killing netd when a client
    // revokes one meanwhile.
    if let Err(e) = oxrt::copy::register() {
        println!("netd: cannot register the copy fixup: {}", e);
        return 1;
    }
    let mac = EthernetAddress(card.mac);
    let mut nic = Nic::new(card);
    if oxrt::irq_enable(irq).is_err() {
        println!("netd: cannot enable interrupt {}", irq);
        return 1;
    }
    let mut config = Config::new(mac.into());
    config.random_seed = oxrt::uptime_ms() ^ (phys << 7);
    let mut iface = Interface::new(config, &mut nic, now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::Ipv4(LOOPBACK));
    });
    // Room for every socket the service may make (and DHCP's) from the
    // start: the set never moves to a larger allocation.
    let mut sockets = SocketSet::new(Vec::with_capacity(service::MAX_SMOLTCP + 1));
    let dhcp = sockets.add(dhcpv4::Socket::new());
    // The kernel waits for the registration, so boot messages stay in
    // order: register once DHCP is done, or after DHCP_WAIT_MS without it.
    let register_at = oxrt::uptime_ms() + DHCP_WAIT_MS;
    let mut registered = false;
    let mut service = service::Service::new();
    service.config.mac = mac.0;

    // The only messages netd takes are the kernel's channel offers (a
    // longer one fails in the kernel).
    let mut message = vec![0u8; ring::channel::OFFER_BYTES];
    let mut idle = 0u32;
    let mut busy = 0u32;
    loop {
        let mut progress = service.serve(&mut iface, &mut sockets);
        progress |= poll(&mut iface, &mut nic, &mut sockets, &mut service);
        progress |= service.pump(&mut sockets);
        service.round_end(progress);
        match sockets.get_mut::<dhcpv4::Socket>(dhcp).poll() {
            Some(dhcpv4::Event::Configured(c)) => {
                // smoltcp sends from the first address unless the destination
                // shares a subnet with another one, so loopback goes last.
                iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(c.address));
                    let _ = addrs.push(IpCidr::Ipv4(LOOPBACK));
                });
                nic.address = c.address.address().to_bits();
                match c.router {
                    Some(router) => {
                        let _ = iface.routes_mut().add_default_ipv4_route(router);
                    }
                    None => {
                        iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                service.config.address = c.address.address().to_bits();
                service.config.prefix = c.address.prefix_len();
                service.config.gateway = c.router.map_or(0, |r| r.to_bits());
                service.config.dns = c.dns_servers.first().map_or(0, |d| d.to_bits());
                let mac = mac.0.map(|b| alloc::format!("{b:02x}")).join(":");
                match c.router {
                    Some(router) => println!("netd: {} via DHCP, gateway {} (virtio-net {})", c.address, router, mac),
                    None => println!("netd: {} via DHCP, no gateway (virtio-net {})", c.address, mac),
                }
                progress = true;
            }
            Some(dhcpv4::Event::Deconfigured) => {
                if nic.address != 0 {
                    println!("netd: lost the DHCP lease");
                }
                nic.address = 0;
                iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(LOOPBACK));
                });
                iface.routes_mut().remove_default_ipv4_route();
                service.config = service::Config { mac: mac.0, ..Default::default() };
                progress = true;
            }
            None => {}
        }
        service.flush();
        if !registered && (nic.address != 0 || oxrt::uptime_ms() >= register_at) {
            if nic.address == 0 {
                println!("netd: no DHCP answer yet; continuing in the background");
            }
            if let Err(e) = oxrt::ipc_register_with(netring::SERVICE, 0, oxrt::IPC_CHANNELS) {
                println!("netd: cannot register: {}", e);
                return 1;
            }
            registered = true;
        }
        let sleep = if progress || nic.has_received() {
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
            service.prepare_sleep()
        };
        let mut timeout = iface.poll_delay(now(), &sockets).map(|d| d.total_millis());
        // The service's own timers: orphans that make no progress, the end
        // of TIME-WAIT.
        if let Some(at) = service.deadline() {
            let left = (at - now().min(at)).total_millis();
            timeout = Some(timeout.map_or(left, |t| t.min(left)));
        }
        if !registered {
            let left = register_at.saturating_sub(oxrt::uptime_ms());
            timeout = Some(timeout.map_or(left, |t| t.min(left)));
        }
        // Marks that keep coming for nothing: a brief sleep only.
        if let Some(cap) = service.sleep_cap() {
            timeout = Some(timeout.map_or(cap, |t| t.min(cap)));
        }
        let event = oxrt::ipc_receive(&mut message, if sleep { timeout } else { Some(0) });
        service.awake();
        match event {
            Ok(oxrt::Event::Interrupt(_)) => {
                nic.card.ack_interrupt();
                let _ = oxrt::irq_enable(irq);
            }
            Ok(oxrt::Event::Control(id, len)) => {
                let status = service.offer(&message[..len], &mut sockets);
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
const LOOPBACK: Ipv4Cidr = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);

/// Must match the kernel's NETD_DMA_PAGES.
const DMA_BYTES: usize = 128 * 4096;
