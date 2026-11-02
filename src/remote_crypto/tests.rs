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
    let cli = agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, &[0x11; 32]).unwrap();
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
    let another_session = ViewKey::new(keys.shared.clone(), "session-b".into(), "reader".into());
    assert!(
        another_session
            .open::<serde_json::Value>(Purpose::Commands, &good)
            .is_err()
    );
    let another_reader = ViewKey::new(keys.shared.clone(), "session-a".into(), "reader-2".into());
    assert!(
        another_reader
            .open::<serde_json::Value>(Purpose::Commands, &good)
            .is_err()
    );
}
