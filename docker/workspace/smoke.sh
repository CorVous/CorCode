#!/usr/bin/env bash
set -euo pipefail

readonly request='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}'

transcript="$(mktemp)"
trap 'rm -f "$transcript"' EXIT

printf '%s\n' "$request" | timeout 120 claude-agent-acp >"$transcript"

response="$(jq -c -s '[.[] | select(.id == 1)] | first' "$transcript")"
if [ "$response" = 'null' ]; then
  printf 'smoke: the adapter answered no initialize request. Transcript:\n' >&2
  cat "$transcript" >&2
  exit 1
fi

if ! jq -e '
  .jsonrpc == "2.0"
  and (has("error") | not)
  and (.result.protocolVersion | type) == "number"
  and (.result.agentInfo.name | type) == "string"
  and (.result.agentInfo.name | length) > 0
  and .result.agentCapabilities.loadSession == true
' >/dev/null <<<"$response"; then
  printf 'smoke: malformed initialize result: %s\n' "$response" >&2
  exit 1
fi

printf 'smoke: ACP initialize handshake completed with %s\n' \
  "$(jq -r '"\(.result.agentInfo.name) \(.result.agentInfo.version), protocol \(.result.protocolVersion)"' <<<"$response")"
