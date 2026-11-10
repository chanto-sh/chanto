//! `chanto github install`: write the GitHub Action workflows (and a review
//! policy) into a repository without overwriting anything the user has.

use anyhow::{Context, Result, bail};
use std::path::Path;

const WORKFLOWS: [(&str, &str); 3] = [
    (
        ".github/workflows/chanto-review.yml",
        include_str!("../integrations/github/review.yml"),
    ),
    (
        ".github/workflows/chanto.yml",
        include_str!("../integrations/github/chanto.yml"),
    ),
    (
        ".github/workflows/chanto-quality.yml",
        include_str!("../integrations/github/quality.yml"),
    ),
];
const POLICY: (&str, &str) = (
    ".chanto/review.toml",
    include_str!("../integrations/github/review.toml"),
);

fn create_new(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

pub fn install(root: &Path) -> Result<String> {
    for (name, _) in &WORKFLOWS {
        if root.join(name).exists() {
            bail!("{name} already exists; refusing to overwrite it");
        }
    }
    let mut out = String::new();
    for (name, content) in WORKFLOWS {
        create_new(&root.join(name), content)?;
        out.push_str(&format!("Created {name}\n"));
    }
    // The policy is optional and often already customized: keep an existing one.
    if root.join(POLICY.0).exists() {
        out.push_str(&format!("Kept existing {}\n", POLICY.0));
    } else {
        create_new(&root.join(POLICY.0), POLICY.1)?;
        out.push_str(&format!(
            "Created {} (strictness, inline comment cap, ignore paths, checks)\n",
            POLICY.0
        ));
    }
    out.push_str(
        "\nAdd one repository secret (Settings → Secrets and variables → Actions):\n\
         \x20 ANTHROPIC_API_KEY   used by the workflows as written\n\
         \x20 OPENAI_API_KEY      instead, if you switch the workflows to openai_api_key and an OpenAI model\n\
         \x20 CHANTO_PROVIDERS   optional: [providers.<name>] TOML passed as provider_config, for other providers\n\
         \n\
         Next:\n\
         \x20 - Commit these files to the default branch; review policy is read from each PR's base commit.\n\
         \x20 - Pin chanto-sh/chanto/action@main to a reviewed commit.\n\
         \x20 - Set `checks` in .chanto/review.toml if your project checks aren't detected; fixes need passing checks.\n\
         \x20 - chanto-quality.yml needs no secret: run `chanto prime` and commit .chanto/quality.toml, and it checks every PR against it.\n\
         \x20 - Fork PRs are not reviewed (no secrets on `pull_request` from forks); see docs/reference.md, \"GitHub review bot\".",
    );
    Ok(out)
}

#[cfg(test)]
mod tests;
