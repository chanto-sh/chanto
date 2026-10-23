//! Durable facts outside the checkout. Indexes contain summaries, never bodies.
use crate::util::Canonical as _;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const INDEX_BUDGET: usize = 4096;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub auto: bool,
    pub scope: String,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            auto: true,
            scope: "project".into(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub created: String,
    pub updated: String,
    #[serde(skip)]
    pub body: String,
}
fn project_key(root: &Path) -> String {
    let full = root
        .canonical()
        .unwrap_or_else(|_| root.to_path_buf())
        .display()
        .to_string();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in full.bytes() {
        hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("project-{hash:016x}")
}

/// Called before adding the first user message; later turns retain that context.
pub fn first_turn_context(
    store: &Store,
    messages: &[crate::llm::Message],
) -> Result<Option<String>> {
    if messages.iter().any(|m| m.role == crate::llm::Role::User) {
        return Ok(None);
    }
    let index = store.context()?;
    Ok((!index.is_empty()).then_some(index))
}

pub struct Store {
    pub home: PathBuf,
    pub root: PathBuf,
}
impl Store {
    pub fn new(root: &Path) -> Self {
        Self {
            home: crate::config::home_dir(),
            root: root.into(),
        }
    }
    pub fn dir(&self, scope: &str) -> Result<PathBuf> {
        match scope {
            "user" | "global" => Ok(self.home.join("memory")),
            "project" => Ok(self
                .home
                .join("projects")
                .join(project_key(&self.root))
                .join("memory")),
            _ => bail!("scope must be user or project"),
        }
    }
    /// Memories are addressed by name; a trailing `.md` is optional and never
    /// part of the key, so `foo` and `foo.md` are the same memory.
    fn clean(name: &str) -> &str {
        name.strip_suffix(".md").unwrap_or(name)
    }
    fn path(&self, scope: &str, name: &str) -> Result<PathBuf> {
        let name = Self::clean(name);
        if name.is_empty()
            || name == "MEMORY"
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("name must contain only letters, digits, - or _ (except MEMORY)");
        }
        Ok(self.dir(scope)?.join(format!("{name}.md")))
    }
    pub fn read(&self, scope: &str, name: &str) -> Result<Memory> {
        let text = std::fs::read_to_string(self.path(scope, name)?)?;
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| anyhow::anyhow!("missing frontmatter"))?;
        let (meta, body) = rest
            .split_once("\n---\n")
            .ok_or_else(|| anyhow::anyhow!("missing body"))?;
        // JSON string values are valid YAML scalars, without a YAML dependency.
        let mut map = serde_json::Map::new();
        for line in meta.lines() {
            let (k, v) = line
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("invalid frontmatter"))?;
            map.insert(
                k.into(),
                serde_json::from_str(v.trim())
                    .unwrap_or_else(|_| serde_json::Value::String(v.trim().into())),
            );
        }
        let mut m: Memory = serde_json::from_value(map.into())?;
        m.body = body.into();
        Ok(m)
    }
    pub fn list(&self, scope: &str) -> Result<Vec<Memory>> {
        let mut out = Vec::new();
        let dir = self.dir(scope)?;
        if !dir.exists() {
            return Ok(out);
        }
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.extension().is_some_and(|e| e == "md")
                && let Some(name) = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|s| *s != "MEMORY")
            {
                out.push(self.read(scope, name)?);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
    pub fn regenerate(&self, scope: &str) -> Result<String> {
        let text: String = self
            .list(scope)?
            .iter()
            .map(|m| {
                format!(
                    "- {}.md: {} [{}]\n",
                    m.name,
                    m.description.replace(['\n', '\r'], " "),
                    m.kind
                )
            })
            .collect();
        let d = self.dir(scope)?;
        if d.exists() {
            crate::util::write_private(&d.join("MEMORY.md"), text.as_bytes())?;
        }
        Ok(text)
    }
    fn remember(&self, scope: &str, name: &str) -> Result<()> {
        let p = self.path(scope, name)?;
        let d = self.dir(scope)?;
        std::fs::create_dir_all(&d)?;
        let entry = serde_json::json!({"name":name,"previous":std::fs::read_to_string(p).ok(),"ts":chrono::Utc::now().to_rfc3339()});
        use std::io::Write;
        writeln!(
            crate::util::append_private(&d.join("history.jsonl"))?,
            "{entry}"
        )?;
        Ok(())
    }
    pub fn save(
        &self,
        scope: &str,
        name: &str,
        description: &str,
        kind: &str,
        body: &str,
    ) -> Result<()> {
        let name = Self::clean(name);
        let p = self.path(scope, name)?;
        if !["user", "feedback", "project", "reference"].contains(&kind) {
            bail!("invalid memory type");
        }
        if body.trim().is_empty() || description.trim().is_empty() {
            bail!("body and description must not be empty");
        }
        if crate::secrets::contains_secret(&format!("{name}\n{description}\n{body}")) {
            bail!("refusing to store a secret");
        }
        let now = chrono::Utc::now().to_rfc3339();
        let created = self
            .read(scope, name)
            .map(|m| m.created)
            .unwrap_or_else(|_| now.clone());
        let m = Memory {
            name: name.into(),
            description: description.into(),
            kind: kind.into(),
            created,
            updated: now,
            body: body.into(),
        };
        let fields = serde_json::to_value(&m)?;
        let mut text = "---\n".to_string();
        for key in ["name", "description", "type", "created", "updated"] {
            text.push_str(&format!("{key}: {}\n", fields[key]));
        }
        text.push_str(&format!("---\n{body}"));
        self.remember(scope, name)?;
        crate::util::write_private(&p, text.as_bytes())?;
        self.regenerate(scope)?;
        Ok(())
    }
    pub fn delete(&self, scope: &str, name: &str) -> Result<()> {
        let p = self.path(scope, name)?;
        if !p.exists() {
            bail!("memory not found");
        }
        self.remember(scope, name)?;
        std::fs::remove_file(p)?;
        self.regenerate(scope)?;
        Ok(())
    }
    pub fn undo(&self, scope: &str) -> Result<()> {
        let d = self.dir(scope)?;
        let text = std::fs::read_to_string(d.join("history.jsonl"))?;
        let mut lines: Vec<_> = text.lines().collect();
        let v: serde_json::Value = serde_json::from_str(
            lines
                .pop()
                .ok_or_else(|| anyhow::anyhow!("nothing to undo"))?,
        )?;
        let p = self.path(scope, v["name"].as_str().unwrap_or(""))?;
        if let Some(prev) = v["previous"].as_str() {
            crate::util::write_private(&p, prev.as_bytes())?;
        } else if p.exists() {
            std::fs::remove_file(p)?;
        }
        crate::util::write_private(
            &d.join("history.jsonl"),
            (lines.join("\n") + if lines.is_empty() { "" } else { "\n" }).as_bytes(),
        )?;
        self.regenerate(scope)?;
        Ok(())
    }
    pub fn context(&self) -> Result<String> {
        let mut entries = String::new();
        for scope in ["user", "project"] {
            for line in self.regenerate(scope)?.lines() {
                let line = format!("{scope} {line}\n");
                if entries.len() + line.len() > INDEX_BUDGET {
                    continue;
                }
                entries.push_str(&line);
            }
        }
        // No memories yet: the reminder would be an empty heading on every
        // first turn, so send nothing.
        if entries.is_empty() {
            return Ok(String::new());
        }
        Ok(format!(
            "Memory index (summaries only; use Memory read when relevant):\n{entries}"
        ))
    }
}

const MEMORY_HELP: &str = "Usage: chanto memory [list | show <name> | edit <name> <text> | delete <name> | undo | forget all] [--user]\n       chanto memory <note>        Save a note for later sessions\n\n\
Durable memory the agent keeps across sessions: `user` (all projects) and `project` (this one). \
`--user` picks the user scope; the default comes from `[memory] scope`.";

pub fn command(root: &Path, input: &str, default_scope: &str) -> Result<String> {
    let store = Store::new(root);
    let mut parts = input.splitn(2, ' ');
    let action = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    let (scope, rest) = if let Some(r) = rest.strip_prefix("--user") {
        ("user", r.trim())
    } else {
        (default_scope, rest)
    };
    match action {
        "" | "list" => {
            let mut out = String::new();
            for scope in ["user", "project"] {
                let index = store.regenerate(scope)?;
                let index = if index.trim().is_empty() {
                    "  (none yet)\n".to_string()
                } else {
                    index
                };
                out.push_str(&format!("{scope}:\n{index}"));
            }
            Ok(out)
        }
        "help" | "--help" | "-h" => Ok(MEMORY_HELP.into()),
        "open" | "show" => Ok(store.read(scope, rest)?.body),
        "delete" | "rm" => {
            store.delete(scope, rest)?;
            Ok(format!(
                "Deleted memory: {rest} (undo: /memory undo {scope})"
            ))
        }
        "undo" => {
            store.undo(if rest.is_empty() { scope } else { rest })?;
            Ok("Memory change undone".into())
        }
        "forget" if rest == "all" => {
            for scope in ["user", "project"] {
                for m in store.list(scope)? {
                    store.delete(scope, &m.name)?;
                }
            }
            Ok("Forgot all memories; /memory undo restores one change at a time".into())
        }
        "edit" => {
            let (name, body) = rest
                .split_once(' ')
                .ok_or_else(|| anyhow::anyhow!("Usage: /memory edit <name> <text>"))?;
            let m = store.read(scope, name)?;
            store.save(scope, name, &m.description, &m.kind, body)?;
            Ok(format!("Saved memory: {name}"))
        }
        _ => {
            let name = format!("note-{}", uuid::Uuid::new_v4().simple());
            store.save(
                scope,
                &name,
                &crate::util::truncate(input, 120),
                "project",
                input,
            )?;
            Ok(format!("Saved memory: {name} (undo: /memory undo {scope})"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_memory_store_adds_no_reminder() {
        let home = std::env::temp_dir().join(format!("chanto-memory-{}", uuid::Uuid::new_v4()));
        let store = Store {
            root: home.join("project"),
            home: home.clone(),
        };
        assert_eq!(store.context().unwrap(), "");
        assert!(first_turn_context(&store, &[]).unwrap().is_none());
        store
            .save("user", "style", "Style", "reference", "prefers tabs")
            .unwrap();
        let index = first_turn_context(&store, &[]).unwrap().unwrap();
        assert!(index.starts_with("Memory index"));
        assert!(index.contains("user - style.md:"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn md_suffix_is_optional() {
        let home = std::env::temp_dir().join(format!("chanto-memory-{}", uuid::Uuid::new_v4()));
        let store = Store {
            root: home.join("project"),
            home: home.clone(),
        };
        store
            .save(
                "project",
                "deploy.md",
                "Deploy detail",
                "reference",
                "staging job",
            )
            .unwrap();
        assert_eq!(store.read("project", "deploy").unwrap().body, "staging job");
        assert_eq!(
            store.read("project", "deploy.md").unwrap().body,
            "staging job"
        );
        assert!(
            store
                .regenerate("project")
                .unwrap()
                .contains("- deploy.md:")
        );
        store.delete("project", "deploy.md").unwrap();
        assert!(store.read("project", "deploy").is_err());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn lifecycle_scopes_secrets_and_budget() {
        let home = std::env::temp_dir().join(format!("chanto-memory-{}", uuid::Uuid::new_v4()));
        let store = Store {
            root: home.join("project"),
            home: home.clone(),
        };
        store
            .save(
                "user",
                "style",
                "Writing style",
                "user",
                "Keep prose concise",
            )
            .unwrap();
        store
            .save(
                "project",
                "deploy",
                "Deployment detail",
                "reference",
                "Deploys require the staging job",
            )
            .unwrap();
        assert!(store.dir("project").unwrap().starts_with(&home));
        let other = Store {
            root: home.join("other"),
            home: home.clone(),
        };
        assert!(other.read("project", "deploy").is_err());
        assert!(other.read("user", "style").is_ok());
        assert!(first_turn_context(&store, &[]).unwrap().is_some());
        assert!(
            first_turn_context(&store, &[crate::llm::Message::user("task")])
                .unwrap()
                .is_none()
        );
        assert!(store.read("project", "style").is_err());
        assert!(store.read("user", "deploy").is_err());
        let created = store.read("user", "style").unwrap().created;
        store
            .save(
                "user",
                "style",
                "Writing style",
                "user",
                "Use short paragraphs",
            )
            .unwrap();
        assert_eq!(store.read("user", "style").unwrap().created, created);
        store.undo("user").unwrap();
        assert_eq!(
            store.read("user", "style").unwrap().body,
            "Keep prose concise"
        );
        store.delete("user", "style").unwrap();
        assert!(!store.regenerate("user").unwrap().contains("style"));
        store.undo("user").unwrap();
        assert!(store.regenerate("user").unwrap().contains("style.md"));
        assert!(
            store
                .save(
                    "project",
                    "bad",
                    "Credential",
                    "project",
                    "AKIAIOSFODNN7EXAMPLE"
                )
                .is_err()
        );
        assert!(
            store
                .save("project", "../escape", "Bad", "project", "text")
                .is_err()
        );
        store
            .save("project", "huge", &"界".repeat(5000), "project", "body")
            .unwrap();
        let context = store.context().unwrap();
        assert!(context.len() <= INDEX_BUDGET);
        assert!(
            !context.contains("Deploys require"),
            "index must not include bodies"
        );
        std::fs::remove_file(store.dir("project").unwrap().join("MEMORY.md")).unwrap();
        assert!(store.regenerate("project").unwrap().contains("deploy.md"));
        std::fs::remove_dir_all(home).unwrap();
    }
}
