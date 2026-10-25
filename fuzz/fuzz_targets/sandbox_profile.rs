//! The macOS Seatbelt profile built from paths the sandbox is given.
//!
//! The project directory, --add-dir paths, configured `allow_write` and
//! `deny_read` entries and the home directory all land in the profile as
//! SBPL string literals, and a directory name is attacker-controlled in a
//! cloned repository. Property: for arbitrary paths, the profile parses (with
//! SBPL's reader rules, as observed from `sandbox-exec`) into exactly the
//! expected rule list, with each inserted path decoding back to the path it
//! came from. So no path can close its literal and add, drop or reorder a
//! rule: `x")) (allow file-write* (subpath "/")) (` stays one string.
//!
//! The reader rules encoded below: `"…"` strings know the escapes `\\`,
//! `\"`, `\n`, `\r` and `\t` (anything else is rejected, since the profile
//! never needs it); `#"…"` regex literals are raw, ending at the next `"`
//! with no escapes; `;` and `#|` would start comments. Also: the secret-key
//! pattern, decoded, still matches a private key under that home and not the
//! public key next to it (checked with the `regex` crate, which agrees with
//! Seatbelt's engine on the escaped literals and groups the pattern uses).
//! That was a real bug, fixed with this target: the pattern sat in a raw
//! `#"…"` literal with its backslashes doubled, so it matched no key at all.
//!
//! Inputs with NUL are skipped: no path holds one, and `sandbox-exec -p`
//! (a C string) can't be spawned with one.

#![no_main]

use arbitrary::Arbitrary;
use chanto::config::{NetworkPolicy, SandboxConfig};
use libfuzzer_sys::fuzz_target;
use std::path::{Path, PathBuf};

#[derive(Arbitrary, Debug)]
struct Input {
    writable: Vec<String>,
    home: String,
    deny_read: Vec<String>,
    network: u8,
    proxy_port: Option<u16>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum Sx {
    Atom(String),
    Str(String),
    Regex(String),
    List(Vec<Sx>),
}

fn atom(s: &str) -> Sx {
    Sx::Atom(s.into())
}

fn list(items: Vec<Sx>) -> Sx {
    Sx::List(items)
}

/// Tokenizes and parses the whole profile; panics on anything SBPL would
/// read differently from plain lists of atoms and strings.
fn parse(src: &str) -> Vec<Sx> {
    let mut stack: Vec<Vec<Sx>> = vec![vec![]];
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\n' | '\t' => {}
            '(' => stack.push(vec![]),
            ')' => {
                let done = stack.pop().unwrap();
                stack
                    .last_mut()
                    .unwrap_or_else(|| panic!("unbalanced `)` in:\n{src}"))
                    .push(Sx::List(done));
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => panic!("unterminated string in:\n{src}"),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('\\') => s.push('\\'),
                            Some('"') => s.push('"'),
                            Some('n') => s.push('\n'),
                            Some('r') => s.push('\r'),
                            Some('t') => s.push('\t'),
                            e => panic!("unexpected escape {e:?} in:\n{src}"),
                        },
                        Some(c) => s.push(c),
                    }
                }
                stack.last_mut().unwrap().push(Sx::Str(s));
            }
            '#' if chars.peek() == Some(&'"') => {
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => panic!("unterminated regex in:\n{src}"),
                        Some('"') => break,
                        Some(c) => s.push(c),
                    }
                }
                stack.last_mut().unwrap().push(Sx::Regex(s));
            }
            ';' | '#' | '\'' | '`' | ',' | '|' => {
                panic!("reader syntax {c:?} outside a string in:\n{src}")
            }
            c if c.is_whitespace() || c.is_control() => {
                panic!("stray {c:?} outside a string in:\n{src}")
            }
            c => {
                let mut a = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || matches!(n, '(' | ')' | '"' | ';') {
                        break;
                    }
                    a.push(n);
                    chars.next();
                }
                stack.last_mut().unwrap().push(Sx::Atom(a));
            }
        }
    }
    assert_eq!(stack.len(), 1, "unbalanced `(` in:\n{src}");
    stack.pop().unwrap()
}

/// What Seatbelt is given for a writable path: resolved where it exists.
fn resolved(p: &str) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.display().to_string())
        .unwrap_or_else(|_| p.to_string())
}

fn deny_resolved(p: &Path) -> String {
    chanto::fuzzing::resolve_for_deny(p).display().to_string()
}

fn expected(input: &Input, cfg: &SandboxConfig, home: &Path) -> Vec<Sx> {
    let mut out = vec![
        list(vec![atom("version"), atom("1")]),
        list(vec![atom("allow"), atom("default")]),
        list(vec![atom("deny"), atom("file-write*")]),
    ];
    let mut allow = vec![atom("allow"), atom("file-write*")];
    for p in &input.writable {
        allow.push(list(vec![atom("subpath"), Sx::Str(resolved(p))]));
    }
    out.push(list(allow));
    let canon_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let cwd = input
        .writable
        .first()
        .map(PathBuf::from)
        .unwrap_or_default();
    let writable: Vec<String> = input
        .writable
        .iter()
        .map(|p| deny_resolved(Path::new(p)))
        .collect();
    for (action, ops, filter, arg) in
        chanto::fuzzing::seatbelt_guard_rules(&cwd, &canon_home, &writable)
    {
        let mut rule = vec![atom(action)];
        rule.extend(ops.iter().map(|o| atom(o)));
        rule.push(list(vec![atom(filter), Sx::Str(arg)]));
        out.push(list(rule));
    }
    for (is_regex, p) in chanto::tools::bash::secret_read_denies(&canon_home, &cfg.deny_read) {
        let filter = if is_regex {
            list(vec![atom("regex"), Sx::Str(p)])
        } else {
            list(vec![atom("subpath"), Sx::Str(deny_resolved(Path::new(&p)))])
        };
        out.push(list(vec![atom("deny"), atom("file-read*"), filter]));
    }
    if cfg.network_policy() != NetworkPolicy::Allow {
        out.push(list(vec![atom("deny"), atom("network*")]));
        out.push(list(vec![
            atom("allow"),
            atom("network*"),
            list(vec![atom("remote"), atom("unix-socket")]),
        ]));
        if let Some(port) = input.proxy_port {
            out.push(list(vec![
                atom("allow"),
                atom("network-outbound"),
                list(vec![
                    atom("remote"),
                    atom("ip"),
                    Sx::Str(format!("localhost:{port}")),
                ]),
            ]));
        }
    }
    // Local IPC: Unix sockets only in the project and the private temp dir.
    let socket = |action: &str, filter: Vec<Sx>| {
        let mut remote = vec![atom("remote"), atom("unix-socket")];
        if !filter.is_empty() {
            remote.push(list(filter));
        }
        list(vec![atom(action), atom("network-outbound"), list(remote)])
    };
    out.push(socket("deny", vec![]));
    for dir in writable
        .first()
        .cloned()
        .into_iter()
        .chain([chanto::fuzzing::seatbelt_tmpdir()])
    {
        out.push(socket("allow", vec![atom("subpath"), Sx::Str(dir)]));
    }
    for p in ["/private/var/run/mDNSResponder", "/private/var/run/syslog"] {
        out.push(socket("allow", vec![atom("path-literal"), Sx::Str(p.into())]));
    }
    let (fixed, risky) = chanto::fuzzing::seatbelt_ipc_fixed();
    out.push(socket(
        "deny",
        vec![atom("path-regex"), Sx::Str(risky.into())],
    ));
    if let Some(agent) = std::env::var_os("SSH_AUTH_SOCK") {
        let agent = regex::escape(&deny_resolved(Path::new(&agent)));
        out.push(socket(
            "deny",
            vec![atom("path-regex"), Sx::Str(format!("^{agent}$"))],
        ));
    }
    out.extend(parse(fixed));
    out
}

/// The decoded secret-key pattern still denies the key, not its `.pub`.
fn check_key_pattern(rules: &[Sx], home: &Path, profile: &str) {
    let canon_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let pattern = rules
        .iter()
        .find_map(|r| match r {
            Sx::List(items) => match items.as_slice() {
                [Sx::Atom(d), Sx::Atom(f), Sx::List(filter)]
                    if d == "deny" && f == "file-read*" =>
                {
                    match filter.as_slice() {
                        [Sx::Atom(r), Sx::Str(p) | Sx::Regex(p)] if r == "regex" => Some(p.clone()),
                        _ => None,
                    }
                }
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| panic!("no secret-key pattern in:\n{profile}"));
    let Ok(re) = regex::Regex::new(&pattern) else {
        panic!("secret-key pattern {pattern:?} doesn't compile:\n{profile}");
    };
    let key = format!("{}/.ssh/id_ed25519", canon_home.display());
    assert!(re.is_match(&key), "{pattern:?} misses {key:?}:\n{profile}");
    let public = format!("{key}.pub");
    assert!(!re.is_match(&public), "{pattern:?} matches {public:?}");
}

fuzz_target!(|input: Input| {
    let mut input = input;
    input.writable.truncate(8);
    input.deny_read.truncate(8);
    let strings = input
        .writable
        .iter()
        .chain(&input.deny_read)
        .chain(std::iter::once(&input.home));
    if strings.into_iter().any(|s| s.contains('\0')) {
        return;
    }
    let cfg = SandboxConfig {
        deny_read: input.deny_read.clone(),
        network: Some(match input.network % 3 {
            0 => NetworkPolicy::Allow,
            1 => NetworkPolicy::Deny,
            _ => NetworkPolicy::Allowlist,
        }),
        ..Default::default()
    };
    let home = PathBuf::from(&input.home);
    let profile = chanto::fuzzing::seatbelt_profile(&input.writable, &home, &cfg, input.proxy_port);
    let rules = parse(&profile);
    let want = expected(&input, &cfg, &home);
    assert_eq!(rules, want, "profile:\n{profile}");
    check_key_pattern(&rules, &home, &profile);
});
