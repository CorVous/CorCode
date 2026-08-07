#!/usr/bin/env bash
set -uo pipefail

install_dir="${1:-/opt/corcode-agent}"
readonly adapter='@agentclientprotocol/claude-agent-acp'
readonly sdk='@anthropic-ai/claude-agent-sdk'
readonly bin='claude-agent-acp'

cd "$install_dir" || exit 1

# Every copy of $2 the install ended up with must be the version pinned at
# npm-pkg path $1 — one stale copy deeper in the tree is one the adapter can
# load instead.
verify() {
  local path="$1" package="$2" pinned installed
  pinned="$(npm pkg get "$path" | tr -d '"')"
  installed="$(npm ls "$package" --all --json 2>/dev/null | jq -r --arg package "$package" '
    [.. | objects | (.dependencies? // {}) | to_entries[]
     | select(.key == $package) | .value.version]
    | unique | join(" ")
  ')"

  if [ "$installed" != "$pinned" ]; then
    printf 'the image does not install the pinned %s: pinned %s, installed [%s]\n' \
      "$package" "$pinned" "$installed" >&2
    return 1
  fi

  printf 'the image installs the pinned %s %s\n' "$package" "$pinned"
}

verify "dependencies.$adapter" "$adapter" || exit 1
verify "overrides.$sdk" "$sdk" || exit 1

if [ ! -x "node_modules/.bin/$bin" ]; then
  printf 'the adapter installs no %s for the core to exec; node_modules/.bin holds [%s]\n' \
    "$bin" "$(echo node_modules/.bin/*)" >&2
  exit 1
fi

printf 'the adapter installs the %s the core execs\n' "$bin"
