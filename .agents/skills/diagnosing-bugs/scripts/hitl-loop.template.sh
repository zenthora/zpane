#!/usr/bin/env bash
# Human-in-the-loop reproduction loop for real-TTY / shell-integration bugs.
# Copy this file, edit the steps below, and run it.
# The agent runs the script; the user follows prompts in their terminal.
#
# Usage:
#   bash hitl-loop.template.sh
#
# Two helpers:
#   step "<instruction>"          -> show instruction, wait for Enter
#   capture VAR "<question>"      -> show question, read response into VAR
#
# At the end, captured values are printed as KEY=VALUE for the agent to parse.

set -euo pipefail

step() {
  printf '\n>>> %s\n' "$1"
  read -r -p "    [Enter when done] " _
}

capture() {
  local var="$1" question="$2" answer
  printf '\n>>> %s\n' "$question"
  read -r -p "    > " answer
  printf -v "$var" '%s' "$answer"
}

# --- edit below ---------------------------------------------------------

step "In your target shell/terminal, run: cargo run -- --initial \"git \""

capture VISUAL_OK "Press Right, type 'com', and press Enter. Did the emitted command match the preview? (y/n)"

capture OBSERVED "Paste the emitted command or observed terminal artifact (or 'none'):"

# --- edit above ---------------------------------------------------------

printf '\n--- Captured ---\n'
printf 'VISUAL_OK=%s\n' "$VISUAL_OK"
printf 'OBSERVED=%s\n' "$OBSERVED"
