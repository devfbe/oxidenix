//! netd: the network server. It drives the virtio network card from user
//! space, runs the TCP/IP stack (smoltcp) and configures itself by DHCP.

#![no_std]
#![no_main]

extern crate alloc;

mod nic;
mod service;
mod virtio_net;

use alloc::vec;
use alloc::vec::Vec;
use oxrt::println;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;
use nic::Nic;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address, Ipv4Cidr};
use virtio_net::VirtioNet;

oxrt::entry!(main);

fn now() -> Instant {
    Instant::from_millis(oxrt::uptime_ms() as i64)
}

/// Value of `key=...` among the arguments the kernel passed.
fn arg(args: &[&str], key: &str) -> Option<u64> {
    let v = args.iter().find_map(|a| a.strip_prefix(key)?.strip_prefix('='))?;
    match v.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

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
    let mut sockets = SocketSet::new(vec![]);
    let dhcp = sockets.add(dhcpv4::Socket::new());
    // The kernel waits for the registration, so boot messages stay in
    // order: register once DHCP is done, or after DHCP_WAIT_MS without it.
    let register_at = oxrt::uptime_ms() + DHCP_WAIT_MS;
    let mut registered = false;
    let mut service = service::Service::new();
    service.config.mac = mac.0;

    let mut request = vec![0u8; 64 * 1024];
    loop {
        iface.poll(now(), &mut nic, &mut sockets);
        service.progress(&mut iface, &mut sockets, now());
        service.announce(&sockets);
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
            }
            None => {}
        }
        if !registered && (nic.address != 0 || oxrt::uptime_ms() >= register_at) {
            if nic.address == 0 {
                println!("netd: no DHCP answer yet; continuing in the background");
            }
            if let Err(e) = oxrt::ipc_register("net", 0) {
                println!("netd: cannot register: {}", e);
                return 1;
            }
            registered = true;
        }
        let mut timeout = iface.poll_delay(now(), &sockets).map(|d| d.total_millis());
        if !registered {
            let left = register_at.saturating_sub(oxrt::uptime_ms());
            timeout = Some(timeout.map_or(left, |t| t.min(left)));
        }
        if nic.has_received() {
            continue;
        }
        match oxrt::ipc_receive(&mut request, timeout) {
            Ok(oxrt::Event::Interrupt(_)) => {
                nic.card.ack_interrupt();
                let _ = oxrt::irq_enable(irq);
            }
            Ok(oxrt::Event::Request(id, len)) => match netproto::decode_request(&request[..len]) {
                Some(netproto::Request { op: Some(op), args, payload }) => {
                    if let Some((status, values, data)) = service.handle(id, op, args, payload, &mut iface, &mut sockets, now()) {
                        service.respond(id, status, values, &data);
                    }
                    service.announce(&sockets);
                }
                _ => service.respond(id, -EINVAL, [0; 6], &[]),
            },
            Ok(oxrt::Event::Timeout) | Err(_) => {}
        }
    }
}

const DHCP_WAIT_MS: u64 = 3000;
const LOOPBACK: Ipv4Cidr = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);
const EINVAL: i64 = 22;

/// Must match the kernel's NETD_DMA_PAGES.
const DMA_BYTES: usize = 128 * 4096;
