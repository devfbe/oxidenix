//! The algorithms against their published test vectors, and the
//! generator's properties.

use csprng::*;

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn chacha20_block_is_rfc8439s() {
    // RFC 8439, 2.3.2.
    let key: [u8; 32] = core::array::from_fn(|i| i as u8);
    let nonce = [0, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    let want = hex(
        "10 f1 e7 e4 d1 3b 59 15 50 0f dd 1f a3 20 71 c4 c7 d1 f4 c7 33 c0 68 03 04 22 aa 9a c3 d4 6c 4e
         d2 82 64 46 07 9f aa 09 14 c2 d7 05 d9 8b 02 a2 b5 12 9c d1 de 16 4e b9 cb d0 83 e8 a2 50 3c 4e",
    );
    assert_eq!(chacha20_block(&key, 1, &nonce).to_vec(), want);
}

#[test]
fn chacha20_block_all_zero_key() {
    // RFC 8439, A.1, test vector #1.
    let want = hex(
        "76 b8 e0 ad a0 f1 3d 90 40 5d 6a e5 53 86 bd 28 bd d2 19 b8 a0 8d ed 1a a8 36 ef cc 8b 77 0d c7
         da 41 59 7c 51 57 48 8d 77 24 e0 3f b8 d8 4a 37 6a 43 b8 f4 15 18 a1 1c c3 87 b6 69 b2 ee 65 86",
    );
    assert_eq!(chacha20_block(&[0; 32], 0, &[0; 12]).to_vec(), want);
}

#[test]
fn siphash_is_the_references() {
    // The reference implementation's vectors: key 00..0f, messages 00..(n-1).
    let key: [u8; 16] = core::array::from_fn(|i| i as u8);
    let msg: Vec<u8> = (0..64).collect();
    assert_eq!(siphash(&key, &msg[..0]), 0x726f_db47_dd0e_0e31);
    assert_eq!(siphash(&key, &msg[..1]), 0x74f8_39c5_93dc_67fd);
    assert_eq!(siphash(&key, &msg[..8]), 0x93f5_f579_9a93_2462);
    assert_eq!(siphash(&key, &msg[..15]), 0xa129_ca61_49be_45e5);
}

#[test]
fn the_generator_never_repeats_and_erases_its_key() {
    let mut g = ChaCha::new([7; 32]);
    let mut a = [0u8; 100];
    let mut b = [0u8; 100];
    g.fill(&mut a);
    g.fill(&mut b);
    assert_ne!(a, b);
    // The same seed gives the same stream (a deterministic generator).
    let mut h = ChaCha::new([7; 32]);
    let mut c = [0u8; 100];
    h.fill(&mut c);
    assert_eq!(a, c);
    // Entropy changes what follows.
    h.reseed(b"some entropy");
    let mut d = [0u8; 100];
    let mut e = [0u8; 100];
    h.fill(&mut d);
    g.fill(&mut e);
    assert_ne!(d, e);
    // Long requests and short ones, every byte set.
    let mut long = [0u8; 1000];
    g.fill(&mut long);
    assert!(long.chunks(64).all(|c| c.iter().any(|&x| x != 0)));
    let mut none = [0u8; 0];
    g.fill(&mut none);
}

#[test]
fn the_generators_bytes_look_uniform() {
    let mut g = ChaCha::new([1; 32]);
    let mut counts = [0u32; 256];
    let mut buf = [0u8; 256];
    for _ in 0..1024 {
        g.fill(&mut buf);
        for &b in &buf {
            counts[b as usize] += 1;
        }
    }
    // 1024 expected per value: far from any of them would be a bug.
    assert!(counts.iter().all(|&c| (800..1250).contains(&c)), "{counts:?}");
}
