#!/bin/bash
set -x
"${RUSH_BIN:?Set RUSH_BIN to the Rush executable}" clean
ls -la rush-out/ 2>&1 || echo "rush-out doesn't exist"
"${RUSH_BIN:?Set RUSH_BIN to the Rush executable}" build //:hello &
PID=$!
sleep 3
kill -9 $PID 2>/dev/null
wait $PID 2>/dev/null
ls -la rush-out/.build-state.json 2>&1 || echo "state file doesn't exist"
