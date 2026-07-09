//! OCB2-AES128 — the Mumble UDP voice crypto.
//!
//! Ported faithfully from umurmur's `crypt.cpp` (itself Mumble's `CryptState`).
//! The stock FiveM client speaks exactly this, so we cannot substitute a
//! modern AEAD: OCB2 is academically weakened but mandated by the fixed client.
//! We keep it, isolate it here, and document the constraint.
//!
//! # Wire layout of an encrypted UDP packet
//!
//! ```text
//! byte 0      IV[0]        (low byte of the 128-bit nonce counter)
//! bytes 1..4  tag[0..3]    (first 3 bytes of the OCB2 authentication tag)
//! bytes 4..   ciphertext   (same length as the plaintext)
//! ```
//!
//! # GF(2^128) doubling
//!
//! The block is treated as a big-endian 128-bit integer. `times2` is a left
//! shift by one bit with a conditional reduction by the polynomial `0x87` —
//! the same doubling used by CMAC (RFC 4493), which we exploit for a
//! known-answer test.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;

const BLOCK: usize = 16;

/// Big-endian GF(2^128) doubling (`times2` / OCB2 `S2`).
fn times2(block: &[u8; BLOCK]) -> [u8; BLOCK] {
    let carry = block[0] >> 7;
    let mut out = [0u8; BLOCK];
    for i in 0..BLOCK {
        let next = if i + 1 < BLOCK { block[i + 1] >> 7 } else { 0 };
        out[i] = (block[i] << 1) | next;
    }
    if carry == 1 {
        out[BLOCK - 1] ^= 0x87;
    }
    out
}

/// `times3` (OCB2 `S3`): `block XOR times2(block)`.
fn times3(block: &[u8; BLOCK]) -> [u8; BLOCK] {
    let two = times2(block);
    let mut out = *block;
    xor_into(&mut out, &two);
    out
}

fn xor_into(dst: &mut [u8; BLOCK], src: &[u8; BLOCK]) {
    for i in 0..BLOCK {
        dst[i] ^= src[i];
    }
}

fn xor(a: &[u8; BLOCK], b: &[u8; BLOCK]) -> [u8; BLOCK] {
    let mut out = *a;
    xor_into(&mut out, b);
    out
}

/// AES-128 key schedule pair (Mumble uses raw ECB blocks for both directions).
pub struct Ocb2 {
    enc: Aes128,
    dec: Aes128,
}

impl Ocb2 {
    /// Build from the 16-byte shared key delivered in `CryptSetup.key`.
    pub fn new(key: &[u8; BLOCK]) -> Self {
        let ga = aes::cipher::generic_array::GenericArray::from_slice(key);
        Self {
            enc: Aes128::new(ga),
            dec: Aes128::new(ga),
        }
    }

    fn aes_encrypt(&self, input: &[u8; BLOCK]) -> [u8; BLOCK] {
        let mut b = aes::cipher::generic_array::GenericArray::clone_from_slice(input);
        self.enc.encrypt_block(&mut b);
        b.into()
    }

    fn aes_decrypt(&self, input: &[u8; BLOCK]) -> [u8; BLOCK] {
        let mut b = aes::cipher::generic_array::GenericArray::clone_from_slice(input);
        self.dec.decrypt_block(&mut b);
        b.into()
    }

    /// OCB2 encrypt `plain` under `nonce`. Writes ciphertext to `encrypted`
    /// (same length as `plain`) and returns the 16-byte tag.
    pub fn ocb_encrypt(
        &self,
        plain: &[u8],
        encrypted: &mut Vec<u8>,
        nonce: &[u8; BLOCK],
    ) -> [u8; BLOCK] {
        encrypted.clear();
        let mut delta = self.aes_encrypt(nonce);
        let mut checksum = [0u8; BLOCK];
        let mut off = 0;
        let mut len = plain.len();

        while len > BLOCK {
            delta = times2(&delta);
            let mut chunk = [0u8; BLOCK];
            chunk.copy_from_slice(&plain[off..off + BLOCK]);
            let tmp = self.aes_encrypt(&xor(&delta, &chunk));
            encrypted.extend_from_slice(&xor(&delta, &tmp));
            xor_into(&mut checksum, &chunk);
            off += BLOCK;
            len -= BLOCK;
        }

        // Final (possibly partial) block.
        delta = times2(&delta);
        let mut tmp = [0u8; BLOCK];
        tmp[8..].copy_from_slice(&((len as u64) * 8).to_be_bytes());
        xor_into(&mut tmp, &delta);
        let pad = self.aes_encrypt(&tmp);

        // tmp = plain[..len] || pad[len..]
        let mut last = pad;
        last[..len].copy_from_slice(&plain[off..off + len]);
        xor_into(&mut checksum, &last);
        let cipher_last = xor(&pad, &last);
        encrypted.extend_from_slice(&cipher_last[..len]);

        delta = times3(&delta);
        self.aes_encrypt(&xor(&delta, &checksum))
    }

    /// OCB2 decrypt `encrypted` under `nonce`. Writes plaintext to `plain`
    /// (same length) and returns the 16-byte tag for the caller to verify.
    pub fn ocb_decrypt(
        &self,
        encrypted: &[u8],
        plain: &mut Vec<u8>,
        nonce: &[u8; BLOCK],
    ) -> [u8; BLOCK] {
        plain.clear();
        let mut delta = self.aes_encrypt(nonce);
        let mut checksum = [0u8; BLOCK];
        let mut off = 0;
        let mut len = encrypted.len();

        while len > BLOCK {
            delta = times2(&delta);
            let mut chunk = [0u8; BLOCK];
            chunk.copy_from_slice(&encrypted[off..off + BLOCK]);
            let tmp = self.aes_decrypt(&xor(&delta, &chunk));
            let p = xor(&delta, &tmp);
            plain.extend_from_slice(&p);
            xor_into(&mut checksum, &p);
            off += BLOCK;
            len -= BLOCK;
        }

        delta = times2(&delta);
        let mut tmp = [0u8; BLOCK];
        tmp[8..].copy_from_slice(&((len as u64) * 8).to_be_bytes());
        xor_into(&mut tmp, &delta);
        let pad = self.aes_encrypt(&tmp);

        // tmp = (encrypted[..len] || zeros) XOR pad
        let mut last = pad;
        for i in 0..len {
            last[i] ^= encrypted[off + i];
        }
        // for i in len..BLOCK: last[i] = pad[i] ^ 0 = pad[i] (already)
        xor_into(&mut checksum, &last);
        plain.extend_from_slice(&last[..len]);

        delta = times3(&delta);
        self.aes_encrypt(&xor(&delta, &checksum))
    }
}

/// Per-peer crypt state: key schedule + rolling nonces + replay history +
/// packet stats. Mirrors umurmur's `cryptState_t` and its `encrypt`/`decrypt`
/// nonce bookkeeping (`crypt.cpp`).
pub struct CryptState {
    ocb: Ocb2,
    /// Nonce we increment before each outgoing packet.
    pub encrypt_iv: [u8; BLOCK],
    /// Nonce tracking incoming packets (recovered on loss/reorder).
    pub decrypt_iv: [u8; BLOCK],
    /// Replay guard: last seen `decrypt_iv[1]` indexed by `decrypt_iv[0]`.
    decrypt_history: [u8; 256],
    pub good: u32,
    pub late: u32,
    pub lost: u32,
}

impl CryptState {
    /// Initialise from the key + both nonces (server side: `encrypt_iv` =
    /// `server_nonce`, `decrypt_iv` = `client_nonce`).
    pub fn new(key: &[u8; BLOCK], encrypt_iv: &[u8; BLOCK], decrypt_iv: &[u8; BLOCK]) -> Self {
        Self {
            ocb: Ocb2::new(key),
            encrypt_iv: *encrypt_iv,
            decrypt_iv: *decrypt_iv,
            decrypt_history: [0u8; 256],
            good: 0,
            late: 0,
            lost: 0,
        }
    }

    /// Encrypt one voice payload, producing `[iv0, tag0, tag1, tag2, cipher..]`.
    pub fn encrypt(&mut self, source: &[u8]) -> Vec<u8> {
        // Increment the 128-bit little-endian-by-byte counter.
        for b in self.encrypt_iv.iter_mut() {
            *b = b.wrapping_add(1);
            if *b != 0 {
                break;
            }
        }
        let mut cipher = Vec::with_capacity(source.len());
        let tag = self.ocb.ocb_encrypt(source, &mut cipher, &self.encrypt_iv);
        let mut out = Vec::with_capacity(4 + cipher.len());
        out.push(self.encrypt_iv[0]);
        out.extend_from_slice(&tag[..3]);
        out.extend_from_slice(&cipher);
        out
    }

    /// Decrypt one packet. Returns the plaintext, or `None` if the packet is
    /// too short, a replay, or fails tag verification. Updates nonce + stats.
    pub fn decrypt(&mut self, source: &[u8]) -> Option<Vec<u8>> {
        if source.len() < 4 {
            return None;
        }
        let ivbyte = source[0];
        let saveiv = self.decrypt_iv;
        let mut restore = false;
        let mut late = 0u32;
        let mut lost = 0i64;

        if ((self.decrypt_iv[0] as u16 + 1) & 0xFF) as u8 == ivbyte {
            // In order.
            if ivbyte > self.decrypt_iv[0] {
                self.decrypt_iv[0] = ivbyte;
            } else if ivbyte < self.decrypt_iv[0] {
                self.decrypt_iv[0] = ivbyte;
                for b in self.decrypt_iv[1..].iter_mut() {
                    *b = b.wrapping_add(1);
                    if *b != 0 {
                        break;
                    }
                }
            } else {
                return None;
            }
        } else {
            // Out of order or repeat.
            let mut diff = ivbyte as i32 - self.decrypt_iv[0] as i32;
            if diff > 128 {
                diff -= 256;
            } else if diff < -128 {
                diff += 256;
            }

            if ivbyte < self.decrypt_iv[0] && diff > -30 && diff < 0 {
                late = 1;
                lost = -1;
                self.decrypt_iv[0] = ivbyte;
                restore = true;
            } else if ivbyte > self.decrypt_iv[0] && diff > -30 && diff < 0 {
                late = 1;
                lost = -1;
                self.decrypt_iv[0] = ivbyte;
                for b in self.decrypt_iv[1..].iter_mut() {
                    let was = *b;
                    *b = b.wrapping_sub(1);
                    if was != 0 {
                        break;
                    }
                }
                restore = true;
            } else if ivbyte > self.decrypt_iv[0] && diff > 0 {
                lost = (ivbyte - self.decrypt_iv[0] - 1) as i64;
                self.decrypt_iv[0] = ivbyte;
            } else if ivbyte < self.decrypt_iv[0] && diff > 0 {
                lost = 256 - self.decrypt_iv[0] as i64 + ivbyte as i64 - 1;
                self.decrypt_iv[0] = ivbyte;
                for b in self.decrypt_iv[1..].iter_mut() {
                    *b = b.wrapping_add(1);
                    if *b != 0 {
                        break;
                    }
                }
            } else {
                return None;
            }

            if self.decrypt_history[self.decrypt_iv[0] as usize] == self.decrypt_iv[1] {
                self.decrypt_iv = saveiv;
                return None;
            }
        }

        let mut plain = Vec::with_capacity(source.len() - 4);
        let tag = self
            .ocb
            .ocb_decrypt(&source[4..], &mut plain, &self.decrypt_iv);

        if tag[..3] != source[1..4] {
            self.decrypt_iv = saveiv;
            return None;
        }
        self.decrypt_history[self.decrypt_iv[0] as usize] = self.decrypt_iv[1];

        if restore {
            self.decrypt_iv = saveiv;
        }

        self.good = self.good.wrapping_add(1);
        self.late = self.late.wrapping_add(late);
        if lost > 0 {
            self.lost = self.lost.wrapping_add(lost as u32);
        }
        Some(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times2_matches_rfc4493_subkey_derivation() {
        // RFC 4493 CMAC uses the identical GF(2^128) doubling (poly 0x87).
        // L = AES_K(0) for key 2b7e1516..., K1 = times2(L). Independent vector.
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let ocb = Ocb2::new(&key);
        let l = ocb.aes_encrypt(&[0u8; 16]);
        assert_eq!(
            hexs(&l),
            "7df76b0c1ab899b33e42f047b91b546f",
            "AES_K(0) mismatch"
        );
        let k1 = times2(&l);
        assert_eq!(
            hexs(&k1),
            "fbeed618357133667c85e08f7236a8de",
            "times2 (K1) mismatch"
        );
    }

    #[test]
    fn ocb_roundtrips_various_lengths() {
        let key = hex("000102030405060708090a0b0c0d0e0f");
        let nonce = hex("101112131415161718191a1b1c1d1e1f");
        let ocb = Ocb2::new(&key);
        for len in [1usize, 5, 15, 16, 17, 31, 32, 40, 60, 200] {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let mut enc = Vec::new();
            let t1 = ocb.ocb_encrypt(&plain, &mut enc, &nonce);
            assert_eq!(enc.len(), len, "ciphertext length preserved");
            let mut dec = Vec::new();
            let t2 = ocb.ocb_decrypt(&enc, &mut dec, &nonce);
            assert_eq!(dec, plain, "roundtrip failed at len {len}");
            assert_eq!(t1, t2, "tag mismatch at len {len}");
        }
    }

    #[test]
    fn cryptstate_peer_roundtrip() {
        // Two peers sharing a key: A's encrypt_iv == B's decrypt_iv.
        let key = hex("0f0e0d0c0b0a09080706050403020100");
        let a_enc = hex("00000000000000000000000000000000");
        let b_dec = a_enc;
        let a_to_b_key = key;
        let mut a = CryptState::new(
            &a_to_b_key,
            &a_enc,
            &hex("aa000000000000000000000000000000"),
        );
        let mut b = CryptState::new(
            &a_to_b_key,
            &hex("bb000000000000000000000000000000"),
            &b_dec,
        );

        for n in 0..5u8 {
            let msg = vec![n; 20 + n as usize];
            let packet = a.encrypt(&msg);
            let got = b.decrypt(&packet).expect("valid packet decrypts");
            assert_eq!(got, msg, "packet {n} roundtrip");
        }
        assert_eq!(b.good, 5);
    }

    #[test]
    fn tampered_tag_is_rejected() {
        let key = hex("0f0e0d0c0b0a09080706050403020100");
        let mut a = CryptState::new(&key, &[0u8; 16], &[0u8; 16]);
        let mut b = CryptState::new(&key, &[0u8; 16], &[0u8; 16]);
        let mut packet = a.encrypt(&[1, 2, 3, 4, 5]);
        packet[2] ^= 0xFF; // flip a tag byte
        assert!(b.decrypt(&packet).is_none());
    }

    #[test]
    fn replayed_packet_is_rejected() {
        let key = hex("0f0e0d0c0b0a09080706050403020100");
        let mut a = CryptState::new(&key, &[0u8; 16], &[0u8; 16]);
        let mut b = CryptState::new(&key, &[0u8; 16], &[0u8; 16]);
        // Send a couple so the replay lands out-of-order (history path).
        let p1 = a.encrypt(&[9; 10]);
        let p2 = a.encrypt(&[8; 10]);
        assert!(b.decrypt(&p1).is_some());
        assert!(b.decrypt(&p2).is_some());
        assert!(b.decrypt(&p1).is_none(), "replay of p1 must be refused");
    }

    fn hex(s: &str) -> [u8; 16] {
        let bytes: Vec<u8> = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect();
        let mut out = [0u8; 16];
        out.copy_from_slice(&bytes);
        out
    }

    fn hexs(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
