// The `@path` token "Insert @-mention" types into the chanto prompt.

// The prompt reads mentions back in `expand_mentions` (src/commands.rs): it
// splits on whitespace, and its one escape is `\ ` for a space inside a path.
// A backslash before anything else is literal (Windows paths are full of
// them), so backslashes are not escaped here, and escaping them would change
// the path. That leaves three things this form can't carry: whitespace other
// than a space and control characters (a newline would even submit the
// prompt), and a trailing backslash, which with the space typed after the
// token would read as an escaped space. Such paths get no token.
export function mentionToken(rel: string): string | undefined {
  if (/[\p{Cc}\s]/u.test(rel.replaceAll(" ", "")) || rel.endsWith("\\")) return undefined;
  return "@" + rel.replaceAll(" ", "\\ ");
}
