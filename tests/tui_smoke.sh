#!/bin/bash
# Drive the TUI in tmux with a real model and print screens at key points.
# Usage: tests/tui_smoke.sh [model]
set -u
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/chanto"
MODEL="${1:-qwen3.8-bench}"
W=$(mktemp -d /tmp/chanto-tui-XXXX)
cd "$W" && git init -q && printf 'def greet(name):\n    pass\n' > hello.py && git add -A && git -c user.email=a@b -c user.name=t commit -qm init
tmux kill-session -t hx 2>/dev/null
tmux new-session -d -s hx -x 120 -y 45 "cd $W && $BIN --model $MODEL --mode plan"
sleep 2
snap() { echo "===== $1"; tmux capture-pane -t hx -p | sed -e 's/[[:space:]]*$//' | grep -v '^$' | tail -${2:-30}; }
wait_idle() { # wait until the spinner disappears
  for i in $(seq 1 ${1:-120}); do
    sleep 3
    if ! tmux capture-pane -t hx -p | grep -q "esc to interrupt"; then
      if tmux capture-pane -t hx -p | grep -qE "Allow this action|Question|Select"; then return 0; fi
      sleep 2; tmux capture-pane -t hx -p | grep -q "esc to interrupt" || return 0
    fi
  done
}
# 1. Plan mode -> ExitPlanMode approval ("Proceed in auto mode") -> implementation
tmux send-keys -t hx "implement greet in hello.py so it returns 'Hello, <name>!' and verify it with python" Enter
wait_idle 200; snap "plan mode result" 40
tmux send-keys -t hx "2"; wait_idle 200; snap "after approving plan" 40
# 2. Explore subagent
tmux send-keys -t hx "use an Explore subagent to list every python function in this repo and report them" Enter
wait_idle 200; snap "subagent" 30
# 3. Web search
tmux send-keys -t hx "search the web: what is the latest stable version of the ratatui crate? answer in one line" Enter
wait_idle 200; snap "web search" 25
# 4. Browser + screenshot
tmux send-keys -t hx "open https://example.com in the browser, take a screenshot and tell me what the heading says" Enter
wait_idle 200; snap "browser" 15
cat "$W/hello.py"
tmux kill-session -t hx
