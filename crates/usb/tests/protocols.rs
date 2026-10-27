//! The USB protocols on the host: descriptors as real devices send them
//! (QEMU's usb-net, an Android phone's RNDIS and NCM tethering functions)
//! and malformed ones, the choice of configuration and function, RNDIS's
//! messages and packets, NCM's transfer blocks both ways, CDC's
//! notifications, hubs' port status.

use usb::descriptor::{self, Configuration, Device};
use usb::function::{self, Kind};
use usb::{cdc, hub, ncm, rndis, SetupPacket, Speed};

/// Descriptor bytes, built descriptor by descriptor; the configuration's
/// total length is patched in at the end.
struct Desc(Vec<u8>);

impl Desc {
    fn config(value: u8, interfaces: u8) -> Desc {
        Desc(vec![9, descriptor::CONFIGURATION, 0, 0, interfaces, value, 0, 0x80, 50])
    }
    fn iad(mut self, first: u8, count: u8, class: u8, sub: u8, proto: u8) -> Desc {
        self.0.extend_from_slice(&[8, descriptor::INTERFACE_ASSOCIATION, first, count, class, sub, proto, 0]);
        self
    }
    fn interface(mut self, number: u8, alt: u8, endpoints: u8, class: u8, sub: u8, proto: u8) -> Desc {
        self.0.extend_from_slice(&[9, descriptor::INTERFACE, number, alt, endpoints, class, sub, proto, 0]);
        self
    }
    fn functional(mut self, subtype: u8, data: &[u8]) -> Desc {
        self.0.extend_from_slice(&[3 + data.len() as u8, descriptor::CS_INTERFACE, subtype]);
        self.0.extend_from_slice(data);
        self
    }
    fn endpoint(mut self, address: u8, kind: u8, max_packet: u16, interval: u8) -> Desc {
        let mp = max_packet.to_le_bytes();
        self.0.extend_from_slice(&[7, descriptor::ENDPOINT, address, kind, mp[0], mp[1], interval]);
        self
    }
    fn bytes(mut self) -> Vec<u8> {
        let total = (self.0.len() as u16).to_le_bytes();
        self.0[2] = total[0];
        self.0[3] = total[1];
        self.0
    }
}

const HEADER: &[u8] = &[0x10, 0x01];
/// The Ethernet functional descriptor: iMACAddress 3, no statistics,
/// 1514-byte segments, no filters.
const ETHERNET: &[u8] = &[3, 0, 0, 0, 0, 0xea, 0x05, 0, 0, 0];

/// QEMU's usb-net (hw/usb/dev-network.c): CDC Ethernet in configuration
/// 1, RNDIS in configuration 2, full speed.
fn qemu_usb_net() -> Vec<Vec<u8>> {
    let ecm = Desc::config(1, 2)
        .interface(0, 0, 1, cdc::CLASS_COMM, cdc::SUBCLASS_ECM, 0)
        .functional(cdc::FUNC_HEADER, HEADER)
        .functional(cdc::FUNC_UNION, &[0, 1])
        .functional(cdc::FUNC_ETHERNET, ETHERNET)
        .endpoint(0x81, descriptor::INTERRUPT, 16, 0x20)
        .interface(1, 0, 0, cdc::CLASS_DATA, 0, 0)
        .interface(1, 1, 2, cdc::CLASS_DATA, 0, 0)
        .endpoint(0x82, descriptor::BULK, 64, 0)
        .endpoint(0x02, descriptor::BULK, 64, 0)
        .bytes();
    let rndis = Desc::config(2, 2)
        .interface(0, 0, 1, cdc::CLASS_COMM, cdc::SUBCLASS_ACM, 0xff)
        .functional(cdc::FUNC_HEADER, HEADER)
        .functional(0x01, &[0, 1])
        .functional(0x02, &[0])
        .functional(cdc::FUNC_UNION, &[0, 1])
        .endpoint(0x81, descriptor::INTERRUPT, 8, 0x20)
        .interface(1, 0, 2, cdc::CLASS_DATA, 0, 0)
        .endpoint(0x82, descriptor::BULK, 64, 0)
        .endpoint(0x02, descriptor::BULK, 64, 0)
        .bytes();
    vec![ecm, rndis]
}

/// An Android phone in RNDIS tethering with ADB: an interface
/// association of class "wireless controller / RF / RNDIS", high speed.
fn android_rndis() -> Vec<u8> {
    Desc::config(1, 3)
        .iad(0, 2, 0xe0, 0x01, 0x03)
        .interface(0, 0, 1, 0xe0, 0x01, 0x03)
        .functional(cdc::FUNC_HEADER, HEADER)
        .functional(0x01, &[0, 1])
        .functional(0x02, &[0])
        .functional(cdc::FUNC_UNION, &[0, 1])
        .endpoint(0x81, descriptor::INTERRUPT, 8, 9)
        .interface(1, 0, 2, cdc::CLASS_DATA, 0, 0)
        .endpoint(0x82, descriptor::BULK, 512, 0)
        .endpoint(0x01, descriptor::BULK, 512, 0)
        .interface(2, 0, 2, 0xff, 0x42, 0x01)
        .endpoint(0x83, descriptor::BULK, 512, 0)
        .endpoint(0x02, descriptor::BULK, 512, 0)
        .bytes()
}

/// An Android phone in NCM tethering.
fn android_ncm() -> Vec<u8> {
    Desc::config(1, 2)
        .iad(0, 2, cdc::CLASS_COMM, cdc::SUBCLASS_NCM, 0)
        .interface(0, 0, 1, cdc::CLASS_COMM, cdc::SUBCLASS_NCM, 0)
        .functional(cdc::FUNC_HEADER, HEADER)
        .functional(cdc::FUNC_UNION, &[0, 1])
        .functional(cdc::FUNC_ETHERNET, ETHERNET)
        .functional(cdc::FUNC_NCM, &[0x00, 0x01, 0x00])
        .endpoint(0x81, descriptor::INTERRUPT, 16, 9)
        .interface(1, 0, 0, cdc::CLASS_DATA, 0, cdc::PROTOCOL_NCM_DATA)
        .interface(1, 1, 2, cdc::CLASS_DATA, 0, cdc::PROTOCOL_NCM_DATA)
        .endpoint(0x82, descriptor::BULK, 512, 0)
        .endpoint(0x01, descriptor::BULK, 512, 0)
        .bytes()
}

#[test]
fn the_device_descriptor_parses() {
    let d = [18, 1, 0x00, 0x02, 2, 0, 0, 64, 0x25, 0x05, 0xa2, 0xa4, 0, 1, 1, 2, 3, 2];
    let dev = Device::parse(&d).unwrap();
    assert_eq!((dev.vendor, dev.product, dev.configurations, dev.max_packet0), (0x0525, 0xa4a2, 2, 64));
    assert_eq!(dev.max_packet0_bytes(Speed::High), Some(64));
    assert_eq!(dev.max_packet0_bytes(Speed::Low), None, "64 is no low-speed size");
    assert_eq!(Device::parse(&d[..17]), None);
    let mut wrong = d;
    wrong[1] = 2;
    assert_eq!(Device::parse(&wrong), None);
}

#[test]
fn qemus_usb_net_gets_ecm_from_its_first_configuration() {
    let configs: Vec<Configuration> = qemu_usb_net().iter().map(|b| Configuration::parse(b).unwrap()).collect();
    let (n, f) = function::choose(&configs).unwrap();
    assert_eq!(n, 0);
    assert_eq!(f.kind, Kind::Ecm);
    assert_eq!((f.control, f.data, f.data_alternate), (0, 1, 1));
    assert_eq!((f.bulk_in.address, f.bulk_out.address), (0x82, 0x02));
    assert_eq!(f.notify.map(|e| e.address), Some(0x81));
    assert_eq!((f.mac_string, f.max_segment), (3, 1514));
    // Forced (the kernel's test mode): its second configuration's RNDIS.
    let r = function::find_kind(&configs[1], Kind::Rndis).unwrap();
    assert_eq!((r.control, r.data, r.data_alternate, r.mac_string), (0, 1, 0, 0));
}

#[test]
fn androids_rndis_function_is_found_beside_adb() {
    let config = Configuration::parse(&android_rndis()).unwrap();
    assert_eq!(config.associations.len(), 1);
    assert_eq!(config.interface_numbers().collect::<Vec<_>>(), vec![0, 1, 2]);
    let f = function::find(&config).unwrap();
    assert_eq!(f.kind, Kind::Rndis);
    assert_eq!((f.bulk_in.address, f.bulk_out.address, f.bulk_in.packet_size()), (0x82, 0x01, 512));
    assert_eq!(f.kind.driver(), "rndis_host");
}

#[test]
fn androids_ncm_function_is_preferred_and_wired() {
    let config = Configuration::parse(&android_ncm()).unwrap();
    let f = function::find(&config).unwrap();
    assert_eq!(f.kind, Kind::Ncm);
    assert_eq!((f.data, f.data_alternate), (1, 1));
    // A device with RNDIS in one configuration and NCM in another: NCM.
    let configs = vec![Configuration::parse(&android_rndis()).unwrap(), config];
    assert_eq!(function::choose(&configs).map(|(n, f)| (n, f.kind)), Some((1, Kind::Ncm)));
}

#[test]
fn functions_without_what_they_need_are_not_offered() {
    // ECM without its Ethernet descriptor (no MAC address).
    let no_mac = Desc::config(1, 2)
        .interface(0, 0, 0, cdc::CLASS_COMM, cdc::SUBCLASS_ECM, 0)
        .functional(cdc::FUNC_UNION, &[0, 1])
        .interface(1, 1, 2, cdc::CLASS_DATA, 0, 0)
        .endpoint(0x82, descriptor::BULK, 64, 0)
        .endpoint(0x02, descriptor::BULK, 64, 0)
        .bytes();
    assert_eq!(function::find(&Configuration::parse(&no_mac).unwrap()), None);
    // A data interface without a bulk OUT endpoint.
    let one_way = Desc::config(1, 2)
        .interface(0, 0, 0, cdc::CLASS_COMM, cdc::SUBCLASS_ECM, 0)
        .functional(cdc::FUNC_UNION, &[0, 1])
        .functional(cdc::FUNC_ETHERNET, ETHERNET)
        .interface(1, 1, 1, cdc::CLASS_DATA, 0, 0)
        .endpoint(0x82, descriptor::BULK, 64, 0)
        .bytes();
    assert_eq!(function::find(&Configuration::parse(&one_way).unwrap()), None);
    // A union that names another control interface.
    let crossed = Desc::config(1, 2)
        .interface(0, 0, 0, cdc::CLASS_COMM, cdc::SUBCLASS_ECM, 0)
        .functional(cdc::FUNC_UNION, &[5, 1])
        .functional(cdc::FUNC_ETHERNET, ETHERNET)
        .interface(1, 1, 2, cdc::CLASS_DATA, 0, 0)
        .endpoint(0x82, descriptor::BULK, 64, 0)
        .endpoint(0x02, descriptor::BULK, 64, 0)
        .bytes();
    assert_eq!(function::find(&Configuration::parse(&crossed).unwrap()), None);
}

#[test]
fn malformed_configurations_keep_what_came_before() {
    let mut b = android_ncm();
    // A descriptor of length 0 in the middle ends the walk there.
    let at = 9 + 8 + 9;
    b[at] = 0;
    let c = Configuration::parse(&b).unwrap();
    assert_eq!(c.interfaces.len(), 1);
    assert!(c.interfaces[0].functional.is_empty());
    // A total length beyond the bytes, a header that is no configuration's.
    let mut long = android_ncm();
    long[2] = 0xff;
    long[3] = 0xff;
    assert_eq!(Configuration::parse(&long).map(|c| c.interfaces.len()), Some(3));
    assert_eq!(Configuration::parse(&[9, 4, 0, 0, 0, 0, 0, 0, 0]), None);
    assert_eq!(Configuration::parse(&[2, 2]), None);
    // Every prefix of a real one parses without panicking.
    let full = android_rndis();
    for n in 0..full.len() {
        let _ = Configuration::parse(&full[..n]).map(|c| function::find(&c));
    }
}

#[test]
fn strings_and_mac_addresses() {
    let text: Vec<u8> = [26u8, 3].into_iter().chain("5255C0FFEE01".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
    let s = descriptor::string(&text).unwrap();
    assert_eq!(s, "5255C0FFEE01");
    assert_eq!(cdc::mac_from_string(&s), Some([0x52, 0x55, 0xc0, 0xff, 0xee, 0x01]));
    assert_eq!(cdc::mac_from_string("5255C0FFEE0"), None);
    assert_eq!(cdc::mac_from_string("5255C0FFEE0G"), None);
    assert_eq!(descriptor::languages(&[4, 3, 0x09, 0x04]), vec![0x0409]);
    assert_eq!(descriptor::languages(&[4, 2, 0x09, 0x04]), Vec::<u16>::new());
    // An odd length, a lone surrogate: what decodes, no panic.
    assert_eq!(descriptor::string(&[5, 3, b'A', 0, b'B']).as_deref(), Some("A"));
    assert_eq!(descriptor::string(&[4, 3, 0x00, 0xd8]).as_deref(), Some("\u{fffd}"));
}

#[test]
fn setup_packets_encode_as_usb_says() {
    let s = SetupPacket::get_descriptor(descriptor::CONFIGURATION, 1, 0, 9);
    assert_eq!(s.to_bytes(), [0x80, 6, 1, 2, 0, 0, 9, 0]);
    assert!(s.is_in());
    assert_eq!(SetupPacket::set_interface(1, 1).to_bytes(), [0x01, 11, 1, 0, 1, 0, 0, 0]);
    assert_eq!(SetupPacket::clear_halt(0x82).to_bytes(), [0x02, 1, 0, 0, 0x82, 0, 0, 0]);
    let filter = SetupPacket::class_interface(cdc::SET_ETHERNET_PACKET_FILTER, 0x0e, 0, 0, false);
    assert_eq!(filter.to_bytes(), [0x21, 0x43, 0x0e, 0, 0, 0, 0, 0]);
    assert_eq!(s.to_u64() & 0xff, 0x80);
}

#[test]
fn rndis_control_messages() {
    let init = rndis::initialize(1, rndis::MAX_TRANSFER);
    assert_eq!(init.len(), 24);
    assert_eq!(&init[..8], &[2, 0, 0, 0, 24, 0, 0, 0]);
    let mut cmplt = vec![0u8; 52];
    for (i, w) in [rndis::INITIALIZE_MSG | rndis::COMPLETION, 52, 1, 0, 1, 0, 1, 0, 1, 1558, 0, 0, 0].iter().enumerate() {
        cmplt[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    match rndis::Response::parse(&cmplt) {
        Some(rndis::Response::Initialized { request: 1, status: 0, info }) => assert_eq!((info.max_packets_per_transfer, info.max_transfer), (1, 1558)),
        other => panic!("{other:?}"),
    }
    // A query's answer: the permanent address, the buffer at offset 16
    // from the request id.
    let mut q = Vec::new();
    for w in [rndis::QUERY_MSG | rndis::COMPLETION, 30, 2, 0, 6, 16] {
        q.extend_from_slice(&u32::to_le_bytes(w));
    }
    q.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x2a]);
    assert_eq!(rndis::Response::parse(&q), Some(rndis::Response::Queried { request: 2, status: 0, info: vec![2, 0, 0, 0, 0, 0x2a] }));
    // The buffer beyond the message: refused.
    q[16] = 60;
    assert_eq!(rndis::Response::parse(&q), None);
    // A length beyond the bytes; an unknown kind.
    assert_eq!(rndis::Response::parse(&[4, 0, 0, 0x80, 64, 0, 0, 0]), None);
    assert_eq!(rndis::Response::parse(&[9, 0, 0, 0, 8, 0, 0, 0]), None);
    // The device's keep-alive, and the media indications.
    let mut k = Vec::new();
    for w in [rndis::KEEPALIVE_MSG, 12, 77] {
        k.extend_from_slice(&u32::to_le_bytes(w));
    }
    assert_eq!(rndis::Response::parse(&k), Some(rndis::Response::KeepAlive { request: 77 }));
    assert_eq!(&rndis::keepalive_complete(77)[8..12], &77u32.to_le_bytes());
    let set = rndis::set(3, rndis::OID_GEN_CURRENT_PACKET_FILTER, &0x0fu32.to_le_bytes());
    assert_eq!(set.len(), 32);
    assert_eq!(&set[20..24], &20u32.to_le_bytes());
    assert!(rndis::is_response_available(&[1, 0, 0, 0, 0, 0, 0, 0]));
    assert!(!rndis::is_response_available(&[2, 0, 0, 0, 0, 0, 0, 0]));
}

fn packet(frame: &[u8]) -> Vec<u8> {
    let mut p = rndis::packet_header(frame.len()).to_vec();
    p.extend_from_slice(frame);
    p
}

#[test]
fn rndis_packets_in_one_transfer() {
    let (a, b) = (vec![0xaa; 60], vec![0xbb; 1514]);
    let mut t = packet(&a);
    t.extend(packet(&b));
    // A message of another kind between them is skipped.
    let mut skip = vec![0u8; 12];
    skip[..4].copy_from_slice(&rndis::INDICATE_STATUS_MSG.to_le_bytes());
    skip[4..8].copy_from_slice(&12u32.to_le_bytes());
    t.extend(skip);
    t.extend(packet(&a));
    let frames: Vec<&[u8]> = rndis::packets(&t).collect();
    assert_eq!(frames, vec![&a[..], &b[..], &a[..]]);
    // A length that runs past the transfer ends it; what came before stands.
    let mut bad = packet(&a);
    let mut second = packet(&b);
    second[4..8].copy_from_slice(&5000u32.to_le_bytes());
    bad.extend(second);
    assert_eq!(rndis::packets(&bad).count(), 1);
    // Data beyond its message.
    let mut beyond = packet(&a);
    beyond[12..16].copy_from_slice(&100u32.to_le_bytes());
    assert_eq!(rndis::packets(&beyond).count(), 0);
    // Trailing zero padding ends quietly.
    let mut padded = packet(&a);
    padded.extend([0u8; 1]);
    assert_eq!(rndis::packets(&padded).count(), 1);
    for n in 0..t.len() {
        let _ = rndis::packets(&t[..n]).count();
    }
}

fn params() -> ncm::Parameters {
    let mut b = vec![0u8; ncm::PARAMETERS_BYTES];
    b[0..2].copy_from_slice(&28u16.to_le_bytes());
    b[2..4].copy_from_slice(&1u16.to_le_bytes());
    b[4..8].copy_from_slice(&16384u32.to_le_bytes());
    b[8..10].copy_from_slice(&4u16.to_le_bytes());
    b[12..14].copy_from_slice(&4u16.to_le_bytes());
    b[16..20].copy_from_slice(&16384u32.to_le_bytes());
    b[20..22].copy_from_slice(&4u16.to_le_bytes());
    b[22..24].copy_from_slice(&2u16.to_le_bytes());
    b[24..26].copy_from_slice(&4u16.to_le_bytes());
    ncm::Parameters::parse(&b).unwrap()
}

#[test]
fn ncm_parameters_are_checked() {
    let p = params();
    assert_eq!((p.out_max_size, p.out_divisor, p.out_remainder), (16384, 4, 2));
    let mut b = vec![0u8; ncm::PARAMETERS_BYTES];
    b[2] = 2; // NTB32 only
    assert_eq!(ncm::Parameters::parse(&b), None);
    assert_eq!(ncm::Parameters::parse(&b[..27]), None);
}

#[test]
fn ncm_blocks_survive_the_round_trip() {
    let p = params();
    let mut buf = vec![0u8; 16384];
    let frames: Vec<Vec<u8>> = (0..10).map(|i| (0..(60 + i * 97)).map(|j| (i * 13 + j) as u8).collect()).collect();
    let mut b = ncm::Builder::new(&mut buf, p, 16, 7);
    for f in &frames {
        assert!(b.push_with(f.len(), |d| d.copy_from_slice(f)));
    }
    let len = b.finish();
    let mut out = [(0, 0); ncm::MAX_DATAGRAMS];
    let n = ncm::datagrams(&buf[..len], &mut out).unwrap();
    assert_eq!(n, frames.len());
    for (i, f) in frames.iter().enumerate() {
        let (s, e) = out[i];
        assert_eq!(s % 4, 2, "placed at the remainder");
        assert_eq!(&buf[s..e], &f[..]);
    }
    // A full block refuses more.
    let mut small = vec![0u8; 2048];
    let mut b = ncm::Builder::new(&mut small, p, 16, 0);
    assert!(b.push_with(1514, |d| d.fill(1)));
    assert!(!b.push_with(1514, |d| d.fill(1)));
    let len = b.finish();
    assert_eq!(ncm::datagrams(&small[..len], &mut out), Some(1));
}

#[test]
fn malformed_ncm_blocks_are_refused_or_cut() {
    let p = params();
    let mut buf = vec![0u8; 4096];
    let mut b = ncm::Builder::new(&mut buf, p, 4, 1);
    b.push_with(100, |d| d.fill(7));
    b.push_with(200, |d| d.fill(8));
    let len = b.finish();
    let mut out = [(0, 0); ncm::MAX_DATAGRAMS];
    // Wrong signature.
    let mut bad = buf[..len].to_vec();
    bad[0] = b'X';
    assert_eq!(ncm::datagrams(&bad, &mut out), None);
    // A datagram pointer beyond the block: skipped, the other stands.
    let mut beyond = buf[..len].to_vec();
    let ptr = ncm::NTH16_BYTES + ncm::NDP16_HEADER;
    beyond[ptr + 2..ptr + 4].copy_from_slice(&5000u16.to_le_bytes());
    assert_eq!(ncm::datagrams(&beyond, &mut out), Some(1));
    // An NDP index pointing at itself again: the walk is bounded.
    let mut looped = buf[..len].to_vec();
    looped[ncm::NTH16_BYTES + 6..ncm::NTH16_BYTES + 8].copy_from_slice(&(ncm::NTH16_BYTES as u16).to_le_bytes());
    assert!(ncm::datagrams(&looped, &mut out).unwrap() <= ncm::MAX_DATAGRAMS);
    for n in 0..len {
        let _ = ncm::datagrams(&buf[..n], &mut out);
    }
}

#[test]
fn cdc_notifications() {
    assert_eq!(cdc::Notification::parse(&[0xa1, 0, 1, 0, 0, 0, 0, 0]), cdc::Notification::Connection(true));
    assert_eq!(cdc::Notification::parse(&[0xa1, 0, 0, 0, 0, 0, 0, 0]), cdc::Notification::Connection(false));
    let mut speed = vec![0xa1, 0x2a, 0, 0, 0, 0, 8, 0];
    speed.extend(100_000_000u32.to_le_bytes());
    speed.extend(50_000_000u32.to_le_bytes());
    assert_eq!(cdc::Notification::parse(&speed), cdc::Notification::Speed(100_000_000, 50_000_000));
    assert_eq!(cdc::Notification::parse(&speed[..12]), cdc::Notification::Other);
    assert_eq!(cdc::Notification::parse(&[0x21, 0, 1, 0, 0, 0, 0, 0]), cdc::Notification::Other);
}

#[test]
fn hub_descriptors_and_port_status() {
    let d = hub::Descriptor::parse(&[9, 0x29, 4, 0x29, 0, 50, 100, 0, 0xff], false).unwrap();
    assert_eq!((d.ports, d.power_on_ms, d.tt_think_time(), d.per_port_power()), (4, 100, 1, true));
    assert_eq!(hub::Descriptor::parse(&[9, 0x2a, 4, 0, 0, 50, 0, 0, 0], false), None, "a USB 3 hub's under USB 2");
    assert_eq!(hub::Descriptor::parse(&[9, 0x29, 0, 0, 0, 0, 0, 0, 0], false), None, "no ports");
    let s = hub::PortStatus::parse(&[0x03, 0x05, 0x11, 0x00]).unwrap();
    assert!(s.connected() && s.enabled());
    assert_eq!(s.speed(false), Speed::High);
    let clear: Vec<u16> = s.changes_to_clear(false).collect();
    assert_eq!(clear, vec![hub::C_PORT_CONNECTION, hub::C_PORT_RESET]);
    let ss = hub::PortStatus { status: 0x0203, change: 0x0061 };
    assert_eq!(ss.changes_to_clear(true).collect::<Vec<_>>(), vec![hub::C_PORT_CONNECTION, hub::C_BH_PORT_RESET, hub::C_PORT_LINK_STATE]);
    assert_eq!(hub::changed_ports(&[0b0001_0110], 4).collect::<Vec<_>>(), vec![1, 2, 4]);
    assert_eq!(hub::changed_ports(&[0xff, 0xff], 3).collect::<Vec<_>>(), vec![1, 2, 3]);
}
