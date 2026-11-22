#!/bin/sh
# Keeps the post-merge CodeQL backlog visible and actionable: fails while
# CodeQL has an open alert on main, keeps one tracking issue current, and
# closes it once the backlog is empty. The "Require code scanning results"
# rule in "protect main" already stops a pull request that introduces an
# alert; this catches what only a scan after merge can find (a new query in a
# CodeQL action update, or the weekly full scan).
#
# Needs gh, GH_TOKEN (issues: write, security-events: read), and
# GITHUB_REPOSITORY. Used by codeql.yml right after the analysis and by
# code-scanning-watch.yml every six hours.
#
#   GITHUB_REPOSITORY=chanto-sh/chanto GH_TOKEN=... scripts/codeql-watch.sh
set -eu

TITLE="CodeQL alerts on main"

alerts=$(gh api "repos/$GITHUB_REPOSITORY/code-scanning/alerts?state=open&tool_name=CodeQL&ref=refs/heads/main&per_page=100" --paginate \
    --jq '.[] | "- #\(.number) \(.rule.security_severity_level // .rule.severity) \(.rule.id) \(.most_recent_instance.location.path):\(.most_recent_instance.location.start_line) \(.html_url)"')
number=$(gh issue list --state open --limit 200 --json number,title \
    --jq ".[] | select(.title == \"$TITLE\") | .number" | head -n 1)

if [ -n "$alerts" ]; then
    echo "$alerts"
    body="${RUNNER_TEMP:-/tmp}/codeql-watch-body.md"
    {
        echo "Open CodeQL alerts on main. The pull request rule in \`protect main\` stops new ones; these were found by a scan after merge (a new query or the weekly full scan). Fix or dismiss each, then this issue closes itself."
        echo
        echo "$alerts"
    } > "$body"
    if [ -n "$number" ]; then
        gh issue edit "$number" --body-file "$body"
    else
        gh issue create --title "$TITLE" --body-file "$body"
    fi
    echo "error: CodeQL has open alerts on main" >&2
    exit 1
fi

echo "No open CodeQL alerts on main."
if [ -n "$number" ]; then
    gh issue close "$number" --comment "No open CodeQL alerts on main."
fi
