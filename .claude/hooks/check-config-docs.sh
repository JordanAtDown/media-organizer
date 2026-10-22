#!/usr/bin/env bash
# PreToolUse hook (Bash): before a `git commit`, warn when src/config.rs is staged
# without config.example.toml — the documentation drift that rotted the old repo.
# It asks for confirmation rather than blocking: unlike a leaked secret, stale
# documentation is not a reason to refuse a commit outright.
set -euo pipefail

command -v jq >/dev/null 2>&1 || exit 0
cmd="$(jq -r '.tool_input.command // ""')"
[[ "$cmd" =~ git[[:space:]]+commit ]] || exit 0

staged="$(git diff --cached --name-only 2>/dev/null || true)"
grep -qx 'src/config.rs' <<<"$staged" || exit 0
grep -qx 'config.example.toml' <<<"$staged" && exit 0

jq -n '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "ask",
    permissionDecisionReason: "src/config.rs is staged but config.example.toml is not. Every config option must appear in config.example.toml (and be covered by a validation test). Commit anyway?"
  }
}'
