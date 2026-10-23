//! Related-test selection for the done-gate: given the files changed this
//! turn, work out which existing tests (pytest / cargo / go, by import
//! analysis and naming) the check command should be scoped to, so the gate
//! runs the relevant tests rather than the whole suite.

use super::*;

/// The related selection for one check. Ok(None): the check isn't a test
/// run that can be narrowed (or the related tests are all of it); Err: it is,
/// but the selection can't be worked out safely, so the full suite runs.
pub(super) fn related(
    check: &str,
    root: &Path,
    changed: &[Changed],
) -> Result<Option<Run>, String> {
    if ["|", ";", "`", "$(", "||", ">", "<"]
        .iter()
        .any(|x| check.contains(x))
    {
        return Ok(None);
    }
    let Some(segs) = check
        .split("&&")
        .map(|s| shell_words::split(s.trim()).ok().filter(|w| !w.is_empty()))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    let Some(runners) = segs.iter().map(|w| runner(w)).collect::<Option<Vec<_>>>() else {
        return Ok(None);
    };
    let (n, name) = runners[0].clone();
    let words = &segs[0];
    let a = args(&words[n..]);
    let one = segs.len() == 1;
    match family(&name) {
        "pytest" if one && a.pos.is_empty() => {
            let files = python_related(root, changed)?;
            Ok(Some(Run::scoped(
                check.trim(),
                vec![check.trim().to_string()],
                files.into_iter().map(|f| (path_arg(&f), Some(f))).collect(),
                false,
            )))
        }
        "unittest"
            if one
                && a.pos.iter().all(|p| p == "discover")
                && !a.flags.iter().any(|f| {
                    matches!(
                        f.as_str(),
                        "-s" | "--start-directory" | "-t" | "--top-level-directory"
                    )
                }) =>
        {
            let files = python_related(root, changed)?;
            let mut targets = Vec::new();
            for f in files {
                let module = f
                    .with_extension("")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                if !module.iter().all(|m| is_ident(m)) {
                    return Err(format!("can't name `{}` as a module", f.display()));
                }
                targets.push((module.join("."), Some(f)));
            }
            let mut prefix = words[..n].join(" ");
            for f in a.flags.iter().filter(|f| {
                matches!(
                    f.as_str(),
                    "-q" | "-v" | "-b" | "-f" | "--quiet" | "--verbose" | "--buffer" | "--failfast"
                )
            }) {
                prefix.push(' ');
                prefix.push_str(f);
            }
            Ok(Some(Run::scoped(
                check.trim(),
                vec![prefix],
                targets,
                false,
            )))
        }
        "cargo test" | "cargo nextest run" if one && a.pos.is_empty() && a.pkgs.is_empty() => {
            let crates = rust_related(root, changed)?;
            Ok(crates.map(|c| {
                Run::scoped(
                    check.trim(),
                    vec![check.trim().to_string()],
                    c.into_iter().map(|c| (c, None)).collect(),
                    true,
                )
            }))
        }
        "go vet" | "go test" if runners.iter().all(|(_, r)| r == "go vet" || r == "go test") => {
            if !segs
                .iter()
                .zip(&runners)
                .all(|(w, (n, _))| args(&w[*n..]).pos == ["..."])
            {
                return Ok(None);
            }
            let Some(dirs) = go_related(root, changed)? else {
                return Ok(None);
            };
            let prefixes = segs
                .iter()
                .map(|w| {
                    w.iter()
                        .filter(|x| x.as_str() != "./...")
                        .map(|x| shell_words::quote(x).into_owned())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect();
            Ok(Some(Run::scoped(
                check.trim(),
                prefixes,
                dirs.into_iter().map(|d| (d, None)).collect(),
                false,
            )))
        }
        "npm test" | "npm run test" | "pnpm test" | "yarn test" | "npx jest" | "vitest" if one => {
            let script = if name.starts_with("npx") {
                name.clone()
            } else {
                let pkg: serde_json::Value = std::fs::read_to_string(root.join("package.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default();
                pkg["scripts"]["test"].as_str().unwrap_or("").to_string()
            };
            let prefix = if script.contains("vitest") {
                "npx vitest related --run"
            } else if script.contains("jest") {
                "npx jest --findRelatedTests"
            } else {
                return Ok(None);
            };
            let files = changed
                .iter()
                .filter(|c| root.join(&c.path).exists())
                .map(|c| (path_arg(&c.path), Some(c.path.clone())))
                .collect::<Vec<_>>();
            if files.is_empty() {
                return Err("only deleted files changed".into());
            }
            Ok(Some(Run::scoped(
                check.trim(),
                vec![prefix.to_string()],
                files,
                false,
            )))
        }
        _ => Ok(None),
    }
}

fn path_arg(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn ext_is(p: &Path, exts: &[&str]) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.contains(&e))
}

/// `*`-only glob match, as pytest's `python_files` uses.
fn glob(pat: &str, name: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || !name.ends_with(last) || name.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// pytest's test file name patterns (`python_files`), from the project's config.
fn python_file_patterns(root: &Path) -> Vec<String> {
    for f in ["pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg"] {
        let Ok(text) = std::fs::read_to_string(root.join(f)) else {
            continue;
        };
        let mut section = String::new();
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with('[') {
                section = l.to_string();
                continue;
            }
            let in_pytest = matches!(
                section.as_str(),
                "[pytest]" | "[tool:pytest]" | "[tool.pytest.ini_options]"
            );
            if let Some(v) = l
                .strip_prefix("python_files")
                .map(str::trim_start)
                .and_then(|v| v.strip_prefix('='))
                .filter(|_| in_pytest)
            {
                let pats: Vec<String> = v
                    .split(|c: char| c.is_whitespace() || c == ',')
                    .map(|p| {
                        p.trim_matches(|c| matches!(c, '"' | '\'' | '[' | ']'))
                            .to_string()
                    })
                    .filter(|p| !p.is_empty())
                    .collect();
                if !pats.is_empty() {
                    return pats;
                }
            }
        }
    }
    vec!["test_*.py".into(), "*_test.py".into()]
}

/// Python test files related to the changed files: changed test files
/// themselves, tests named after a changed module, and tests that import it.
fn python_related(root: &Path, changed: &[Changed]) -> Result<Vec<PathBuf>, String> {
    let patterns = python_file_patterns(root);
    let is_test = |p: &Path| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        patterns.iter().any(|pat| glob(pat, name))
    };
    let mut tests: Option<Vec<(PathBuf, String)>> = None;
    let mut out: Vec<PathBuf> = Vec::new();
    for c in changed {
        let f = &c.path;
        if !ext_is(f, &["py"]) {
            return Err(format!("`{}` changed and isn't Python", f.display()));
        }
        if is_test(f) {
            if root.join(f).exists() {
                out.push(f.clone());
            }
            continue;
        }
        let tests = tests.get_or_insert_with(|| {
            ignore::WalkBuilder::new(root)
                .hidden(true)
                .filter_entry(|e| !skip_dir(e.path()))
                .build()
                .flatten()
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()) && is_test(e.path()))
                .take(20_000)
                .filter_map(|e| {
                    let rel = e.path().strip_prefix(root).ok()?.to_path_buf();
                    let text = if e.metadata().ok()?.len() > 2_000_000 {
                        String::new()
                    } else {
                        std::fs::read_to_string(e.path()).unwrap_or_default()
                    };
                    Some((rel, text))
                })
                .collect()
        });
        let mut found = tests_for_module(f, tests);
        if found.is_empty() {
            // A new script nothing imports (a reproduction, a helper) can't
            // break existing tests; an existing module without tests can.
            if c.new {
                continue;
            }
            // Nothing imports the module directly (pytest's own `src/_pytest/…`
            // is the archetype): fall back to tests named after the package or
            // directory it lives in (`mark/evaluate.py` → `test_mark.py`), and to
            // tests importing an ancestor package whose `__init__` re-exports the
            // module, rather than giving up and running the whole suite.
            found = tests_named_after_module(f, tests);
            found.extend(tests_importing_reexporting_package(root, f, tests));
            found.sort();
            found.dedup();
            if found.is_empty() {
                return Err(format!("no tests found for `{}`", f.display()));
            }
        }
        out.extend(found);
    }
    out.sort();
    out.dedup();
    if out.len() > MAX_RELATED_FILES {
        return Err(format!("{} test files relate to the change", out.len()));
    }
    if out.is_empty() {
        return Err("no related tests found".into());
    }
    Ok(out)
}

/// Tests that are named after the module `f` or import it.
fn tests_for_module(f: &Path, tests: &[(PathBuf, String)]) -> Vec<PathBuf> {
    let (parts, package) = module_parts(f);
    // Dotted names it can be imported as: from the root, and without a source dir.
    let names = import_names(&parts);
    let res = import_regexes(&names, package);
    let module = parts.last().cloned().unwrap_or_default();
    let parent_dir = if parts.len() > 2 {
        Some(parts[parts.len() - 2].clone())
    } else {
        None
    };
    tests
        .iter()
        .filter(|(p, text)| {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            // test_<mod>.py, <mod>_test.py; for a nested module, under a matching directory.
            let named = !package
                && !module.is_empty()
                && (stem.ends_with(&format!("test_{module}"))
                    || stem == format!("{module}_test")
                    || stem == format!("{module}_tests"))
                && parent_dir
                    .as_ref()
                    .is_none_or(|d| p.components().any(|c| c.as_os_str() == d.as_str()));
            named || res.iter().any(|re| re.is_match(text))
        })
        .map(|(p, _)| p.clone())
        .collect()
}

/// The module's path parts and whether the last component is a package
/// (`__init__.py` names its directory).
fn module_parts(f: &Path) -> (Vec<String>, bool) {
    let parts: Vec<String> = f
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let package = parts.last().is_some_and(|p| p == "__init__");
    let parts = if package {
        parts[..parts.len() - 1].to_vec()
    } else {
        parts
    };
    (parts, package)
}

/// Dotted names a module can be imported as: from the root, and with a leading
/// `src`/`lib`/`python` directory dropped.
fn import_names(parts: &[String]) -> Vec<Vec<String>> {
    let mut names = Vec::new();
    for skip in 0..parts.len() {
        if skip > 0 && !matches!(parts[skip - 1].as_str(), "src" | "lib" | "python") {
            break;
        }
        let p = &parts[skip..];
        if !p.is_empty() && p.iter().all(|x| is_ident(x)) {
            names.push(p.to_vec());
        }
    }
    names
}

/// Regexes matching `import`/`from` of any of the dotted `names`.
fn import_regexes(names: &[Vec<String>], package: bool) -> Vec<regex::Regex> {
    let mut res = Vec::new();
    for n in names {
        let dotted = regex::escape(&n.join("."));
        // Importing a package's submodule runs the package's __init__ too.
        let tail = if package {
            r"(?:\s|\.|$|,)"
        } else {
            r"(?:\s|$|,|\.)"
        };
        let mut pat = format!(
            r"(?m)^\s*(?:from\s+{dotted}{tail}|import\s+(?:[\w.]+(?:\s+as\s+\w+)?\s*,\s*)*{dotted}{tail})"
        );
        if n.len() > 1 {
            let parent = regex::escape(&n[..n.len() - 1].join("."));
            let last = regex::escape(&n[n.len() - 1]);
            pat.push_str(&format!(r"|(?m)^\s*from\s+{parent}\s+import\s+(?:\([^)]*\b{last}\b[^)]*\)|[^\n(]*\b{last}\b)"));
        }
        if let Ok(re) = regex::Regex::new(&pat) {
            res.push(re);
        }
    }
    res
}

/// Tests named after the module's package or a directory it sits in, used when
/// no test imports the module by name. `src/_pytest/mark/evaluate.py` has no
/// direct importers but `testing/test_mark.py` covers it; without this the
/// done-gate falls back to the whole suite. A superset is fine (the point of
/// `related` scope is to skip the suite, not to prove a test can't be affected),
/// and the caller still caps the result at `MAX_RELATED_FILES`.
fn tests_named_after_module(f: &Path, tests: &[(PathBuf, String)]) -> Vec<PathBuf> {
    let words: Vec<String> = f
        .with_extension("")
        .components()
        .map(|c| {
            c.as_os_str()
                .to_string_lossy()
                .trim_start_matches('_')
                .to_ascii_lowercase()
        })
        .filter(|w| {
            w.len() >= 3 && !matches!(w.as_str(), "src" | "lib" | "python" | "test" | "tests")
        })
        .collect();
    if words.is_empty() {
        return Vec::new();
    }
    tests
        .iter()
        .filter(|(p, _)| {
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let tokens: Vec<&str> = stem.split(|c: char| !c.is_alphanumeric()).collect();
            words.iter().any(|w| tokens.contains(&w.as_str()))
        })
        .map(|(p, _)| p.clone())
        .collect()
}

/// Tests that import an ancestor package whose `__init__.py` re-exports the
/// module (`mark/__init__.py: from .evaluate import …`): importing the package
/// loads the module, so those tests exercise it. Only packages that actually
/// pull the module in count, so importing an unrelated package doesn't widen
/// the selection.
fn tests_importing_reexporting_package(
    root: &Path,
    f: &Path,
    tests: &[(PathBuf, String)],
) -> Vec<PathBuf> {
    let (parts, _) = module_parts(f);
    let Some(module) = parts.last().cloned() else {
        return Vec::new();
    };
    let Ok(reexports) = regex::Regex::new(&format!(
        r"(?m)^\s*(?:from|import)\b[^\n]*\b{}\b",
        regex::escape(&module)
    )) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for end in (1..parts.len()).rev() {
        let pkg = &parts[..end];
        let Ok(init) = std::fs::read_to_string(root.join(pkg.join("/")).join("__init__.py")) else {
            continue;
        };
        if !reexports.is_match(&init) {
            continue;
        }
        let res = import_regexes(&import_names(pkg), true);
        out.extend(
            tests
                .iter()
                .filter(|(_, text)| res.iter().any(|re| re.is_match(text)))
                .map(|(p, _)| p.clone()),
        );
    }
    out
}

/// A Cargo manifest's package name and the names of its dependencies.
fn cargo_package(manifest: &Path) -> Option<(String, Vec<String>)> {
    let t: toml::Table = std::fs::read_to_string(manifest).ok()?.parse().ok()?;
    let name = t.get("package")?.get("name")?.as_str()?.to_string();
    let deps = ["dependencies", "dev-dependencies", "build-dependencies"]
        .iter()
        .filter_map(|k| t.get(*k).and_then(|d| d.as_table()))
        .flat_map(|d| {
            d.iter().map(|(k, v)| {
                v.get("package")
                    .and_then(|p| p.as_str())
                    .unwrap_or(k)
                    .to_string()
            })
        })
        .collect();
    Some((name, deps))
}

/// Workspace crates holding the changed files, plus the crates that depend
/// on them. None: not a workspace, so `cargo test` is already that.
fn rust_related(root: &Path, changed: &[Changed]) -> Result<Option<Vec<String>>, String> {
    let root_manifest: toml::Table = std::fs::read_to_string(root.join("Cargo.toml"))
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or_default();
    if !root_manifest.contains_key("workspace") {
        return Ok(None);
    }
    let mut crates = std::collections::BTreeSet::new();
    for c in changed {
        if !ext_is(&c.path, &["rs"]) {
            return Err(format!("`{}` changed and isn't Rust", c.path.display()));
        }
        let mut dir = root.join(&c.path).parent().map(Path::to_path_buf);
        let mut found = None;
        while let Some(d) = dir {
            if let Some(p) = cargo_package(&d.join("Cargo.toml")) {
                found = Some(p.0);
                break;
            }
            if d == root || !d.starts_with(root) {
                break;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        crates.insert(found.ok_or_else(|| format!("no crate holds `{}`", c.path.display()))?);
    }
    // Crates in the workspace that depend on a changed one.
    let manifests = ignore::WalkBuilder::new(root)
        .max_depth(Some(6))
        .filter_entry(|e| !skip_dir(e.path()))
        .build()
        .flatten()
        .filter(|e| e.file_name() == "Cargo.toml")
        .filter_map(|e| cargo_package(e.path()))
        .collect::<Vec<_>>();
    let direct = crates.clone();
    for (name, deps) in manifests {
        if deps.iter().any(|d| direct.contains(d)) {
            crates.insert(name);
        }
    }
    Ok(Some(crates.into_iter().collect()))
}

/// Go package directories holding the changed files (with their
/// subpackages), plus the packages that import them. None: the root package
/// changed, so `./...` is already that.
fn go_related(root: &Path, changed: &[Changed]) -> Result<Option<Vec<String>>, String> {
    let module = std::fs::read_to_string(root.join("go.mod"))
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("module ")
                    .map(|m| m.trim().trim_matches('"').to_string())
            })
        });
    let mut dirs = std::collections::BTreeSet::new();
    for c in changed {
        if !ext_is(&c.path, &["go"]) {
            return Err(format!("`{}` changed and isn't Go", c.path.display()));
        }
        match c.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            Some(d) => dirs.insert(path_arg(d)),
            None => return Ok(None),
        };
    }
    let mut targets: std::collections::BTreeSet<String> =
        dirs.iter().map(|d| format!("./{d}/...")).collect();
    if let Some(module) = module {
        let imports: Vec<String> = dirs.iter().map(|d| format!("\"{module}/{d}\"")).collect();
        for e in ignore::WalkBuilder::new(root)
            .filter_entry(|e| !skip_dir(e.path()))
            .build()
            .flatten()
            .take(50_000)
        {
            if !e.file_type().is_some_and(|t| t.is_file()) || !ext_is(e.path(), &["go"]) {
                continue;
            }
            let text = std::fs::read_to_string(e.path()).unwrap_or_default();
            if imports.iter().any(|i| text.contains(i.as_str())) {
                match e
                    .path()
                    .parent()
                    .and_then(|d| d.strip_prefix(root).ok())
                    .filter(|d| !d.as_os_str().is_empty())
                {
                    Some(d) => targets.insert(format!("./{}", path_arg(d))),
                    None => return Ok(None),
                };
            }
        }
    }
    // Drop packages already inside another target's `/...`.
    let all: Vec<String> = targets.iter().cloned().collect();
    let kept: Vec<String> = all
        .iter()
        .filter(|t| {
            !all.iter().any(|o| {
                o != *t
                    && target_covers(
                        "go test",
                        o.trim_start_matches("./"),
                        t.trim_start_matches("./"),
                    )
            })
        })
        .cloned()
        .collect();
    if kept.len() > MAX_RELATED_FILES {
        return Err(format!("{} packages relate to the change", kept.len()));
    }
    Ok(Some(kept))
}

/// The related run named files or packages that don't exist (rather than
/// tests that fail): fall back to the full suite.
pub(super) fn selection_broken(output: &str, run: &Run) -> bool {
    let o = output.to_lowercase();
    ["error: file or directory not found", "error: not found:", "no tests found related", "package(s) not found", "did not match any packages",
     "package id specification", "matches no packages", "no go files in", "is not in std"]
        .iter()
        .any(|m| o.contains(m))
        // unittest couldn't import a selected module by its dotted name (not
        // an import error inside the code under test).
        || run.targets.iter().filter(|(_, f)| f.is_some()).any(|(t, _)| {
            let parts: Vec<&str> = t.split('.').collect();
            (1..=parts.len()).any(|n| o.contains(&format!("no module named '{}'", parts[..n].join(".")).to_lowercase()))
        })
}
