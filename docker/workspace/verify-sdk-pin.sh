#!/usr/bin/env bash
set -uo pipefail

install_dir="${1:-/opt/corcode-agent}"
readonly package='@anthropic-ai/claude-agent-sdk'

cd "$install_dir" || exit 1

pinned="$(npm pkg get "overrides.$package" | tr -d '"')"
tree="$(npm ls "$package" --all --json 2>/dev/null)"
installed="$(jq -r --arg package "$package" '
  [.. | objects | (.dependencies? // {}) | to_entries[]
   | select(.key == $package) | .value.version]
  | unique | join(" ")
' <<<"$tree")"

if [ "$installed" != "$pinned" ]; then
  printf 'the adapter does not load the pinned Claude Agent SDK: pinned %s, installed [%s]\n' \
    "$pinned" "$installed" >&2
  exit 1
fi

printf 'the adapter loads the pinned Claude Agent SDK %s\n' "$pinned"
