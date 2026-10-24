const test = require("node:test");
const assert = require("node:assert");
const { mentionToken } = require("../out/mention");

// What `expand_mentions` (src/commands.rs) does to a prompt before resolving
// each token as a path.
function readBack(prompt) {
  return prompt
    .replaceAll("\\ ", "\u0001")
    .split(/\s+/)
    .filter(Boolean)
    .map((t) => t.replaceAll("\u0001", " "));
}

test("a mention reads back as the same path", () => {
  for (const rel of ["src/main.rs", "my docs/a b.md", "src\\win\\path.ts", "odd\\ name.txt", "a\\\\b"]) {
    const token = mentionToken(rel);
    assert.deepStrictEqual(readBack(`${token} explain this`), [`@${rel}`, "explain", "this"]);
  }
});

test("paths the prompt can't read back get no mention", () => {
  for (const rel of ["a\nb.md", "tab\there", "bell\u0007", "nbsp x", "trailing\\"]) {
    assert.strictEqual(mentionToken(rel), undefined, JSON.stringify(rel));
  }
});
