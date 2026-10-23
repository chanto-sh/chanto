# Tools on strange files. SETUP creates them (see weird_files_setup in the
# report); CASE picks the call.
import os

CASE = os.environ.get("CASE", "fifo")
CALLS = {
    "fifo": {"name": "Read", "args": {"file_path": "fifo"}},
    "devzero": {"name": "Read", "args": {"file_path": "/dev/zero"}},
    "devrandom": {"name": "Read", "args": {"file_path": "/dev/urandom"}},
    "dir": {"name": "Read", "args": {"file_path": "."}},
    "loop": {"name": "Read", "args": {"file_path": "loop1"}},
    "glob_loop": {"name": "Glob", "args": {"pattern": "**/*"}},
    "grep_loop": {"name": "Grep", "args": {"pattern": "hello"}},
    "ls_loop": {"name": "Ls", "args": {"path": "."}},
    "binary": {"name": "Read", "args": {"file_path": "bin.dat"}},
    "huge": {"name": "Read", "args": {"file_path": "huge.txt"}},
    "longline": {"name": "Read", "args": {"file_path": "longline.txt"}},
    "crlf_edit": {"name": "Edit", "args": {"file_path": "crlf.txt", "old_string": "two\nthree", "new_string": "TWO\nTHREE"}},
    "latin1_edit": {"name": "Edit", "args": {"file_path": "latin1.txt", "old_string": "caf", "new_string": "CAF"}},
    "write_fifo": {"name": "Write", "args": {"file_path": "fifo", "content": "x"}},
    "write_dir": {"name": "Write", "args": {"file_path": "subdir", "content": "x"}},
    "readonly_edit": {"name": "Edit", "args": {"file_path": "ro.txt", "old_string": "locked", "new_string": "changed"}},
    "offset_huge": {"name": "Read", "args": {"file_path": "a.txt", "offset": 18446744073709551615, "limit": 18446744073709551615}},
    "bash_fifo": {"name": "Bash", "args": {"command": "cat fifo"}},
}
NEEDS_READ = {"crlf_edit": "crlf.txt", "latin1_edit": "latin1.txt", "readonly_edit": "ro.txt"}


def respond(body, n):
    steps = ([{"name": "Read", "args": {"file_path": NEEDS_READ[CASE]}}] if CASE in NEEDS_READ else []) + [CALLS[CASE]]
    if n < len(steps):
        return {"tool_calls": [steps[n]]}
    return {"text": "done."}
