//! Room keys, the keys derived from them, pairing codes and the PAKE that pairing runs on, all
//! matching the MediaLinker web page byte for byte.
//!
//! A room key is 32 random bytes that only the paired devices hold. The server knows the room by an
//! ID derived from it, and every clip is sealed (in the `MLE1` format of [`crate::e2e`]) under keys
//! derived from the room key and the clip's random ID, so no key ever seals two streams.
//!
//! Pairing codes look like `K7X-42PQ`. The server picks the first three characters, the plate, to
//! put the two devices in touch; the rest stays on the devices. They run CPace over ristretto255:
//! each sends its share of a Diffie-Hellman exchange on a generator derived from the whole code, so
//! only a device that knows the code ends up with the same session key. Someone in the middle,
//! the server included, gets a single guess per pairing and can't test guesses offline afterwards.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use curve25519_dalek::{RistrettoPoint, Scalar, ristretto::CompressedRistretto, traits::Identity};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256, Sha512};

use crate::e2e::SecretKey;

/// The share code alphabet: no 0/O, 1/I/L, so codes survive being read out or typed from a screen.
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
pub const PLATE_LEN: usize = 3;
const SECRET_LEN: usize = 4;

fn hmac(key: &[u8], label: &str) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(label.as_bytes());
    mac.finalize().into_bytes().into()
}

pub fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|e| format!("cannot get random bytes: {e}"))?;
    Ok(bytes)
}

/// A random 16-byte ID in base64url, for clips.
pub fn new_id() -> Result<String, String> {
    Ok(URL_SAFE_NO_PAD.encode(random::<16>()?))
}

#[derive(Clone)]
pub struct RoomKey([u8; 32]);

impl RoomKey {
    pub fn generate() -> Result<Self, String> {
        random().map(Self)
    }

    pub fn parse(encoded: &str) -> Option<Self> {
        let raw = URL_SAFE_NO_PAD.decode(encoded.trim()).ok()?;
        Some(Self(raw.try_into().ok()?))
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    /// What the server calls the room; it can't be turned back into the key.
    pub fn room_id(&self) -> String {
        URL_SAFE_NO_PAD.encode(&hmac(&self.0, "ml-clip room")[..16])
    }

    pub fn meta_key(&self, clip_id: &str) -> SecretKey {
        SecretKey::from_bytes(hmac(&self.0, &format!("ml-clip meta {clip_id}")))
    }

    pub fn blob_key(&self, clip_id: &str) -> SecretKey {
        SecretKey::from_bytes(hmac(&self.0, &format!("ml-clip blob {clip_id}")))
    }
}

/// A pairing code without its dash, upper case: the plate, then the secret.
#[derive(Clone, PartialEq, Debug)]
pub struct Code(String);

impl Code {
    pub fn new(plate: &str) -> Result<Self, String> {
        let mut secret = String::new();
        while secret.len() < SECRET_LEN {
            // 248 is the largest multiple of 31 below 256, so every character is equally likely.
            for b in random::<16>()? {
                if b < 248 && secret.len() < SECRET_LEN {
                    secret.push(ALPHABET[(b % 31) as usize] as char);
                }
            }
        }
        Self::parse(&format!("{plate}{secret}")).ok_or_else(|| "the server sent a malformed code".into())
    }

    /// Takes `K7X-42PQ` in any case, with or without the dash or spaces, or a link with `#clip=CODE`.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        let input = input.rsplit_once("#clip=").map_or(input, |(_, c)| c);
        let code: String = input
            .chars()
            .filter(|c| !matches!(c, '-' | ' '))
            .map(|c| c.to_ascii_uppercase())
            .collect();
        (code.len() == PLATE_LEN + SECRET_LEN && code.bytes().all(|b| ALPHABET.contains(&b))).then_some(Self(code))
    }

    pub fn plate(&self) -> &str {
        &self.0[..PLATE_LEN]
    }

    pub fn display(&self) -> String {
        format!("{}-{}", &self.0[..PLATE_LEN], &self.0[PLATE_LEN..])
    }
}

/// One side of a CPace exchange.
pub struct Pake {
    scalar: Scalar,
    sid: String,
    pub public: [u8; 32],
}

/// The keys each direction of a pairing seals its one message with.
pub struct PairKeys {
    pub to_joiner: SecretKey,
    pub to_host: SecretKey,
}

impl Pake {
    pub fn start(code: &Code, sid: &str) -> Result<Self, String> {
        let mut h = Sha512::new();
        h.update(b"ml-clip pake v1\0");
        h.update(sid.as_bytes());
        h.update(b"\0");
        h.update(code.0.as_bytes());
        let generator = RistrettoPoint::from_uniform_bytes(&h.finalize().into());
        let scalar = Scalar::from_bytes_mod_order_wide(&random()?);
        let public = (scalar * generator).compress().to_bytes();
        Ok(Self {
            scalar,
            sid: sid.to_string(),
            public,
        })
    }

    /// Mixes in the other side's share. A wrong code doesn't fail here; it gives keys that don't match.
    pub fn finish(self, host: &[u8; 32], joiner: &[u8; 32]) -> Result<PairKeys, String> {
        let theirs = if *host == self.public { joiner } else { host };
        let point = CompressedRistretto(*theirs)
            .decompress()
            .filter(|p| *p != RistrettoPoint::identity())
            .ok_or("the other device sent a broken pairing message")?;
        let shared = (self.scalar * point).compress().to_bytes();
        let mut h = Sha256::new();
        h.update(b"ml-clip pake v1 key\0");
        h.update(self.sid.as_bytes());
        h.update(host);
        h.update(joiner);
        h.update(shared);
        let session: [u8; 32] = h.finalize().into();
        Ok(PairKeys {
            to_joiner: SecretKey::from_bytes(hmac(&session, "ml-clip host to joiner")),
            to_host: SecretKey::from_bytes(hmac(&session, "ml-clip joiner to host")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_parse_loosely_and_print_with_a_dash() {
        let c = Code::parse(" k7x-42pq ").unwrap();
        assert_eq!(c.display(), "K7X-42PQ");
        assert_eq!(c.plate(), "K7X");
        assert_eq!(Code::parse("https://m.example.com/#clip=K7X-42PQ"), Some(c.clone()));
        assert_eq!(Code::parse("K7X 42PQ"), Some(c));
        assert_eq!(Code::parse("K7X-42P"), None);
        assert_eq!(Code::parse("K0X-42PQ"), None);
        let fresh = Code::new("ABC").unwrap();
        assert_eq!(fresh.plate(), "ABC");
        assert_eq!(fresh.display().len(), 8);
    }

    #[test]
    fn same_code_gives_same_keys_and_a_wrong_code_does_not() {
        let code = Code::parse("K7X-42PQ").unwrap();
        let host = Pake::start(&code, "sid").unwrap();
        let joiner = Pake::start(&code, "sid").unwrap();
        let (h, j) = (host.public, joiner.public);
        let a = host.finish(&h, &j).unwrap();
        let b = joiner.finish(&h, &j).unwrap();
        assert_eq!(a.to_joiner.encode(), b.to_joiner.encode());
        assert_eq!(a.to_host.encode(), b.to_host.encode());
        assert_ne!(a.to_host.encode(), a.to_joiner.encode());

        let host = Pake::start(&code, "sid").unwrap();
        let wrong = Pake::start(&Code::parse("K7X-42PR").unwrap(), "sid").unwrap();
        let (h, j) = (host.public, wrong.public);
        assert_ne!(
            host.finish(&h, &j).unwrap().to_joiner.encode(),
            wrong.finish(&h, &j).unwrap().to_joiner.encode()
        );
    }

    #[test]
    fn room_keys_derive_stable_ids_and_keys() {
        let key = RoomKey([7; 32]);
        assert_eq!(key.room_id().len(), 22);
        assert_eq!(key.room_id(), RoomKey([7; 32]).room_id());
        assert_ne!(key.meta_key("a").encode(), key.blob_key("a").encode());
        assert_ne!(key.meta_key("a").encode(), key.meta_key("b").encode());
        assert_eq!(RoomKey::parse(&key.encode()).unwrap().0, key.0);
    }
}
