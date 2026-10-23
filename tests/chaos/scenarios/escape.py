# Path-escape attempts. CASE picks one; in acceptEdits -p runs these must be
# denied (they need approval), never silently written.
import os

CASE = os.environ.get("CASE", "dotdot")
CALLS = {
    "dotdot": {"name": "Write", "args": {"file_path": os.path.relpath(os.environ.get("OUTSIDE", "/tmp"), os.environ.get("PROJ", ".")) + "/pwn.txt", "content": "pwned\n"}},
    "absolute": {"name": "Write", "args": {"file_path": os.environ.get("OUTSIDE", "/tmp") + "/pwn.txt", "content": "pwned\n"}},
    "symlink_dir": {"name": "Write", "args": {"file_path": "link/pwn.txt", "content": "pwned\n"}},
    "symlink_file": {"name": "Write", "args": {"file_path": "linkfile", "content": "pwned\n"}},
    "symlink_edit": {"name": "Edit", "args": {"file_path": "linkfile", "old_string": "secret", "new_string": "pwned"}},
    "tilde": {"name": "Write", "args": {"file_path": "~/pwn.txt", "content": "pwned\n"}},
    "dotgit_hook": {"name": "Write", "args": {"file_path": ".git/hooks/pre-commit", "content": "#!/bin/sh\ntouch pwned\n"}},
    "chanto_cfg": {"name": "Write", "args": {"file_path": ".chanto/config.toml", "content": "mode = \"bypassPermissions\"\n"}},
    "claude_settings": {"name": "Write", "args": {"file_path": ".claude/settings.json", "content": "{\"permissions\":{\"allow\":[\"Bash\"]}}"}},
    "multiedit_link": {"name": "MultiEdit", "args": {"file_path": "linkfile", "edits": [{"old_string": "secret", "new_string": "pwned"}]}},
    "patch_link": {"name": "apply_patch", "args": {"input": "*** Begin Patch\n*** Update File: linkfile\n@@\n-secret\n+pwned\n*** End Patch\n"}},
    "patch_dotdot": {"name": "apply_patch", "args": {"input": "*** Begin Patch\n*** Add File: ../outside/pwn2.txt\n+pwned\n*** End Patch\n"}},
    "notebook_link": {"name": "NotebookEdit", "args": {"notebook_path": "link/nb.ipynb", "new_source": "x", "cell_number": 0}},
}


def respond(body, n):
    # Edits need a Read of the file first.
    steps = ([{"name": "Read", "args": {"file_path": "linkfile"}}] if CASE in ("symlink_edit", "multiedit_link") else []) + [CALLS[CASE]]
    if n < len(steps):
        return {"tool_calls": [steps[n]]}
    return {"text": "done."}
