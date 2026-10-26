#!/usr/bin/env python3
"""Render captured UI frames with pinned VHS and build a local preview gallery."""
import argparse
import html
import json
import pathlib
import re
import shlex

from show import caption
import subprocess


def regex(text):
    return re.sub(r"([\\.^$|?*+()\[\]{}/])", r"\\\1", text)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--frames", type=pathlib.Path, default=pathlib.Path("target/visual"))
    parser.add_argument("--generate-only", action="store_true", help="write tape files without launching VHS")
    args = parser.parse_args()
    root = args.frames.resolve()
    captures = sorted(root.glob("*.ansi"))
    if not captures:
        raise SystemExit("No frames found. Run the visual_snapshots test with CHANTO_VISUAL_OUT first.")
    groups = {}
    for capture in captures:
        theme, size, _ = capture.stem.split("_", 2)
        groups.setdefault((theme, size), []).append(capture)
    for (theme, size), frames in sorted(groups.items()):
        cols, rows = size.split("x")
        # Defaults are explicit: application RGB styles come from the captured cells.
        colors = {"name": "fixture", "background": "#111111" if theme == "dark" else "#ffffff", "foreground": "#eeeeee" if theme == "dark" else "#111111", "cursor": "#888888", "black": "#000000", "red": "#800000", "green": "#008000", "yellow": "#808000", "blue": "#000080", "magenta": "#800080", "cyan": "#008080", "white": "#c0c0c0", "brightBlack": "#808080", "brightRed": "#ff0000", "brightGreen": "#00ff00", "brightYellow": "#ffff00", "brightBlue": "#0000ff", "brightMagenta": "#ff00ff", "brightCyan": "#00ffff", "brightWhite": "#ffffff", "selection": "#666666"}
        tape = [f'Output {json.dumps(str(root / f"{theme}_{size}.gif"))}', 'Require python3', 'Set Shell "bash"', 'Set FontFamily "JetBrains Mono"', 'Set FontSize 16', f'Set Columns {cols}', f'Set Rows {int(rows) + 1}', 'Set Padding 12', 'Set Framerate 5', 'Set TypingSpeed 0', 'Set Theme ' + json.dumps(colors), 'Env PS1 ""']
        for capture in frames:
            ready = caption(capture)
            command = shlex.join(["python3", "tests/visual/show.py", str(capture)])
            tape += ['Hide', 'Type ' + json.dumps(command), 'Enter', 'Wait+Screen /' + regex(ready) + '/', 'Show', 'Sleep 200ms', 'Screenshot ' + json.dumps(str(capture.with_suffix(".png"))), 'Sleep 200ms', 'Hide', 'Enter', 'Sleep 100ms']
        path = root / f"{theme}_{size}.tape"
        path.write_text("\n".join(tape) + "\n")
        if not args.generate_only:
            subprocess.run(["vhs", str(path)], check=True)
    if args.generate_only:
        print(f"Generated {len(groups)} VHS tapes for {len(captures)} frames")
        return
    cards = []
    for capture in captures:
        name = html.escape(capture.stem)
        cards.append(f'<article><h2>{name}</h2><a href="{name}.png"><img loading="lazy" src="{name}.png" alt="{name}"></a><p><a href="{name}.txt">Text and styles</a></p></article>')
    (root / "index.html").write_text('<!doctype html><meta charset="utf-8"><title>Chanto visual fixtures</title><style>body{font:16px system-ui;background:#eee;color:#222;margin:24px}article{margin-bottom:32px}img{max-width:100%;height:auto}h2{font-size:16px}</style><h1>Chanto UI previews</h1><p>Rendered from the same captured cells checked by the snapshot suite. These previews do not exercise a live terminal session or model.</p>' + "".join(cards))
    print(f"Preview gallery: {root / 'index.html'}")


if __name__ == "__main__":
    main()
