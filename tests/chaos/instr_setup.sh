#!/usr/bin/env bash
# Run in a (trusted) project dir: other tools' instruction files at their
# worst. Expect: nothing from outside the project, a bounded system prompt.
echo "OUTSIDE-SECRET-$$" > ../secret.txt
mkdir -p docs .cursor/rules .github/instructions .opencode/agent
for i in $(seq 400); do python3 -c "print('doc $i ' + 'filler ' * 1500)" > "docs/d$i.md"; done
printf '{"instructions": ["**/*.md", "../secret.txt", "/etc/hosts", "docs/../../secret.txt"]}\n' > opencode.json
printf 'read: [/etc/hosts, ../secret.txt, CONVENTIONS.md]\n' > .aider.conf.yml
echo "aider conventions" > CONVENTIONS.md
printf -- '---\nalwaysApply: true\nglobs: [\n---\ncursor rule with broken frontmatter\n' > .cursor/rules/broken.mdc
printf -- '---\napplyTo: "**"\n---\ncopilot always\n' > .github/instructions/all.instructions.md
python3 -c "print('G' * 3000000)" > GEMINI.md
[ -n "${NOLINK:-}" ] || ln -s ../secret.txt .cursorrules
