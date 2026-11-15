//! RTMP handshake (simple and Adobe "complex" digest variant).
//!
//! Port of `ngx_rtmp_handshake.c`.
//!
//! ```text
//!          =peer1=                      =peer2=
//! challenge ----> (.....[digest1]......) ----> 1537 bytes
//! response  <---- (...........[digest2]) <---- 1536 bytes
//! ```
//!
//! * digest1: HMAC_SHA256(packet, peer1_partial_key), position derived from
//!   packet bytes;
//! * digest2: HMAC_SHA256(packet, HMAC_SHA256(digest1, peer2_full_key)),
//!   always at the end of the packet.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const RTMP_VERSION: u8 = 3;
pub const PACKET_SIZE: usize = 1536;
const KEYLEN: usize = 32;

const KEY_TAIL: [u8; 32] = [
    0xF0, 0xEE, 0xC2, 0x4A, 0x80, 0x68, 0xBE, 0xE8, 0x2E, 0x00, 0xD0, 0xD1, 0x02, 0x9E, 0x7E,
    0x57, 0x6E, 0xEC, 0x5D, 0x2D, 0x29, 0x80, 0x6F, 0xAB, 0x93, 0xB8, 0xE6, 0x36, 0xCF, 0xEB,
    0x31, 0xAE,
];

const fn concat<const N: usize>(head: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut i = 0;
    while i < head.len() {
        out[i] = head[i];
        i += 1;
    }
    let mut j = 0;
    while j < KEY_TAIL.len() {
        out[head.len() + j] = KEY_TAIL[j];
        j += 1;
    }
    out
}

static SERVER_KEY: [u8; 68] = concat(b"Genuine Adobe Flash Media Server 001");
static CLIENT_KEY: [u8; 62] = concat(b"Genuine Adobe Flash Player 001");

fn server_full_key() -> &'static [u8] {
    &SERVER_KEY
}
fn server_partial_key() -> &'static [u8] {
    &SERVER_KEY[..36]
}
fn client_full_key() -> &'static [u8] {
    &CLIENT_KEY
}
fn client_partial_key() -> &'static [u8] {
    &CLIENT_KEY[..30]
}

const SERVER_VERSION: [u8; 4] = [0x0D, 0x0E, 0x0A, 0x0D];
const CLIENT_VERSION: [u8; 4] = [0x0C, 0x00, 0x0D, 0x0E];

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 of `buf`, optionally skipping the 32-byte digest at `skip`.
fn make_digest(key: &[u8], buf: &[u8], skip: Option<usize>) -> [u8; KEYLEN] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    match skip {
        Some(off) if off + KEYLEN <= buf.len() => {
            mac.update(&buf[..off]);
            mac.update(&buf[off + KEYLEN..]);
        }
        _ => mac.update(buf),
    }
    mac.finalize().into_bytes().into()
}

/// Locate and verify the peer digest. `base` is 772 or 8 (the two schemes).
fn find_digest(pkt: &[u8; PACKET_SIZE], key: &[u8], base: usize) -> Option<usize> {
    let sum: usize = pkt[base..base + 4].iter().map(|&b| b as usize).sum();
    let offs = sum % 728 + base + 4;
    let digest = make_digest(key, pkt, Some(offs));
    (digest[..] == pkt[offs..offs + KEYLEN]).then_some(offs)
}

fn write_digest(pkt: &mut [u8; PACKET_SIZE], key: &[u8], base: usize) {
    let sum: usize = pkt[base + 8..base + 12].iter().map(|&b| b as usize).sum();
    let offs = sum % 728 + base + 12;
    let d = make_digest(key, pkt, Some(offs));
    pkt[offs..offs + KEYLEN].copy_from_slice(&d);
}

/// Non-cryptographic filler, matching nginx's use of `rand()`.
fn fill_random(buf: &mut [u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(0);
    let mut x = SEED.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x1234_5678)
        | 1;
    for b in buf {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
}

fn create_challenge(epoch: u32, version: [u8; 4], key: &[u8]) -> [u8; PACKET_SIZE] {
    let mut pkt = [0u8; PACKET_SIZE];
    pkt[0..4].copy_from_slice(&epoch.to_be_bytes());
    pkt[4..8].copy_from_slice(&version);
    fill_random(&mut pkt[8..]);
    write_digest(&mut pkt, key, 0);
    pkt
}

/// Result of parsing the peer's challenge (C1 or S1).
#[derive(Debug)]
pub struct Challenge {
    pub peer_epoch: u32,
    /// `None` = old-style (simple) handshake.
    pub digest: Option<[u8; KEYLEN]>,
}

fn parse_challenge(pkt: &[u8; PACKET_SIZE], peer_key: &[u8], key: &[u8]) -> Challenge {
    let peer_epoch = u32::from_be_bytes(pkt[0..4].try_into().unwrap());
    if pkt[4..8] == [0, 0, 0, 0] {
        return Challenge { peer_epoch, digest: None };
    }
    let offs = find_digest(pkt, peer_key, 772).or_else(|| find_digest(pkt, peer_key, 8));
    let digest = offs.map(|o| make_digest(key, &pkt[o..o + KEYLEN], None));
    if digest.is_none() {
        tracing::info!("handshake: digest not found, falling back to old-style");
    }
    Challenge { peer_epoch, digest }
}

fn create_response(digest: &[u8; KEYLEN]) -> [u8; PACKET_SIZE] {
    let mut pkt = [0u8; PACKET_SIZE];
    fill_random(&mut pkt);
    let p = PACKET_SIZE - KEYLEN;
    let d = make_digest(digest, &pkt, Some(p));
    pkt[p..].copy_from_slice(&d);
    pkt
}

fn bad_version(v: u8) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("unexpected RTMP version {v}"))
}

/// Server side: read C0C1, send S0S1S2, read C2.
pub async fn server<S: AsyncRead + AsyncWrite + Unpin>(io: &mut S, epoch: u32) -> io::Result<Challenge> {
    let mut c0 = [0u8; 1];
    io.read_exact(&mut c0).await?;
    if c0[0] != RTMP_VERSION {
        return Err(bad_version(c0[0]));
    }
    let mut c1 = [0u8; PACKET_SIZE];
    io.read_exact(&mut c1).await?;

    let ch = parse_challenge(&c1, client_partial_key(), server_full_key());

    let mut out = Vec::with_capacity(1 + 2 * PACKET_SIZE);
    out.push(RTMP_VERSION);
    match &ch.digest {
        Some(d) => {
            out.extend_from_slice(&create_challenge(epoch, SERVER_VERSION, server_partial_key()));
            out.extend_from_slice(&create_response(d));
        }
        None => {
            // Old-style: echo the client's packet back as both S1 and S2.
            out.extend_from_slice(&c1);
            out.extend_from_slice(&c1);
        }
    }
    io.write_all(&out).await?;
    io.flush().await?;

    let mut c2 = [0u8; PACKET_SIZE];
    io.read_exact(&mut c2).await?;
    Ok(ch)
}

/// Client side (used by relay/push): send C0C1, read S0S1S2, send C2.
pub async fn client<S: AsyncRead + AsyncWrite + Unpin>(io: &mut S, epoch: u32) -> io::Result<Challenge> {
    let mut out = Vec::with_capacity(1 + PACKET_SIZE);
    out.push(RTMP_VERSION);
    out.extend_from_slice(&create_challenge(epoch, CLIENT_VERSION, client_partial_key()));
    io.write_all(&out).await?;
    io.flush().await?;

    let mut s0 = [0u8; 1];
    io.read_exact(&mut s0).await?;
    if s0[0] != RTMP_VERSION {
        return Err(bad_version(s0[0]));
    }
    let mut s1 = [0u8; PACKET_SIZE];
    io.read_exact(&mut s1).await?;
    let ch = parse_challenge(&s1, server_partial_key(), client_full_key());
    let mut s2 = [0u8; PACKET_SIZE];
    io.read_exact(&mut s2).await?;

    let c2 = match &ch.digest {
        Some(d) => create_response(d),
        None => s1,
    };
    io.write_all(&c2).await?;
    io.flush().await?;
    Ok(ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(&SERVER_KEY[..36], b"Genuine Adobe Flash Media Server 001");
        assert_eq!(&CLIENT_KEY[..30], b"Genuine Adobe Flash Player 001");
        assert_eq!(SERVER_KEY[36], 0xF0);
        assert_eq!(CLIENT_KEY[61], 0xAE);
    }

    #[test]
    fn client_digest_is_found_by_server() {
        let c1 = create_challenge(0, CLIENT_VERSION, client_partial_key());
        assert!(find_digest(&c1, client_partial_key(), 8).is_some());
        let ch = parse_challenge(&c1, client_partial_key(), server_full_key());
        assert!(ch.digest.is_some());
    }

    #[test]
    fn simple_handshake_detected() {
        let mut c1 = [0u8; PACKET_SIZE];
        fill_random(&mut c1[8..]);
        assert!(parse_challenge(&c1, client_partial_key(), server_full_key()).digest.is_none());
    }

    #[tokio::test]
    async fn client_server_loopback() {
        let (mut a, mut b) = tokio::io::duplex(8192);
        let srv = tokio::spawn(async move { server(&mut a, 0).await.map(|c| c.digest.is_some()) });
        let cli = client(&mut b, 0).await.unwrap();
        assert!(cli.digest.is_some(), "client verified server digest");
        assert!(srv.await.unwrap().unwrap(), "server verified client digest");
    }
}
