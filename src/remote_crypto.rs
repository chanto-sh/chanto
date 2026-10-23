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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_sealed_value_opens_with_the_same_key_and_purpose() {
        let key = SessionKey::generate().unwrap();
        let sealed = key
            .seal(Purpose::Events, &json!({"type": "text", "text": "hi"}))
            .unwrap();
        let opened: serde_json::Value = key.open(Purpose::Events, &sealed).unwrap();
        assert_eq!(opened, json!({"type": "text", "text": "hi"}));
        // Two seals of the same value look different.
        let again = key
            .seal(Purpose::Events, &json!({"type": "text", "text": "hi"}))
            .unwrap();
        assert_ne!(sealed, again);
    }

    #[test]
    fn another_key_or_another_purpose_does_not_open_it() {
        let key = SessionKey::generate().unwrap();
        let other = SessionKey::generate().unwrap();
        let sealed = key
            .seal(Purpose::Events, &json!({"text": "private"}))
            .unwrap();
        assert!(
            other
                .open::<serde_json::Value>(Purpose::Events, &sealed)
                .is_err()
        );
        assert!(
            key.open::<serde_json::Value>(Purpose::Commands, &sealed)
                .is_err()
        );
    }

    #[test]
    fn an_altered_envelope_does_not_open() {
        let key = SessionKey::generate().unwrap();
        let mut sealed = key
            .seal(Purpose::Events, &json!({"text": "private"}))
            .unwrap();
        let mut body = B64.decode(&sealed.c).unwrap();
        let last = body.len() - 1;
        body[last] ^= 1;
        sealed.c = B64.encode(body);
        assert!(
            key.open::<serde_json::Value>(Purpose::Events, &sealed)
                .is_err()
        );
    }

    #[test]
    fn the_key_travels_only_in_the_links_fragment() {
        let key = SessionKey::generate().unwrap();
        let link = key.link("https://app.example/app/sessions/abc");
        assert_eq!(
            link,
            format!("https://app.example/app/sessions/abc#k={}", key.token())
        );
        assert!(SessionKey::from_link(&link).is_some_and(|k| k.token() == key.token()));
        // No fragment, no key.
        assert!(SessionKey::from_link("https://app.example/app/sessions/abc").is_none());
    }

    #[test]
    fn the_handshake_key_rides_in_the_link_too() {
        let key = SessionKey::generate().unwrap();
        let public = B64.encode([7u8; 65]);
        let link = key.link_with("https://app.example/app/sessions/abc", &public);
        assert!(link.contains(&format!("#k={}", key.token())));
        assert!(link.contains(&format!("&p={public}")));
        assert_eq!(
            fragment(&link, PUBLIC_FRAGMENT).as_deref(),
            Some(public.as_str())
        );
    }

    /// A reader and the CLI derive the same keys from each other's ephemeral
    /// public keys, and a snapshot sealed by one opens with the other.
    #[test]
    fn both_sides_derive_the_same_session_keys() {
        let cli = Handshake::generate().unwrap();
        let reader = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        let reader_public = B64.encode(reader.compute_public_key().unwrap().as_ref());

        let cli_keys = cli
            .complete("viewer-1", &reader_public, "session-1")
            .unwrap();
        let reader_keys = {
            // The reader's half: agree with the CLI's public key, then derive
            // with the same label (this is what the browser does).
            let shared = agreement::agree(
                &reader,
                UnparsedPublicKey::new(
                    &agreement::ECDH_P256,
                    B64.decode(cli.public_token()).unwrap(),
                ),
                (),
                |secret| Ok(secret.to_vec()),
            )
            .unwrap();
            ViewKey::new(shared, "session-1".to_string(), "viewer-1".to_string())
        };

        let sealed = cli_keys
            .seal(Purpose::Events, &json!({"text": "hi"}))
            .unwrap();
        assert_eq!(sealed.v, 2);
        assert_eq!(sealed.seq, Some(1));
        let opened: serde_json::Value = reader_keys.open(Purpose::Events, &sealed).unwrap();
        assert_eq!(opened, json!({"text": "hi"}));

        // The reader's commands open on the CLI, with increasing sequence.
        let command = reader_keys
            .seal(Purpose::Commands, &json!({"kind": "interrupt"}))
            .unwrap();
        assert_eq!(command.seq, Some(1));
        assert!(
            cli_keys
                .open::<serde_json::Value>(Purpose::Commands, &command)
                .is_ok()
        );
        // The same envelope can't be opened for another purpose or reader.
        assert!(
            cli_keys
                .open::<serde_json::Value>(Purpose::Events, &command)
                .is_err()
        );
        let another = ViewKey::new(
            cli_keys.shared.clone(),
            "session-1".to_string(),
            "viewer-2".to_string(),
        );
        assert!(
            another
                .open::<serde_json::Value>(Purpose::Commands, &command)
                .is_err()
        );
    }

    #[test]
    fn a_replayed_sequence_is_visible_and_the_counter_moves_on() {
        let cli = Handshake::generate().unwrap();
        let reader = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        let reader_public = B64.encode(reader.compute_public_key().unwrap().as_ref());
        let keys = cli.complete("v", &reader_public, "s").unwrap();
        let first = keys.seal(Purpose::Events, &json!({"n": 1})).unwrap();
        let second = keys.seal(Purpose::Events, &json!({"n": 2})).unwrap();
        assert_eq!((first.seq, second.seq), (Some(1), Some(2)));
        assert_ne!(first.n, second.n);
        // Flipping the visible sequence breaks the tag.
        let mut moved = second.clone();
        moved.seq = Some(3);
        assert!(
            keys.open::<serde_json::Value>(Purpose::Events, &moved)
                .is_err()
        );
    }

    /// The dashboard derives this same key and opens this same envelope in
    /// its own tests; changing the derivation on either side means changing
    /// both.
    #[test]
    fn an_envelope_sealed_for_the_dashboard_opens() {
        let key = SessionKey::from_token("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        let sealed: Sealed = serde_json::from_str(
            r#"{"v":1,"id":"hldw_hsuQwPwfv6HuV-zXw","n":"OW2raMfyX2Tl6yPU","c":"C3fDKXmijIRTlZqGFbN5qi7dkXsL3gRo20gqE1ZgvFGI3g"}"#,
        )
        .unwrap();
        let opened: serde_json::Value = key.open(Purpose::Meta, &sealed).unwrap();
        assert_eq!(opened, json!({"title": "vector"}));
        assert_eq!(key.fingerprint(), "66687aad");
    }

    #[test]
    fn the_fingerprint_is_stable_and_short() {
        let key = SessionKey::from_token(&SessionKey::generate().unwrap().token()).unwrap();
        assert_eq!(key.fingerprint().len(), 8);
        assert_eq!(
            key.fingerprint(),
            SessionKey::from_token(&key.token()).unwrap().fingerprint()
        );
    }

    /// The dashboard's test hardcodes these; changing the derivation on either
    /// side means changing both. Printed by this test when it fails.
    #[test]
    fn vector_for_the_dashboard() {
        let cli =
            agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, &[0x11; 32]).unwrap();
        let reader =
            agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, &[0x22; 32]).unwrap();
        let cli_public = cli.compute_public_key().unwrap();
        let reader_public = reader.compute_public_key().unwrap();
        let shared = agreement::agree(
            &cli,
            UnparsedPublicKey::new(&agreement::ECDH_P256, reader_public.as_ref()),
            (),
            |secret| Ok(secret.to_vec()),
        )
        .unwrap();
        let key = ViewKey::new(shared, "session-vector".into(), "reader-vector".into());
        let sealed = key
            .seal_with(Purpose::Events, &json!({"text": "vector"}), 1, [7, 7, 7, 7])
            .unwrap();
        assert_eq!(
            B64.encode(cli_public.as_ref()),
            "BAIX5hfwtkQ5KCePlpmeaaI6TywVK99tbN9m5bgCgtTtGUp968uXcS0t2jyoWqh2Wlb0X8dYWZZS8ol8ZTBuV5Q"
        );
        assert_eq!(
            B64.encode(reader_public.as_ref()),
            "BNZak5d8qj0bCBhS_1ennkZfFmBXcwS66tUF3TpIWJzzUBheiVNy32Ih6joTdVfkc_3bZ1XwW9UHw8Uz_OnJEoU"
        );
        assert_eq!(
            serde_json::to_string(&sealed).unwrap(),
            r#"{"v":2,"id":"limlV_VDbLjLtpoeU1vHPw","n":"BwcHBwAAAAAAAAAB","c":"6m3u7vwuWWqWPXUYPYPLhwpPmOlOpeUyVXfcMP6neebI","seq":1,"kind":"events","viewer":"reader-vector"}"#
        );
    }

    /// A relay that replays, reorders or rewrites a command must not get it
    /// accepted: only a version 2 envelope for this reader with a newer
    /// sequence is acted on.
    #[test]
    fn a_command_is_accepted_only_once_and_only_by_its_reader() {
        let cli = Handshake::generate().unwrap();
        let reader = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        let public = B64.encode(reader.compute_public_key().unwrap().as_ref());
        let keys = cli.complete("reader", &public, "session").unwrap();
        let command = |seq| Sealed {
            seq: Some(seq),
            viewer: Some("reader".into()),
            ..keys
                .seal(Purpose::Commands, &json!({"kind": "interrupt"}))
                .unwrap()
        };
        assert!(!keys.accepts(1, &command(1)), "the same sequence twice");
        assert!(!keys.accepts(2, &command(1)), "an older sequence");
        assert!(keys.accepts(1, &command(2)), "a newer sequence");
        assert!(keys.accepts(0, &command(u64::MAX)));

        let mut foreign = command(2);
        foreign.viewer = Some("someone-else".into());
        assert!(!keys.accepts(1, &foreign));
        let mut unsequenced = command(2);
        unsequenced.seq = None;
        assert!(!keys.accepts(1, &unsequenced));
        let mut old_version = command(2);
        old_version.v = 1;
        assert!(!keys.accepts(1, &old_version));
    }

    /// Every seal gets its own sequence, nonce and id; a repeat would mean a
    /// nonce reuse, which breaks AES-GCM outright.
    #[test]
    fn every_seal_gets_one_sequence_and_a_fresh_nonce_and_id() {
        let cli = Handshake::generate().unwrap();
        let reader = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        let public = B64.encode(reader.compute_public_key().unwrap().as_ref());
        let keys = cli.complete("reader", &public, "session").unwrap();
        let mut sequences = Vec::new();
        let mut nonces = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..512 {
            let sealed = keys
                .seal(Purpose::Events, &json!({"text": "same"}))
                .unwrap();
            sequences.push(sealed.seq.unwrap());
            assert!(nonces.insert(sealed.n.clone()), "nonce repeated");
            assert!(ids.insert(sealed.id.clone()), "id repeated");
        }
        assert_eq!(sequences, (1..=512).collect::<Vec<_>>());
        // Each purpose counts on its own.
        assert_eq!(
            keys.seal(Purpose::Commands, &json!({})).unwrap().seq,
            Some(1)
        );
        assert_eq!(
            keys.seal(Purpose::Events, &json!({})).unwrap().seq,
            Some(513)
        );
    }

    /// A relay (or a link that leaked before anyone joined) has the bootstrap
    /// key. Everything sealed after the join must stay closed to it.
    #[test]
    fn a_leaked_link_key_cannot_open_the_conversation() {
        let bootstrap = SessionKey::generate().unwrap();
        let leaked = SessionKey::from_token(&bootstrap.token()).unwrap();
        let crypto = RemoteCrypto::with_parts("session", bootstrap, Handshake::generate().unwrap());
        let early = crypto.seal_snapshot(&json!({"title": "old"})).unwrap();
        assert_eq!(
            leaked
                .open::<serde_json::Value>(Purpose::Snapshot, &early)
                .unwrap(),
            json!({"title": "old"})
        );

        let reader = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        crypto
            .join(
                "reader",
                &B64.encode(reader.compute_public_key().unwrap().as_ref()),
            )
            .unwrap()
            .unwrap();
        let event = crypto
            .seal_event(&json!({"text": "after the join"}))
            .unwrap();
        assert!(
            leaked
                .open::<serde_json::Value>(Purpose::Events, &event)
                .is_err()
        );
        // Nor does downgrading the visible version turn it back into a v1
        // envelope the link's key could try.
        let mut downgraded = event.clone();
        downgraded.v = 1;
        downgraded.seq = None;
        downgraded.kind = None;
        downgraded.viewer = None;
        assert!(
            leaked
                .open::<serde_json::Value>(Purpose::Events, &downgraded)
                .is_err()
        );
    }

    /// The handshake is good for one reader: a second join is refused, the
    /// link stops carrying the key, and the first reader still works.
    #[test]
    fn the_handshake_is_used_once_and_leaves_the_link() {
        let bootstrap = SessionKey::generate().unwrap();
        let token = bootstrap.token();
        let crypto = RemoteCrypto::with_parts("session", bootstrap, Handshake::generate().unwrap());
        let url = "https://app.example/app/sessions/session";
        assert!(crypto.link(url).contains("&p="));

        let first = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        let first_key = crypto
            .join(
                "reader-one",
                &B64.encode(first.compute_public_key().unwrap().as_ref()),
            )
            .unwrap()
            .expect("the first join");
        let after = crypto.link(url);
        assert!(!after.contains("&p="));
        assert_eq!(after, format!("{url}#k={token}"));

        let second = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        assert!(
            crypto
                .join(
                    "reader-two",
                    &B64.encode(second.compute_public_key().unwrap().as_ref())
                )
                .unwrap()
                .is_none()
        );
        let event = crypto
            .seal_event(&json!({"type": "text", "text": "after"}))
            .unwrap();
        assert_eq!(
            first_key
                .open::<serde_json::Value>(Purpose::Events, &event)
                .unwrap(),
            json!({"type": "text", "text": "after"})
        );
    }

    /// Completing with a bogus point fails and leaves the handshake usable.
    #[test]
    fn a_bogus_handshake_point_is_refused_without_burning_the_handshake() {
        let crypto = RemoteCrypto::with_parts(
            "session",
            SessionKey::generate().unwrap(),
            Handshake::generate().unwrap(),
        );
        let cases = vec![
            String::new(),
            "AAAA".to_string(),
            "not base64!".to_string(),
            B64.encode([0u8; 65]),
            B64.encode([4u8; 65]),
            B64.encode([5u8; 33]),
            {
                let mut point = [0xffu8; 33];
                point[0] = 0x02;
                B64.encode(point)
            },
            B64.encode([0x11u8; 64]),
            B64.encode([0xffu8; 200]),
        ];
        for public in &cases {
            assert!(crypto.join("reader", public).is_err(), "{public}");
        }
        assert!(crypto.viewer().is_none());
        let reader = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        assert!(
            crypto
                .join(
                    "reader",
                    &B64.encode(reader.compute_public_key().unwrap().as_ref())
                )
                .unwrap()
                .is_some(),
            "a real join still works after the bad ones"
        );
    }

    /// Hostile envelopes are refused (never panic), and an envelope is bound
    /// to its session, reader, purpose and sequence.
    #[test]
    fn hostile_envelopes_are_refused() {
        let cli = Handshake::generate().unwrap();
        let reader = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        let public = B64.encode(reader.compute_public_key().unwrap().as_ref());
        let keys = cli.complete("reader", &public, "session-a").unwrap();
        let good = keys
            .seal(Purpose::Commands, &json!({"kind": "message", "text": "hi"}))
            .unwrap();

        let cases = [
            Sealed {
                v: 0,
                ..good.clone()
            },
            Sealed {
                v: 3,
                ..good.clone()
            },
            Sealed {
                v: 1,
                ..good.clone()
            },
            Sealed {
                seq: None,
                ..good.clone()
            },
            Sealed {
                seq: Some(0),
                ..good.clone()
            },
            Sealed {
                seq: Some(good.seq.unwrap() + 1),
                ..good.clone()
            },
            Sealed {
                seq: Some(u64::MAX),
                ..good.clone()
            },
            Sealed {
                viewer: None,
                ..good.clone()
            },
            Sealed {
                viewer: Some("other".into()),
                ..good.clone()
            },
            Sealed {
                kind: None,
                ..good.clone()
            },
            Sealed {
                kind: Some("events".into()),
                ..good.clone()
            },
            Sealed {
                id: "".into(),
                ..good.clone()
            },
            Sealed {
                id: "!!!".into(),
                ..good.clone()
            },
            Sealed {
                id: B64.encode([0u8; 16]),
                ..good.clone()
            },
            Sealed {
                n: "".into(),
                ..good.clone()
            },
            Sealed {
                n: B64.encode([0u8; 12]),
                ..good.clone()
            },
            Sealed {
                c: "".into(),
                ..good.clone()
            },
            Sealed {
                c: "AAAA".into(),
                ..good.clone()
            },
            Sealed {
                c: B64.encode(vec![0u8; 4096]),
                ..good.clone()
            },
        ];
        for case in cases {
            assert!(
                keys.open::<serde_json::Value>(Purpose::Commands, &case)
                    .is_err(),
                "{case:?}"
            );
        }

        // A random body of every length never panics and never opens.
        let mut state = 0x9e3779b97f4a7c15u64;
        for len in 0..128 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let body = state.to_be_bytes().repeat(len / 8 + 1)[..len].to_vec();
            let host = Sealed {
                c: B64.encode(body),
                ..good.clone()
            };
            assert!(
                keys.open::<serde_json::Value>(Purpose::Commands, &host)
                    .is_err()
            );
        }

        // The same envelope does not open in another session or for another
        // reader, even with the same ECDH secret.
        let another_session =
            ViewKey::new(keys.shared.clone(), "session-b".into(), "reader".into());
        assert!(
            another_session
                .open::<serde_json::Value>(Purpose::Commands, &good)
                .is_err()
        );
        let another_reader =
            ViewKey::new(keys.shared.clone(), "session-a".into(), "reader-2".into());
        assert!(
            another_reader
                .open::<serde_json::Value>(Purpose::Commands, &good)
                .is_err()
        );
    }
}
