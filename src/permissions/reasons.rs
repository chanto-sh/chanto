use super::*;

/// Explain a static permission prompt without changing its decision.
pub(crate) fn ask_reason(
    mode: Mode,
    kind: ToolKind,
    tool: &str,
    args: &Value,
    cwd: &Path,
) -> String {
    if let Some(path) = secret_file_target(kind, tool, args, cwd) {
        return format!("This action accesses a protected secret file: {path}.");
    }
    if matches!(tool, "WebFetch" | "Browser")
        && args["url"]
            .as_str()
            .is_some_and(|url| url_needs_approval(url, cwd))
    {
        return "This URL may access local files, private metadata, or a restricted scheme.".into();
    }
    if kind == ToolKind::Execute {
        if crate::tools::bash::sandbox_on()
            && args["dangerouslyDisableSandbox"].as_bool() == Some(true)
            && crate::runs::container::for_path(cwd).is_none()
        {
            return "This command requests execution outside the sandbox.".into();
        }
        let writes = shell_writes::scan(args["command"].as_str().unwrap_or(""), cwd, posix_shell());
        if let Some(path) = writes.protected {
            return format!("This command writes a protected or shell-computed path: {path}.");
        }
        if mode == Mode::Auto && writes.unseen {
            return "This command applies a patch or extracts an archive whose destination paths cannot be checked from the command. Use the file-editing tools for inspectable patches.".into();
        }
    }
    if tool == "Memory" && kind == ToolKind::Edit && args["scope"] != "project" {
        return "This memory change affects other projects and future sessions.".into();
    }
    if kind == ToolKind::Edit {
        let paths = if tool == "apply_patch" {
            crate::tools::patch::paths(
                args["input"]
                    .as_str()
                    .or(args["patch"].as_str())
                    .unwrap_or(""),
            )
        } else {
            ["file_path", "notebook_path", "path"]
                .iter()
                .find_map(|key| args[*key].as_str())
                .map(|path| vec![path.to_string()])
                .unwrap_or_default()
        };
        if let Some(path) = paths
            .iter()
            .find(|path| is_protected_path(&crate::util::resolve(cwd, path)))
        {
            return format!(
                "This edit changes protected settings or executable configuration: {path}."
            );
        }
        if paths.is_empty()
            || paths
                .iter()
                .any(|path| path.is_empty() || !in_workspace(Path::new(path), cwd))
        {
            return "This edit targets a path outside the allowed workspace, or its destination could not be determined. Add a trusted directory with --add-dir if it belongs to this task.".into();
        }
        return "This permission mode requires approval for file edits.".into();
    }
    if tool == "Computer" {
        return "This permission mode requires approval for actions on your desktop.".into();
    }
    format!(
        "{} requires approval for this action unless a permission rule allows it.",
        mode.label()
    )
}
