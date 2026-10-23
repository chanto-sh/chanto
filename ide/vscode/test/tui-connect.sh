#!/bin/sh
# Start the Chanto TUI in tmux against an editor's lockfiles and show /ide.
# Usage: test/tui-connect.sh <chanto-binary> <workspace> <chanto-home>
bin=$1; ws=$2; home=$3
tmux kill-session -t ide-probe 2>/dev/null
tmux new-session -d -s ide-probe -x 110 -y 30 "cd $ws && CHANTO_HOME=$home $bin --trust; sleep 5"
sleep 3
tmux send-keys -t ide-probe '/ide' Enter
sleep 1
tmux capture-pane -p -t ide-probe | grep -v '^\s*$' | tail -14
tmux kill-session -t ide-probe
