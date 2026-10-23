# Text that is hard to render. One turn per prompt, in this order.
import os

BIG = int(os.environ.get("BIG", 2_000_000))
TURNS = [
    {"text": "emoji 😀👩‍👩‍👧🏳️‍🌈 wide 漢字かな 한국어 rtl שלום עולם مرحبا ‮evil‬ zw[​‍﻿] combining é́́́ zalgo Z̶̷̸̵̴̳ tab[\t] cr[\r] bell[\u0007] esc[\u001b[2J\u001b]0;pwned\u0007] nul[\u0000] del[\u007f] c1[\u0085\u009b31m]\n\n| a | b |\n|---|---|\n| 😀 | 漢字 |\n\n```\n\ttabbed code\u001b[31m red?\n```\n" + "😀" * 500},
    {"text": "x" * 200000, "chunk": 5000},
    {"text": ("line of markdown with **bold** and `code` and [link](http://x)\n" * (BIG // 60)), "chunk": 65536},
    {"text": "```rust\n" + "fn f() {}\n" * 50000 + "```\n", "chunk": 65536},
    {"text": "# " * 5000 + "\n" + "> " * 5000 + "\n" + "- " * 5000 + "\n" + "[" * 5000 + "\n" + "*" * 5000 + "\n" + "`" * 5000},
]
