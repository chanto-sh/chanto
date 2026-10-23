# Answers taller than the terminal, to check inline-mode scrollback.
TURNS = [
    {"text": "".join(f"row {i:03d}\n" for i in range(1, 61)), "chunk": 9, "drip_ms": 10},
    {"text": "".join(f"row {i:03d} " + "😀" * 30 + "\n" for i in range(1, 31)), "chunk": 50, "drip_ms": 10},
    {"text": "start " + "😀" * 500 + " end", "chunk": 50, "drip_ms": 10},
]
