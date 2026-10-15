use super::*;

use crate::socket::tcp::Socket;

impl InterfaceInner {
    pub(crate) fn process_tcp<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        handled_by_raw_socket: bool,
        ip_repr: IpRepr,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());

        // Per RFC 1122 §3.2.1.3, the unspecified address must never appear as a source
        // or destination in any IP datagram. Drop such TCP segments early to avoid
        // creating sockets with unspecified peers (which would later panic on egress).
        // This is not done at the iface level because it might be useful with
        // UDP or raw sockets, but it's definitely not useful for TCP.
        if src_addr.is_unspecified() || dst_addr.is_unspecified() {
            return None;
        }

        let tcp_packet = check!(TcpPacket::new_checked(ip_payload));
        let tcp_repr = check!(TcpRepr::parse(
            &tcp_packet,
            &src_addr,
            &dst_addr,
            &self.caps.checksum
        ));

        // oxidenix: a new connection's SYN ends a TIME-WAIT of its 4-tuple
        // (RFC 9293 3.10.7.4), so that a listener takes it, wherever the
        // two sockets are in the set.
        if tcp_repr.control == TcpControl::Syn && tcp_repr.ack_number.is_none() {
            for tcp_socket in sockets.items_mut().filter_map(|i| Socket::downcast_mut(&mut i.socket)) {
                if tcp_socket.yields_to_syn(self, &ip_repr, &tcp_repr) {
                    tcp_socket.end_time_wait();
                }
            }
        }

        // oxidenix: the socket of the segment's own 4-tuple first, then a
        // listener on its address, then one on any address (as Linux's
        // lookup scores them), wherever they are in the set: of listeners
        // sharing a port (SO_REUSEPORT) the most specific takes it.
        let mut listener: Option<(bool, SocketHandle)> = None;
        for item in sockets.items_mut() {
            let handle = item.meta.handle;
            let Some(tcp_socket) = Socket::downcast_mut(&mut item.socket) else {
                continue;
            };
            if tcp_socket.accepts(self, &ip_repr, &tcp_repr) {
                if tcp_socket.has_tuple() {
                    return tcp_socket
                        .process(self, &ip_repr, &tcp_repr)
                        .map(|(ip, tcp)| Packet::new(ip, IpPayload::Tcp(tcp)));
                }
                let specific = tcp_socket.listen_endpoint().addr.is_some();
                if listener.is_none_or(|(s, _)| specific && !s) {
                    listener = Some((specific, handle));
                }
            }
        }
        if let Some((_, handle)) = listener {
            return sockets
                .get_mut::<Socket>(handle)
                .process(self, &ip_repr, &tcp_repr)
                .map(|(ip, tcp)| Packet::new(ip, IpPayload::Tcp(tcp)));
        }

        if tcp_repr.control == TcpControl::Rst
            || ip_repr.dst_addr().is_unspecified()
            || ip_repr.src_addr().is_unspecified()
            || handled_by_raw_socket
        {
            // Never reply to a TCP RST packet with another TCP RST packet.
            // Never send a TCP RST packet with unspecified addresses.
            // Never send a TCP RST when packet has been handled by raw socket.
            None
        } else {
            // The packet wasn't handled by a socket, send a TCP RST packet.
            let (ip, tcp) = tcp::Socket::rst_reply(&ip_repr, &tcp_repr);
            Some(Packet::new(ip, IpPayload::Tcp(tcp)))
        }
    }
}
