//! Secret redaction (what reaches transcripts, shared sessions and the model).
//!
//! Properties: redaction never panics; and a credential in one of the formats
//! `secrets.rs` recognises, set off from arbitrary surrounding text by a
//! delimiter (space, quote, `=`, …), never survives verbatim in the output.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const ALNUM_U: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_";
const URLSAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
const ALNUM_DASH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-";
const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const UPPER_DIGIT: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const HEX: &[u8] = b"0123456789abcdef";
const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const AWS_SECRET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789/+=";
const BEARER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._~+/-";

/// Characters that end a token: none is a word character or in any token's
/// alphabet.
const SEPARATORS: &[&str] = &[
    " ", "\n", "\t", "\"", "'", "(", ")", ",", ";", "<", ">", "[", "]", "{", "}", "\r\n", "`", "|",
];

#[derive(Arbitrary, Debug)]
struct Input {
    kind: u8,
    extra_len: u8,
    body: Vec<u8>,
    before: String,
    sep_before: u8,
    sep_after: u8,
    after: String,
}

/// `n` characters from `alphabet`, driven by the fuzzer's bytes.
fn chars(body: &[u8], alphabet: &[u8], n: usize, offset: usize) -> String {
    (0..n)
        .map(|i| {
            let b = body
                .get((i + offset) % body.len().max(1))
                .copied()
                .unwrap_or(0) as usize;
            alphabet[(b + i * 7) % alphabet.len()] as char
        })
        .collect()
}

/// A synthetic credential to plant in the text, and the part of it that
/// must not survive redaction.
fn planted(input: &Input) -> (String, String) {
    let b = &input.body;
    let x = input.extra_len as usize % 24;
    let whole = |s: String| (s.clone(), s);
    match input.kind % 19 {
        0 => {
            let p = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"][x % 5];
            whole(format!("{p}{}", chars(b, ALNUM, 36 + x, 0)))
        }
        1 => whole(format!("github_pat_{}", chars(b, ALNUM_U, 50 + x, 0))),
        2 => whole(format!("glpat-{}", chars(b, URLSAFE, 20 + x, 0))),
        3 => {
            let p = ["xoxa-", "xoxb-", "xoxp-", "xoxo-", "xoxs-", "xoxr-"][x % 6];
            whole(format!("{p}{}", chars(b, ALNUM_DASH, 10 + x, 0)))
        }
        4 => {
            let p = ["sk_live_", "sk_test_", "rk_live_", "rk_test_"][x % 4];
            whole(format!("{p}{}", chars(b, ALNUM, 16 + x, 0)))
        }
        5 => whole(format!("sk-ant-{}", chars(b, URLSAFE, 20 + x, 0))),
        6 => {
            let p = ["sk-", "sk-proj-", "sk-svcacct-"][x % 3];
            whole(format!("{p}{}", chars(b, URLSAFE, 32 + x, 0)))
        }
        7 => whole(format!("AIza{}", chars(b, URLSAFE, 35, 0))),
        8 => whole(format!("npm_{}", chars(b, ALNUM, 36, 0))),
        9 => whole(format!("pypi-AgE{}", chars(b, URLSAFE, 50 + x, 0))),
        10 => whole(format!("hf_{}", chars(b, ALPHA, 34 + x, 0))),
        11 => whole(format!(
            "SG.{}.{}",
            chars(b, URLSAFE, 22, 0),
            chars(b, URLSAFE, 43, 22)
        )),
        12 => whole(format!("dop_v1_{}", chars(b, HEX, 64, 0))),
        13 => {
            let p = ["AKIA", "ASIA"][x % 2];
            whole(format!("{p}{}", chars(b, UPPER_DIGIT, 16, 0)))
        }
        14 => whole(format!(
            "eyJ{}.eyJ{}.{}",
            chars(b, URLSAFE, 10 + x, 0),
            chars(b, URLSAFE, 10 + x, 5),
            chars(b, URLSAFE, 10 + x, 9)
        )),
        15 => {
            let key = chars(b, B64, 64 + x, 0);
            // Split so the repository's own secret scan doesn't flag it.
            let pem = concat!("PRIVATE", " KEY-----");
            (
                format!("-----BEGIN RSA {pem}\n{key}\n-----END RSA {pem}"),
                key,
            )
        }
        16 => {
            let v = chars(b, AWS_SECRET, 40, 0);
            (format!("aws_secret_access_key = {v}"), v)
        }
        17 => {
            let v = chars(b, BEARER, 20 + x, 0);
            (format!("Authorization: Bearer {v}"), v)
        }
        _ => {
            let pw = chars(b, ALNUM, 12 + x, 0);
            (format!("https://deploy:{pw}@db.example.com/app"), pw)
        }
    }
}

fuzz_target!(|input: Input| {
    // No panics on arbitrary text, whichever entry point.
    let _ = chanto::fuzzing::redact(&input.before);
    let _ = chanto::fuzzing::contains_secret(&input.before);

    let (embedded, core) = planted(&input);
    let sep = |i: u8| SEPARATORS[i as usize % SEPARATORS.len()];
    let text = format!(
        "{}{}{embedded}{}{}",
        input.before,
        sep(input.sep_before),
        sep(input.sep_after),
        input.after
    );
    // The surrounding text may hold the same characters itself (in a spot no
    // pattern can see, like the middle of a word): judge only the copy put
    // there.
    if text.matches(core.as_str()).count() != 1 {
        return;
    }
    let (out, labels) = chanto::fuzzing::redact_with_labels(&text);
    assert!(
        !out.contains(core.as_str()),
        "secret survived redaction\n  secret: {core:?}\n  input:  {text:?}\n  output: {out:?}"
    );
    assert!(!labels.is_empty(), "redacted without a label: {text:?}");
    assert_eq!(chanto::fuzzing::redact_always(&text), out);
    assert!(chanto::fuzzing::contains_secret(&text), "{text:?}");
});
