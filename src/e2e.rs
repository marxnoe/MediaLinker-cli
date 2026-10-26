//! End-to-end encryption, byte for byte the same as the MediaLinker web page, so shares encrypted on
//! either side open on the other.
//!
//! Ciphertext: `MLE1`, then the plaintext in 64 KiB chunks, each sealed with AES-256-GCM. The nonce is
//! an 11-byte big-endian chunk counter plus a flag byte that is 1 only on the last chunk (the STREAM
//! construction age uses), so reordered, dropped or truncated chunks fail to decrypt.
//! Plaintext: a 4-byte big-endian header length, a JSON [`Header`], then the content.
//! The key is fresh per share and never sent to the server.

use std::io::{self, Read};

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 4] = b"MLE1";
const CHUNK: usize = 64 * 1024;
const TAG: usize = 16;
const KEY_BYTES: usize = 32;
/// Headers are a few bytes of JSON; anything near this is a damaged share.
const MAX_HEADER: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Header {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

pub struct SecretKey([u8; KEY_BYTES]);

impl SecretKey {
    pub fn generate() -> Result<Self, String> {
        let mut key = [0; KEY_BYTES];
        getrandom::fill(&mut key).map_err(|e| format!("cannot get random bytes for a key: {e}"))?;
        Ok(Self(key))
    }

    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn parse(encoded: &str) -> Option<Self> {
        let raw = URL_SAFE_NO_PAD.decode(encoded.trim()).ok()?;
        Some(Self(raw.try_into().ok()?))
    }

    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0))
    }
}

/// Seals a small message in one go.
pub fn seal(key: &SecretKey, kind: &str, content: Vec<u8>) -> Vec<u8> {
    let header = Header {
        kind: kind.into(),
        name: None,
        mime: None,
    };
    let mut sealed = Vec::new();
    Encryptor::new(key, &header, Box::new(io::Cursor::new(content)))
        .read_to_end(&mut sealed)
        .expect("sealing bytes in memory can't fail");
    sealed
}

/// Opens a message from [`seal`], checking it is of the expected kind.
pub fn unseal(key: &SecretKey, kind: &str, sealed: Vec<u8>) -> io::Result<Vec<u8>> {
    let (header, mut plain) = Decryptor::open(key, Box::new(io::Cursor::new(sealed)))?;
    if header.kind != kind {
        return Err(damaged("unexpected message"));
    }
    let mut content = Vec::new();
    plain.read_to_end(&mut content)?;
    Ok(content)
}

fn nonce(counter: u64, last: bool) -> [u8; 12] {
    let mut n = [0; 12];
    n[3..11].copy_from_slice(&counter.to_be_bytes());
    n[11] = last as u8;
    n
}

/// Reads until `buf` is full or the source ends; returns how much it got.
fn fill(source: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match source.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Fills `buf` and tells whether the source ended with it, holding back one byte of look-ahead.
struct Chunker {
    source: Box<dyn Read>,
    peeked: Option<u8>,
}

impl Chunker {
    fn next(&mut self, buf: &mut [u8]) -> io::Result<(usize, bool)> {
        let mut got = 0;
        if let Some(b) = self.peeked.take() {
            buf[0] = b;
            got = 1;
        }
        got += fill(&mut self.source, &mut buf[got..])?;
        if got < buf.len() {
            return Ok((got, true));
        }
        let mut one = [0];
        let more = fill(&mut self.source, &mut one)? == 1;
        self.peeked = more.then_some(one[0]);
        Ok((got, !more))
    }
}

/// Streams the ciphertext of `header` followed by `content`.
pub struct Encryptor {
    cipher: Aes256Gcm,
    plain: Chunker,
    counter: u64,
    out: Vec<u8>,
    pos: usize,
    done: bool,
}

impl Encryptor {
    pub fn new(key: &SecretKey, header: &Header, content: Box<dyn Read>) -> Self {
        let json = serde_json::to_vec(header).expect("header is plain JSON");
        let mut prefix = (json.len() as u32).to_be_bytes().to_vec();
        prefix.extend(json);
        Self {
            cipher: key.cipher(),
            plain: Chunker {
                source: Box::new(io::Cursor::new(prefix).chain(content)),
                peeked: None,
            },
            counter: 0,
            out: MAGIC.to_vec(),
            pos: 0,
            done: false,
        }
    }
}

impl Read for Encryptor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.out.len() {
            if self.done {
                return Ok(0);
            }
            let mut chunk = vec![0; CHUNK];
            let (n, last) = self.plain.next(&mut chunk)?;
            self.out = self
                .cipher
                .encrypt(Nonce::from_slice(&nonce(self.counter, last)), &chunk[..n])
                .map_err(|_| io::Error::other("encryption failed"))?;
            self.pos = 0;
            self.counter += 1;
            self.done = last;
        }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Streams the content of an encrypted share; any tampering surfaces as an `InvalidData` error.
pub struct Decryptor {
    cipher: Aes256Gcm,
    sealed: Chunker,
    counter: u64,
    out: Vec<u8>,
    pos: usize,
    done: bool,
}

fn damaged(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl Decryptor {
    /// Checks the format, decrypts the header and leaves the reader at the start of the content.
    pub fn open(key: &SecretKey, mut source: Box<dyn Read>) -> io::Result<(Header, Self)> {
        let mut magic = [0; 4];
        if fill(&mut source, &mut magic)? != 4 || &magic != MAGIC {
            return Err(damaged("this share is not in a format ml knows"));
        }
        let mut d = Self {
            cipher: key.cipher(),
            sealed: Chunker { source, peeked: None },
            counter: 0,
            out: Vec::new(),
            pos: 0,
            done: false,
        };
        let mut len = [0; 4];
        if fill(&mut d, &mut len)? != 4 {
            return Err(damaged("the share is damaged"));
        }
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_HEADER {
            return Err(damaged("the share is damaged"));
        }
        let mut json = vec![0; len];
        if fill(&mut d, &mut json)? != len {
            return Err(damaged("the share is damaged"));
        }
        let header = serde_json::from_slice(&json).map_err(|_| damaged("the share is damaged"))?;
        Ok((header, d))
    }
}

impl Read for Decryptor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.out.len() {
            if self.done {
                return Ok(0);
            }
            let mut chunk = vec![0; CHUNK + TAG];
            let (n, last) = self.sealed.next(&mut chunk)?;
            if n < TAG {
                return Err(damaged("the share is cut short or damaged"));
            }
            self.out = self
                .cipher
                .decrypt(Nonce::from_slice(&nonce(self.counter, last)), &chunk[..n])
                .map_err(|_| damaged("wrong key, or the share was damaged"))?;
            self.pos = 0;
            self.counter += 1;
            self.done = last;
        }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(content: Vec<u8>) {
        let key = SecretKey::generate().unwrap();
        let header = Header {
            kind: "file".into(),
            name: Some("a.bin".into()),
            mime: None,
        };
        let mut sealed = Vec::new();
        Encryptor::new(&key, &header, Box::new(io::Cursor::new(content.clone())))
            .read_to_end(&mut sealed)
            .unwrap();
        let (header, mut d) = Decryptor::open(&key, Box::new(io::Cursor::new(sealed))).unwrap();
        assert_eq!(header.name.as_deref(), Some("a.bin"));
        let mut plain = Vec::new();
        d.read_to_end(&mut plain).unwrap();
        assert_eq!(plain, content);
    }

    #[test]
    fn content_survives_any_length() {
        for len in [0, 1, CHUNK - 30, CHUNK, 3 * CHUNK + 7] {
            roundtrip((0..len).map(|i| i as u8).collect());
        }
    }

    #[test]
    fn tampering_and_truncation_are_caught() {
        let key = SecretKey::generate().unwrap();
        let header = Header {
            kind: "text".into(),
            name: None,
            mime: None,
        };
        let mut sealed = Vec::new();
        Encryptor::new(&key, &header, Box::new(io::Cursor::new(vec![7; 2 * CHUNK])))
            .read_to_end(&mut sealed)
            .unwrap();
        let read = |bytes: Vec<u8>, key: &SecretKey| -> io::Result<Vec<u8>> {
            let (_, mut d) = Decryptor::open(key, Box::new(io::Cursor::new(bytes)))?;
            let mut out = Vec::new();
            d.read_to_end(&mut out)?;
            Ok(out)
        };
        // Dropping the final chunk leaves a stream whose new last chunk isn't marked last.
        assert!(read(sealed[..4 + 2 * (CHUNK + TAG)].to_vec(), &key).is_err());
        let mut flipped = sealed.clone();
        flipped[100] ^= 1;
        assert!(read(flipped, &key).is_err());
        assert!(read(sealed.clone(), &SecretKey::generate().unwrap()).is_err());
        assert_eq!(read(sealed, &key).unwrap().len(), 2 * CHUNK);
    }

    #[test]
    fn keys_round_trip_as_base64url() {
        let key = SecretKey::generate().unwrap();
        assert_eq!(key.encode().len(), 43);
        assert_eq!(SecretKey::parse(&key.encode()).unwrap().0, key.0);
        assert!(SecretKey::parse("short").is_none());
    }
}
