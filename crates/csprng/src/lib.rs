//! Cryptographic randomness for the kernel and the servers: ChaCha20 (RFC
//! 8439) as a random generator with fast key erasure (the kernel's
//! getrandom, the servers' secrets), and SipHash-2-4 as the keyed hash for
//! values others must not guess (netd's TCP initial sequence numbers, RFC
//! 6528, and ephemeral ports, RFC 6056). Kept apart so the algorithms are
//! tested on the host against their published test vectors.
//!
//! **The generator** holds a 256-bit key. Each request runs ChaCha20 under
//! it (a counter as the nonce) and takes the first 32 bytes of output as
//! the next key before any byte goes out, so that a later compromise of
//! the state reveals nothing that was handed out before (fast key erasure,
//! as Linux's crng and OpenBSD's arc4random do). Entropy is mixed in by
//! hashing it with the current key into a new one (`reseed`).

#![no_std]

/// The ChaCha20 block function (RFC 8439 section 2.3): 64 bytes of key
/// stream for `key`, block `counter` and `nonce`.
pub fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let mut state = [0u32; 16];
    state[0..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    for i in 0..8 {
        state[4 + i] = u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    state[12] = counter;
    for i in 0..3 {
        state[13 + i] = u32::from_le_bytes([nonce[4 * i], nonce[4 * i + 1], nonce[4 * i + 2], nonce[4 * i + 3]]);
    }
    let mut x = state;
    fn quarter(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
        x[a] = x[a].wrapping_add(x[b]);
        x[d] = (x[d] ^ x[a]).rotate_left(16);
        x[c] = x[c].wrapping_add(x[d]);
        x[b] = (x[b] ^ x[c]).rotate_left(12);
        x[a] = x[a].wrapping_add(x[b]);
        x[d] = (x[d] ^ x[a]).rotate_left(8);
        x[c] = x[c].wrapping_add(x[d]);
        x[b] = (x[b] ^ x[c]).rotate_left(7);
    }
    for _ in 0..10 {
        quarter(&mut x, 0, 4, 8, 12);
        quarter(&mut x, 1, 5, 9, 13);
        quarter(&mut x, 2, 6, 10, 14);
        quarter(&mut x, 3, 7, 11, 15);
        quarter(&mut x, 0, 5, 10, 15);
        quarter(&mut x, 1, 6, 11, 12);
        quarter(&mut x, 2, 7, 8, 13);
        quarter(&mut x, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&x[i].wrapping_add(state[i]).to_le_bytes());
    }
    out
}

/// A random generator: ChaCha20 with fast key erasure (see the crate's
/// documentation). Seeded once with at least 256 bits of entropy, it never
/// runs out.
pub struct ChaCha {
    key: [u8; 32],
    /// Requests served (the nonce: no two runs share one).
    requests: u64,
}

impl ChaCha {
    pub const fn new(seed: [u8; 32]) -> ChaCha {
        ChaCha { key: seed, requests: 0 }
    }

    /// Mixes `entropy` into the key: the new key is a run of ChaCha20 under
    /// the old key with the entropy (in 32-byte pieces) as further keys.
    pub fn reseed(&mut self, entropy: &[u8]) {
        for piece in entropy.chunks(32) {
            let mut k = [0u8; 32];
            k[..piece.len()].copy_from_slice(piece);
            for (a, b) in k.iter_mut().zip(self.key.iter()) {
                *a ^= *b;
            }
            let block = chacha20_block(&k, 0, &[0xff; 12]);
            let old = chacha20_block(&self.key, 0, &[0xfe; 12]);
            for i in 0..32 {
                self.key[i] = block[i] ^ old[32 + i];
            }
        }
    }

    /// Fills `out` with random bytes (up to 256 bytes per call keep the
    /// key's erasure cheap; any length works).
    pub fn fill(&mut self, out: &mut [u8]) {
        self.requests = self.requests.wrapping_add(1);
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&self.requests.to_le_bytes());
        let key = self.key;
        let first = chacha20_block(&key, 0, &nonce);
        // The next key first: what goes out below cannot be derived from it.
        self.key.copy_from_slice(&first[..32]);
        let mut done = 0;
        let take = (out.len()).min(32);
        out[..take].copy_from_slice(&first[32..32 + take]);
        done += take;
        let mut counter = 1u32;
        while done < out.len() {
            let block = chacha20_block(&key, counter, &nonce);
            let take = (out.len() - done).min(64);
            out[done..done + take].copy_from_slice(&block[..take]);
            done += take;
            counter = counter.wrapping_add(1);
        }
    }

    pub fn u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill(&mut b);
        u64::from_le_bytes(b)
    }
}

/// SipHash-2-4 of `data` under `key` (Aumasson and Bernstein's reference).
pub fn siphash(key: &[u8; 16], data: &[u8]) -> u64 {
    let k0 = u64::from_le_bytes(key[..8].try_into().expect("8 bytes"));
    let k1 = u64::from_le_bytes(key[8..].try_into().expect("8 bytes"));
    let mut v = [k0 ^ 0x736f_6d65_7073_6575, k1 ^ 0x646f_7261_6e64_6f6d, k0 ^ 0x6c79_6765_6e65_7261, k1 ^ 0x7465_6462_7974_6573];
    fn round(v: &mut [u64; 4]) {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13) ^ v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16) ^ v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21) ^ v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17) ^ v[2];
        v[2] = v[2].rotate_left(32);
    }
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let m = u64::from_le_bytes(c.try_into().expect("8 bytes"));
        v[3] ^= m;
        round(&mut v);
        round(&mut v);
        v[0] ^= m;
    }
    let rest = chunks.remainder();
    let mut last = [0u8; 8];
    last[..rest.len()].copy_from_slice(rest);
    last[7] = data.len() as u8;
    let m = u64::from_le_bytes(last);
    v[3] ^= m;
    round(&mut v);
    round(&mut v);
    v[0] ^= m;
    v[2] ^= 0xff;
    for _ in 0..4 {
        round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}
