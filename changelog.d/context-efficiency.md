- **Leaner context.** Two new tools keep bulk work out of the prompt: `Index`
  returns a tree-sitter skeleton of a file (imports, type definitions and function
  signatures with line ranges) for Rust, Python, JavaScript, TypeScript and Go, with a
  regex outline for other languages, and `Exec` runs a short Python script in which
  `read`, `glob` and `grep` are functions, so only what the script prints enters the
  context. `Exec` is only offered when `python3` (or `python`) is on `PATH`.
- **Rare tools behind `ToolSearch`.** Browser, Computer, ChantoCloud, Monitor,
  NotebookEdit, RepoMap and SemanticSearch can be deferred from the prompt and loaded
  on demand with `optimize.defer_tools`.
