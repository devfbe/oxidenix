//! netd: the network server. It drives the virtio network card from user
//! space, runs the TCP/IP stack (smoltcp) and configures itself by DHCP.

#![no_std]
#![no_main]

extern crate alloc;

mod virtio;

use alloc::vec;
use alloc::vec::Vec;
use oxrt::println;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr};
use virtio::VirtioNet;

oxrt::entry!(main);

/// smoltcp's view of the card.
struct Nic(VirtioNet);

struct RxToken(Vec<u8>);
struct TxToken<'a>(&'a mut VirtioNet);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.0.send(len, f)
    }
}

impl Device for Nic {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _now: Instant) -> Option<(RxToken, TxToken<'_>)> {
        if !self.0.can_send() {
            return None;
        }
        let frame = self.0.receive(|f| f.to_vec())?;
        Some((RxToken(frame), TxToken(&mut self.0)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        self.0.can_send().then(|| TxToken(&mut self.0))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = virtio::MTU;
        caps
    }
}

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
    let mut nic = Nic(card);
    if oxrt::irq_enable(irq).is_err() {
        println!("netd: cannot enable interrupt {}", irq);
        return 1;
    }
    let mut config = Config::new(mac.into());
    config.random_seed = oxrt::uptime_ms() ^ (phys << 7);
    let mut iface = Interface::new(config, &mut nic, now());
    let mut sockets = SocketSet::new(vec![]);
    let dhcp = sockets.add(dhcpv4::Socket::new());
    // The kernel waits for the registration, so boot messages stay in
    // order: register once DHCP is done, or after DHCP_WAIT_MS without it.
    let register_at = oxrt::uptime_ms() + DHCP_WAIT_MS;
    let mut registered = false;

    let mut request = vec![0u8; 64 * 1024];
    loop {
        iface.poll(now(), &mut nic, &mut sockets);
        match sockets.get_mut::<dhcpv4::Socket>(dhcp).poll() {
            Some(dhcpv4::Event::Configured(c)) => {
                iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(c.address));
                });
                match c.router {
                    Some(router) => {
                        let _ = iface.routes_mut().add_default_ipv4_route(router);
                    }
                    None => {
                        iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                let mac = mac.0.map(|b| alloc::format!("{b:02x}")).join(":");
                match c.router {
                    Some(router) => println!("netd: {} via DHCP, gateway {} (virtio-net {})", c.address, router, mac),
                    None => println!("netd: {} via DHCP, no gateway (virtio-net {})", c.address, mac),
                }
            }
            Some(dhcpv4::Event::Deconfigured) => {
                if !iface.ip_addrs().is_empty() {
                    println!("netd: lost the DHCP lease");
                }
                iface.update_ip_addrs(|addrs| addrs.clear());
                iface.routes_mut().remove_default_ipv4_route();
            }
            None => {}
        }
        if !registered && (!iface.ip_addrs().is_empty() || oxrt::uptime_ms() >= register_at) {
            if iface.ip_addrs().is_empty() {
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
        if nic.0.has_received() {
            continue;
        }
        match oxrt::ipc_receive(&mut request, timeout) {
            Ok(oxrt::Event::Interrupt(_)) => {
                nic.0.ack_interrupt();
                let _ = oxrt::irq_enable(irq);
            }
            Ok(oxrt::Event::Request(id, _)) => {
                let _ = oxrt::ipc_reply(id, &[]);
            }
            Ok(oxrt::Event::Timeout) | Err(_) => {}
        }
    }
}

const DHCP_WAIT_MS: u64 = 3000;

/// Must match the kernel's NETD_DMA_PAGES.
const DMA_BYTES: usize = 128 * 4096;
