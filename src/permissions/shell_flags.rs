//! Options that turn a safe-listed command into one that runs another program
//! or writes a file: `fd -x`, `find -fprint`, `git fetch --upload-pack`,
//! `sort --compress-program`, `xxd in out` and the like. The command's name
//! alone says nothing about these, so the words are checked one by one.

/// Whether one simple command (its words, program first) uses an option or
/// argument that runs another program or writes a file. Quotes are resolved
/// the way the shell resolves them (`'-x'` and `-e'x'ec` are `-x` and
/// `-exec`); a command that doesn't split counts as one that does.
pub(super) fn runs_or_writes(part: &str) -> bool {
    let Ok(words) = shell_words::split(part) else {
        return true;
    };
    let Some(first) = words.first() else {
        return false;
    };
    let prog = first.rsplit('/').next().unwrap_or(first);
    let rest = &words[1..];
    match prog {
        "fd" | "fdfind" => rest.iter().any(|w| {
            long(w).is_some_and(|n| n.starts_with("exec"))
                || short(w).is_some_and(|s| s.contains(['x', 'X']))
        }),
        "find" => rest.iter().any(|w| {
            ["-exec", "-ok", "-delete", "-fprint", "-fls"]
                .iter()
                .any(|p| w.starts_with(p))
        }),
        "sort" => rest.iter().any(|w| {
            short(w).is_some_and(|s| s.contains('o'))
                || long(w)
                    .is_some_and(|n| abbrev(n, "output", 1) || abbrev(n, "compress-program", 2))
        }),
        // `--pre` runs a program on each file (`--pre-glob` and `--pretty`
        // don't); ripgrep takes no abbreviations.
        "rg" => rest
            .iter()
            .any(|w| long(w).is_some_and(|n| n == "pre" || n == "hostname-bin")),
        "ag" => rest
            .iter()
            .any(|w| long(w).is_some_and(|n| abbrev(n, "pager", 3))),
        "xxd" => xxd_writes(rest),
        // `-o file` writes the listing; `-R` writes an `00Tree.html` into
        // every directory it visits.
        "tree" => rest.iter().any(|w| {
            short(w).is_some_and(|s| s.contains(['o', 'R']))
                || long(w).is_some_and(|n| n.starts_with('o'))
        }),
        // `-C` compiles a magic file into the working directory.
        "file" => rest.iter().any(|w| {
            short(w).is_some_and(|s| s.contains('C'))
                || long(w).is_some_and(|n| abbrev(n, "compile", 2))
        }),
        // `-i` edits in place; `-s` writes one file per result.
        "yq" => rest.iter().any(|w| {
            short(w).is_some_and(|s| s.contains(['i', 's']))
                || long(w).is_some_and(|n| matches!(n, "inplace" | "in-place" | "split-exp"))
        }),
        "cloc" => rest.iter().any(|w| {
            long(w).is_some_and(|n| {
                [
                    "report-file",
                    "out",
                    "sql",
                    "sql-append",
                    "write-lang-def",
                    "write-lang-def-incl-dup",
                    "counted",
                    "ignored",
                    "categorized",
                    "found",
                    "unique",
                    "extract-with",
                ]
                .iter()
                .any(|full| abbrev(n, full, 2))
            })
        }),
        "git" => git_runs_or_writes(rest),
        // Options that run a program: a compressor, a checkpoint or volume
        // script, a command fed each file, a remote shell.
        "tar" | "bsdtar" | "gtar" => rest.iter().enumerate().any(|(i, w)| {
            long(w).is_some_and(|n| {
                [
                    "checkpoint-action",
                    "use-compress-program",
                    "to-command",
                    "info-script",
                    "new-volume-script",
                    "rsh-command",
                    "rmt-command",
                ]
                .iter()
                .any(|full| abbrev(n, full, 3))
            }) || short(w).is_some_and(|s| s.contains(['I', 'F']))
                // Old style: `tar xIf zstd a.tar`.
                || (i == 0 && !w.starts_with('-') && w.contains(['I', 'F']))
        }),
        "go" => {
            // `go env -w GOFLAGS=-toolexec=…` persists a program for every
            // later build; `-toolexec`, `-exec` and `-vettool` run one now,
            // as does a linker named in `-ldflags`. `-o` writes the build
            // (a program of the project's choosing) wherever it says.
            (rest.first().is_some_and(|s| s == "env")
                && rest.iter().any(|w| w == "-w" || w == "-u"))
                || rest
                    .iter()
                    .any(|w| w.contains("-extld") || w.contains("-extar"))
                || (rest
                    .first()
                    .is_some_and(|s| matches!(s.as_str(), "build" | "test" | "install"))
                    && rest.iter().any(|w| {
                        let name = w.trim_start_matches('-');
                        w.starts_with('-') && (name == "o" || name.starts_with("o="))
                    }))
                || rest.iter().any(|w| {
                    let name = w.trim_start_matches('-');
                    w.starts_with('-')
                        && matches!(
                            name.split('=').next().unwrap_or(""),
                            "toolexec" | "exec" | "vettool"
                        )
                })
        }
        // `--config 'build.rustc-wrapper="…"'` names a program to run.
        "cargo" => rest
            .iter()
            .any(|w| long(w).is_some_and(|n| n == "config") || w == "-Z"),
        _ => false,
    }
}

/// `git <sub> …` beyond the flags the caller already refuses everywhere
/// (`-c`, `--output`, `--ext-diff`).
fn git_runs_or_writes(rest: &[String]) -> bool {
    let Some(sub) = rest.first() else {
        return false;
    };
    let args = &rest[1..];
    match sub.as_str() {
        // `--upload-pack` runs a program; a URL or path fetches from (and
        // sends ref names to) a repository the user never configured, which
        // is also a way to carry data out. Only configured remotes by name.
        "fetch" => args.iter().any(|w| {
            if let Some(n) = long(w) {
                return abbrev(n, "upload-pack", 3);
            }
            if w.starts_with('-') {
                return false;
            }
            w.contains(':') || w.contains('@') || w.contains('\\') || w.starts_with(['/', '.', '~'])
        }),
        // `-O`/`--open-files-in-pager` runs the named program on the matches.
        "grep" => args.iter().any(|w| {
            short(w).is_some_and(|s| s.contains('O'))
                || long(w).is_some_and(|n| abbrev(n, "open-files-in-pager", 2))
        }),
        // `-x`/`--exec` runs a command after each commit.
        "rebase" => args.iter().any(|w| {
            short(w).is_some_and(|s| s.contains('x'))
                || long(w).is_some_and(|n| abbrev(n, "exec", 2))
        }),
        // `expire`, `delete` and `drop` rewrite the reflog.
        "reflog" => args
            .iter()
            .find(|w| !w.starts_with('-'))
            .is_some_and(|w| matches!(w.as_str(), "expire" | "delete" | "drop")),
        _ => false,
    }
}

/// `xxd [options] [infile [outfile]]`: a second file name is written to,
/// with or without `-r`.
fn xxd_writes(rest: &[String]) -> bool {
    const VALUED: &[&str] = &[
        "-c",
        "-cols",
        "-g",
        "-groupsize",
        "-l",
        "-len",
        "-s",
        "-seek",
        "-o",
        "-offset",
        "-n",
        "-name",
    ];
    let mut files = 0;
    let mut it = rest.iter();
    while let Some(w) = it.next() {
        if w.len() > 1 && w.starts_with('-') {
            if VALUED.contains(&w.as_str()) {
                it.next();
            }
        } else {
            files += 1;
        }
    }
    files > 1
}

/// The letters of a short-option cluster (`-Hx` → `Hx`).
fn short(w: &str) -> Option<&str> {
    w.strip_prefix('-')
        .filter(|s| !s.is_empty() && !s.starts_with('-'))
}

/// The name of a long option (`--exec=x` → `exec`).
fn long(w: &str) -> Option<&str> {
    w.strip_prefix("--")
        .filter(|s| !s.is_empty())
        .map(|s| s.split('=').next().unwrap_or(s))
}

/// Whether `name` spells `full` or an abbreviation of it at least `min`
/// letters long: getopt and git accept any unambiguous prefix.
fn abbrev(name: &str, full: &str, min: usize) -> bool {
    name.len() >= min && full.starts_with(name)
}
