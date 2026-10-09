//! The network interfaces as Linux programs see them: netd's description
//! (`SYS_NET_LINKS`, `netproto::Link`) with Linux's names and flags, for
//! rtnetlink (`netlink`) and for the interface requests every socket takes
//! (netdevice(7): SIOCGIFCONF, SIOCGIFINDEX, SIOCGIFFLAGS, ...). The
//! configuration is netd's (DHCP): the requests that would change it are
//! not taken.

use crate::files;
use crate::syscall;
use crate::usercopy;
use alloc::string::String;
use alloc::vec::Vec;
use netlink::{Interface, Ipv4};
use restricted::*;

const AF_INET: u16 = 2;

const SIOCGIFNAME: u64 = 0x8910;
const SIOCGIFCONF: u64 = 0x8912;
const SIOCGIFFLAGS: u64 = 0x8913;
const SIOCGIFADDR: u64 = 0x8915;
const SIOCGIFBRDADDR: u64 = 0x8919;
const SIOCGIFNETMASK: u64 = 0x891b;
const SIOCGIFMETRIC: u64 = 0x891d;
const SIOCGIFMTU: u64 = 0x8921;
const SIOCGIFHWADDR: u64 = 0x8927;
const SIOCGIFINDEX: u64 = 0x8933;
const SIOCGIFTXQLEN: u64 = 0x8942;

/// `struct ifreq`: the name, then a union of 24 bytes.
const IFREQ: usize = 40;
const IFNAMSIZ: usize = 16;

const ENOTTY: i64 = 25;
const ENODEV: i64 = 19;
const EADDRNOTAVAIL: i64 = 99;
const S_IFMT: u32 = 0o170000;
const S_IFSOCK: u32 = 0o140000;

/// The interfaces as netd describes them, named as Linux would: the
/// loopback "lo", Ethernet cards "eth0" and up. None without netd (the
/// lists are empty then, not an error).
pub fn interfaces() -> Vec<Interface> {
    let mut buf = alloc::vec![0u8; 64 * netproto::Link::SIZE];
    let n = syscall(SYS_NET_LINKS, [buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0]);
    if n < 0 {
        return Vec::new();
    }
    let mut ethernet = 0;
    netproto::Link::decode_all(&buf[..n as usize])
        .map(|l| {
            let loopback = l.kind == netproto::LINK_LOOPBACK;
            let name = if loopback {
                String::from("lo")
            } else {
                ethernet += 1;
                alloc::format!("eth{}", ethernet - 1)
            };
            let mut flags = if loopback { netlink::IFF_LOOPBACK } else { netlink::IFF_BROADCAST };
            if l.state & netproto::LINK_UP != 0 {
                flags |= netlink::IFF_UP;
                if l.state & netproto::LINK_RUNNING != 0 {
                    flags |= netlink::IFF_RUNNING | netlink::IFF_LOWER_UP;
                }
            }
            let ipv4 = (l.address != 0).then_some(Ipv4 {
                address: l.address,
                prefix: l.prefix,
                scope: if loopback { netlink::RT_SCOPE_HOST } else { netlink::RT_SCOPE_UNIVERSE },
                // The loopback's is configured; the card's leased by DHCP.
                permanent: loopback,
            });
            Interface {
                index: l.index,
                name,
                hw_type: if loopback { netlink::ARPHRD_LOOPBACK } else { netlink::ARPHRD_ETHER },
                flags,
                mtu: l.mtu,
                mac: l.mac,
                broadcast_mac: if loopback { [0; 6] } else { [0xff; 6] },
                ipv4,
            }
        })
        .collect()
}

/// Whether `request` is one of the interface requests answered here.
pub fn is_request(request: u64) -> bool {
    matches!(
        request,
        SIOCGIFNAME | SIOCGIFCONF | SIOCGIFFLAGS | SIOCGIFADDR | SIOCGIFBRDADDR | SIOCGIFNETMASK | SIOCGIFMETRIC | SIOCGIFMTU | SIOCGIFHWADDR | SIOCGIFINDEX | SIOCGIFTXQLEN
    )
}

/// A `struct sockaddr_in` of an address (host order), port 0.
fn sockaddr_in(address: u32) -> [u8; 16] {
    let mut sa = [0u8; 16];
    sa[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    sa[4..8].copy_from_slice(&address.to_be_bytes());
    sa
}

fn netmask(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - prefix.min(32) as u32) }
}

/// ioctl(fd, request, arg) for an interface request: on a socket of any
/// kind (ENOTTY on another file), as netdevice(7) describes.
pub fn ioctl(fd: u64, request: u64, arg: u64) -> Result<i64, i64> {
    if files::stat_of(fd)?.mode & S_IFMT != S_IFSOCK {
        return Err(ENOTTY);
    }
    let all = interfaces();
    if request == SIOCGIFCONF {
        // struct ifconf: the buffer's length, then the buffer; a null
        // buffer asks for the length needed. One entry per IPv4 address.
        let len: i32 = usercopy::read(arg)?;
        let buf: u64 = usercopy::read(arg + 8)?;
        let with_address: Vec<&Interface> = all.iter().filter(|i| i.ipv4.is_some()).collect();
        if buf == 0 {
            usercopy::write(arg, &((with_address.len() * IFREQ) as i32))?;
            return Ok(0);
        }
        let mut used = 0;
        for i in with_address {
            if used + IFREQ > len.max(0) as usize {
                break;
            }
            let mut req = [0u8; IFREQ];
            let name = i.name.as_bytes();
            req[..name.len().min(IFNAMSIZ - 1)].copy_from_slice(&name[..name.len().min(IFNAMSIZ - 1)]);
            req[IFNAMSIZ..IFNAMSIZ + 16].copy_from_slice(&sockaddr_in(i.ipv4.map_or(0, |a| a.address)));
            usercopy::to_program(buf + used as u64, &req)?;
            used += IFREQ;
        }
        usercopy::write(arg, &(used as i32))?;
        return Ok(0);
    }
    let mut req = [0u8; IFREQ];
    usercopy::from_program(arg, &mut req)?;
    let found = if request == SIOCGIFNAME {
        let index = i32::from_le_bytes(req[IFNAMSIZ..IFNAMSIZ + 4].try_into().expect("4 bytes"));
        all.iter().find(|i| i.index as i32 == index)
    } else {
        let end = req[..IFNAMSIZ].iter().position(|&b| b == 0).unwrap_or(IFNAMSIZ);
        let name = &req[..end];
        all.iter().find(|i| i.name.as_bytes() == name)
    };
    let i = found.ok_or(ENODEV)?;
    let u = IFNAMSIZ;
    match request {
        SIOCGIFNAME => {
            req[..u].fill(0);
            let name = i.name.as_bytes();
            req[..name.len().min(u - 1)].copy_from_slice(&name[..name.len().min(u - 1)]);
        }
        // The flags' low 16 bits, as a short.
        SIOCGIFFLAGS => req[u..u + 2].copy_from_slice(&(i.flags as u16).to_le_bytes()),
        SIOCGIFADDR | SIOCGIFBRDADDR | SIOCGIFNETMASK => {
            let a = i.ipv4.ok_or(EADDRNOTAVAIL)?;
            let value = match request {
                SIOCGIFADDR => a.address,
                SIOCGIFNETMASK => netmask(a.prefix),
                _ if i.flags & netlink::IFF_BROADCAST != 0 && a.prefix < 31 => a.address | !netmask(a.prefix),
                _ => 0,
            };
            req[u..u + 16].copy_from_slice(&sockaddr_in(value));
        }
        SIOCGIFMETRIC => req[u..u + 4].copy_from_slice(&0i32.to_le_bytes()),
        SIOCGIFMTU => req[u..u + 4].copy_from_slice(&(i.mtu as i32).to_le_bytes()),
        SIOCGIFINDEX => req[u..u + 4].copy_from_slice(&(i.index as i32).to_le_bytes()),
        // No transmit queue of the kind Linux counts (netd sends at once).
        SIOCGIFTXQLEN => req[u..u + 4].copy_from_slice(&0i32.to_le_bytes()),
        SIOCGIFHWADDR => {
            // struct sockaddr: the hardware type as its family, the address.
            req[u..u + 16].fill(0);
            req[u..u + 2].copy_from_slice(&i.hw_type.to_le_bytes());
            req[u + 2..u + 8].copy_from_slice(&i.mac);
        }
        _ => return Err(ENOTTY),
    }
    usercopy::to_program(arg, &req)?;
    Ok(0)
}
