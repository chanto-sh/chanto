//! End-to-end encryption for `/remote cloud`.
//!
//! The relay is blind: it stores [`Sealed`] envelopes and never has the key.
//! The link's `#k=` fragment carries a **bootstrap** key, used for two things:
//! opening the (small) metadata envelope and sealing the reader's first
//! message, which carries the reader's ephemeral P-256 public key. The CLI
//! answers with its own ephemeral public key in the same link (`#p=`), so both
//! ends can run ECDH and derive the **session** keys the conversation, the
//! events and the commands actually use. The CLI drops its private half the
//! moment a reader joins, so a link that leaks later can't open what it
//! recorded.
//!
//! Session keys are HKDF-SHA256 per purpose, sealed with AES-256-GCM using a
//! counter nonce (a random four-byte prefix and the message's sequence number).
//! The session id, the reader, the purpose and the sequence are all in the
//! authenticated data, so envelopes can't be replayed, reordered or moved
//! between sessions.
//!
//! What this hides is the content — the conversation, what you type, the
//! directory and machine. It does not hide that a session exists, when, or
//! roughly how big its messages are, which the relay needs to route them.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use anyhow::{Context, Result, bail};
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use aws_lc_rs::agreement::{self, UnparsedPublicKey};
use aws_lc_rs::digest::{SHA256, digest};
use aws_lc_rs::hkdf::{HKDF_SHA256, Salt};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use chanto_proto::remote::{KEY_FRAGMENT, PUBLIC_FRAGMENT, RemoteCommand, Sealed};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The bootstrap envelope: the link's key, a random nonce, no session binding.
const BOOTSTRAP_VERSION: u8 = 1;
/// Session envelopes: derived keys, counter nonces, session and reader bound.
const VERSION: u8 = 2;
/// Separates the bootstrap derivation from anything else that might ever use
/// the same key.
const BOOTSTRAP_CONTEXT: &[u8] = b"chanto-remote-v1";
/// Separates the session derivation (and the associated data) from it.
const CONTEXT: &[u8] = b"chanto-remote-v2";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Purpose {
    Meta,
    Snapshot,
    Events,
    Commands,
}

impl Purpose {
    pub fn label(self) -> &'static str {
        match self {
            Purpose::Meta => "meta",
            Purpose::Snapshot => "snapshot",
            Purpose::Events => "events",
            Purpose::Commands => "commands",
        }
    }
}

/// The link's bootstrap key, as the CLI holds it.
#[derive(Clone)]
pub struct SessionKey([u8; 32]);

impl SessionKey {
    pub fn generate() -> Result<Self> {
        let mut key = [0u8; 32];
        SystemRandom::new()
            .fill(&mut key)
            .map_err(|_| anyhow::anyhow!("no randomness available"))?;
        Ok(Self(key))
    }

    /// The key as it goes in the link (`#k=<this>`).
    pub fn token(&self) -> String {
        B64.encode(self.0)
    }

    #[cfg(test)]
    pub fn from_token(token: &str) -> Result<Self> {
        let bytes = B64.decode(token.trim()).context("not a session key")?;
        let key: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("not a 32-byte session key"))?;
        Ok(Self(key))
    }

    /// A session key from a link the CLI printed, if it carries one.
    #[cfg(test)]
    pub fn from_link(link: &str) -> Option<Self> {
        let value = fragment(link, KEY_FRAGMENT)?;
        Self::from_token(&value).ok()
    }

    /// The link to open in the dashboard: the session's URL, its bootstrap key
    /// and the CLI's handshake public key.
    pub fn link(&self, url: &str) -> String {
        format!("{url}#{}={}", KEY_FRAGMENT, self.token())
    }

    /// The link with the ephemeral handshake key a reader needs to join.
    pub fn link_with(&self, url: &str, public: &str) -> String {
        format!("{}&{}={}", self.link(url), PUBLIC_FRAGMENT, public)
    }

    /// A short, stable value both ends can show to confirm they hold the same
    /// key (and so that the link someone opened is the session they meant).
    pub fn fingerprint(&self) -> String {
        let hash = digest(&SHA256, &self.0);
        hash.as_ref()[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Seal one bootstrap value (the metadata, before a reader has joined).
    pub fn seal<T: Serialize>(&self, purpose: Purpose, value: &T) -> Result<Sealed> {
        let id = random_bytes::<16>()?;
        let nonce = random_bytes::<12>()?;
        let mut body = serde_json::to_vec(value).context("sealing it")?;
        let aad = bootstrap_aad(purpose, &id);
        self.key(purpose)?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&aad),
                &mut body,
            )
            .map_err(|_| anyhow::anyhow!("couldn't seal it"))?;
        Ok(Sealed {
            v: BOOTSTRAP_VERSION,
            id: B64.encode(id),
            n: B64.encode(nonce),
            c: B64.encode(body),
            seq: None,
            kind: None,
            viewer: None,
        })
    }

    /// Open one bootstrap value. Fails if it was sealed for another purpose,
    /// with another key, or if it was altered.
    pub fn open<T: DeserializeOwned>(&self, purpose: Purpose, sealed: &Sealed) -> Result<T> {
        if sealed.v != BOOTSTRAP_VERSION {
            bail!("this session's key is for a different version of remote control");
        }
        let id = B64.decode(&sealed.id).context("a broken envelope")?;
        let nonce: [u8; 12] = B64
            .decode(&sealed.n)
            .context("a broken envelope")?
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("a broken envelope"))?;
        let mut body = B64.decode(&sealed.c).context("a broken envelope")?;
        let aad = bootstrap_aad(purpose, &id);
        let plain = self
            .key(purpose)?
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&aad),
                &mut body,
            )
            .map_err(|_| anyhow::anyhow!("couldn't open it: wrong key, or it was altered"))?;
        serde_json::from_slice(plain).context("opening it")
    }

    fn key(&self, purpose: Purpose) -> Result<LessSafeKey> {
        let prk = Salt::new(HKDF_SHA256, BOOTSTRAP_CONTEXT).extract(&self.0);
        let info = [BOOTSTRAP_CONTEXT, b" ", purpose.label().as_bytes()];
        let material = prk
            .expand(&info, HKDF_SHA256)
            .map_err(|_| anyhow::anyhow!("couldn't derive the session key"))?;
        let mut bytes = [0u8; 32];
        material
            .fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("couldn't derive the session key"))?;
        let unbound = UnboundKey::new(&AES_256_GCM, &bytes)
            .map_err(|_| anyhow::anyhow!("couldn't use the session key"))?;
        Ok(LessSafeKey::new(unbound))
    }
}

/// The CLI's half of the handshake: an ephemeral P-256 key, published in the
/// link as `#p=`. It is dropped once a reader joins.
pub struct Handshake {
    private: agreement::PrivateKey,
    public: Vec<u8>,
}

impl Handshake {
    pub fn generate() -> Result<Self> {
        let private = agreement::PrivateKey::generate(&agreement::ECDH_P256)
            .map_err(|_| anyhow::anyhow!("no randomness available"))?;
        let public = private
            .compute_public_key()
            .map_err(|_| anyhow::anyhow!("couldn't make a handshake key"))?
            .as_ref()
            .to_vec();
        Ok(Self { private, public })
    }

    pub fn public_token(&self) -> String {
        B64.encode(&self.public)
    }

    /// Derive the session keys from the reader's ephemeral public key (`#p=`
    /// carries ours). Failure leaves the handshake intact.
    pub fn complete(&self, viewer: &str, public: &str, session_id: &str) -> Result<ViewKey> {
        let peer = B64.decode(public).context("not a handshake key")?;
        let unparsed = UnparsedPublicKey::new(&agreement::ECDH_P256, peer);
        let shared = agreement::agree(
            &self.private,
            unparsed,
            anyhow::anyhow!("not a handshake key"),
            |secret| Ok(secret.to_vec()),
        )?;
        Ok(ViewKey::new(
            shared,
            session_id.to_string(),
            viewer.to_string(),
        ))
    }
}

/// The keys a joined reader and the CLI share. Purpose keys are derived from
/// the ECDH secret; each purpose has its own counter, used as the nonce and
/// authenticated.
pub struct ViewKey {
    shared: Vec<u8>,
    session_id: String,
    viewer: String,
    next: Mutex<HashMap<Purpose, u64>>,
}

impl ViewKey {
    fn new(shared: Vec<u8>, session_id: String, viewer: String) -> Self {
        Self {
            shared,
            session_id,
            viewer,
            next: Mutex::new(HashMap::new()),
        }
    }

    pub fn viewer(&self) -> &str {
        &self.viewer
    }

    /// Whether a command may be acted on: it must be a version 2 envelope
    /// addressed to this reader, with a sequence above everything already
    /// acted on. A relay that replays, reorders or rewrites a command cannot
    /// get it through this check.
    pub fn accepts(&self, last_seq: u64, sealed: &Sealed) -> bool {
        sealed.v == VERSION
            && sealed.viewer.as_deref() == Some(self.viewer())
            && sealed.seq.is_some_and(|seq| seq > last_seq)
    }

    /// Encrypt one value. `seq` starts at 1 per purpose and is authenticated.
    pub fn seal<T: Serialize>(&self, purpose: Purpose, value: &T) -> Result<Sealed> {
        let seq = self.next(purpose);
        let prefix = random_bytes::<4>()?;
        self.seal_with(purpose, value, seq, prefix)
    }

    fn seal_with<T: Serialize>(
        &self,
        purpose: Purpose,
        value: &T,
        seq: u64,
        prefix: [u8; 4],
    ) -> Result<Sealed> {
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&prefix);
        nonce[4..].copy_from_slice(&seq.to_be_bytes());
        // The id names the envelope for dedupe; deriving it from the session
        // secret and the nonce keeps it unique per message without another
        // random draw, and makes the whole envelope reproducible in a vector.
        let mut material = Vec::with_capacity(self.shared.len() + 16);
        material.extend_from_slice(&self.shared);
        material.extend_from_slice(purpose.label().as_bytes());
        material.extend_from_slice(&nonce);
        let id: [u8; 16] = digest(&SHA256, &material).as_ref()[..16]
            .try_into()
            .expect("a SHA-256 digest is longer than 16 bytes");
        let mut body = serde_json::to_vec(value).context("sealing it")?;
        let aad = self.aad(purpose, &id, seq);
        self.key(purpose)?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&aad),
                &mut body,
            )
            .map_err(|_| anyhow::anyhow!("couldn't seal it"))?;
        Ok(Sealed {
            v: VERSION,
            id: B64.encode(id),
            n: B64.encode(nonce),
            c: B64.encode(body),
            seq: Some(seq),
            kind: Some(purpose.label().to_string()),
            viewer: Some(self.viewer.clone()),
        })
    }

    /// Decrypt one value, checking that it is for this reader and purpose.
    pub fn open<T: DeserializeOwned>(&self, purpose: Purpose, sealed: &Sealed) -> Result<T> {
        if sealed.v != VERSION {
            bail!("this envelope is for a different version of remote control");
        }
        if sealed.kind.as_deref() != Some(purpose.label()) {
            bail!("this envelope is for something else");
        }
        if sealed.viewer.as_deref() != Some(self.viewer.as_str()) {
            bail!("this envelope is for another reader");
        }
        let seq = sealed.seq.context("a broken envelope")?;
        let id = B64.decode(&sealed.id).context("a broken envelope")?;
        let nonce: [u8; 12] = B64
            .decode(&sealed.n)
            .context("a broken envelope")?
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("a broken envelope"))?;
        // The nonce's counter half must be the sequence the tag covers.
        if nonce[4..] != seq.to_be_bytes() {
            bail!("a broken envelope");
        }
        let mut body = B64.decode(&sealed.c).context("a broken envelope")?;
        let aad = self.aad(purpose, &id, seq);
        let plain = self
            .key(purpose)?
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&aad),
                &mut body,
            )
            .map_err(|_| anyhow::anyhow!("couldn't open it: wrong key, or it was altered"))?;
        serde_json::from_slice(plain).context("opening it")
    }

    fn next(&self, purpose: Purpose) -> u64 {
        let mut next = self.next.lock().unwrap();
        let seq = next.entry(purpose).or_insert(0);
        *seq += 1;
        *seq
    }

    fn aad(&self, purpose: Purpose, id: &[u8], seq: u64) -> Vec<u8> {
        let mut aad = Vec::with_capacity(
            CONTEXT.len() + self.session_id.len() + self.viewer.len() + id.len() + 64,
        );
        aad.extend_from_slice(CONTEXT);
        aad.push(b'\n');
        aad.extend_from_slice(purpose.label().as_bytes());
        aad.push(b'\n');
        aad.extend_from_slice(self.session_id.as_bytes());
        aad.push(b'\n');
        aad.extend_from_slice(self.viewer.as_bytes());
        aad.push(b'\n');
        aad.extend_from_slice(id);
        aad.push(b'\n');
        aad.extend_from_slice(seq.to_string().as_bytes());
        aad
    }

    fn key(&self, purpose: Purpose) -> Result<LessSafeKey> {
        let prk = Salt::new(HKDF_SHA256, CONTEXT).extract(&self.shared);
        let mut info =
            Vec::with_capacity(CONTEXT.len() + self.session_id.len() + self.viewer.len() + 32);
        info.extend_from_slice(CONTEXT);
        info.push(b'\n');
        info.extend_from_slice(purpose.label().as_bytes());
        info.push(b'\n');
        info.extend_from_slice(self.session_id.as_bytes());
        info.push(b'\n');
        info.extend_from_slice(self.viewer.as_bytes());
        let info = [info.as_slice()];
        let material = prk
            .expand(&info, HKDF_SHA256)
            .map_err(|_| anyhow::anyhow!("couldn't derive the session key"))?;
        let mut bytes = [0u8; 32];
        material
            .fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("couldn't derive the session key"))?;
        let unbound = UnboundKey::new(&AES_256_GCM, &bytes)
            .map_err(|_| anyhow::anyhow!("couldn't use the session key"))?;
        Ok(LessSafeKey::new(unbound))
    }
}

/// Everything the CLI needs: the link's bootstrap key, the pending handshake
/// and, once a reader joins, the session keys.
pub struct RemoteCrypto {
    bootstrap: SessionKey,
    session_id: String,
    handshake: Mutex<Option<Handshake>>,
    viewer: RwLock<Option<std::sync::Arc<ViewKey>>>,
}

impl RemoteCrypto {
    /// The key and handshake are made before the relay names the session (the
    /// sealed metadata goes in the create request), so they're handed in here.
    pub fn with_parts(
        session_id: impl Into<String>,
        bootstrap: SessionKey,
        handshake: Handshake,
    ) -> Self {
        Self {
            bootstrap,
            session_id: session_id.into(),
            handshake: Mutex::new(Some(handshake)),
            viewer: RwLock::new(None),
        }
    }

    pub fn fingerprint(&self) -> String {
        self.bootstrap.fingerprint()
    }

    pub fn link(&self, url: &str) -> String {
        match self.handshake.lock().unwrap().as_ref() {
            Some(h) => self.bootstrap.link_with(url, &h.public_token()),
            None => self.bootstrap.link(url),
        }
    }

    /// A snapshot goes under the session keys once someone has joined; before
    /// that there is no reader, and it stays under the link's key.
    pub fn seal_snapshot<T: Serialize>(&self, value: &T) -> Result<Sealed> {
        match self.viewer() {
            Some(key) => key.seal(Purpose::Snapshot, value),
            None => self.bootstrap.seal(Purpose::Snapshot, value),
        }
    }

    pub fn seal_event<T: Serialize>(&self, value: &T) -> Result<Sealed> {
        match self.viewer() {
            Some(key) => key.seal(Purpose::Events, value),
            None => self.bootstrap.seal(Purpose::Events, value),
        }
    }

    pub fn viewer(&self) -> Option<std::sync::Arc<ViewKey>> {
        self.viewer.read().unwrap().clone()
    }

    /// The reader's first message. Only the first one is accepted; the private
    /// half is dropped afterwards, so a later leak of the link can't re-derive
    /// the session keys.
    pub fn join(&self, viewer: &str, public: &str) -> Result<Option<std::sync::Arc<ViewKey>>> {
        if self.viewer.read().unwrap().is_some() {
            return Ok(None);
        }
        let mut handshake = self.handshake.lock().unwrap();
        let Some(h) = handshake.as_ref() else {
            return Ok(None);
        };
        let key = h.complete(viewer, public, &self.session_id)?;
        // Take the handshake while holding its lock: two joins racing here
        // must not both derive a key from the same private half.
        *handshake = None;
        drop(handshake);
        let key = std::sync::Arc::new(key);
        *self.viewer.write().unwrap() = Some(key.clone());
        Ok(Some(key))
    }

    /// Open a reader's first message (sealed with the link's key).
    pub fn open_join(&self, sealed: &Sealed) -> Result<RemoteCommand> {
        self.bootstrap.open(Purpose::Commands, sealed)
    }
}

#[cfg(test)]
fn fragment(link: &str, name: &str) -> Option<String> {
    let (_, fragment) = link.split_once('#')?;
    fragment.split('&').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

fn bootstrap_aad(purpose: Purpose, id: &[u8]) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(BOOTSTRAP_CONTEXT.len() + purpose.label().len() + id.len() + 2);
    aad.extend_from_slice(BOOTSTRAP_CONTEXT);
    aad.push(b'\n');
    aad.extend_from_slice(purpose.label().as_bytes());
    aad.push(b'\n');
    aad.extend_from_slice(id);
    aad
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    SystemRandom::new()
        .fill(&mut out)
        .map_err(|_| anyhow::anyhow!("no randomness available"))?;
    Ok(out)
}

#[cfg(test)]
mod tests;
