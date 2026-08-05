#!/usr/bin/env bash
set -euo pipefail

readonly request='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}'

transcript="$(mktemp)"
trap 'rm -f "$transcript"' EXIT

give_up() {
  printf 'smoke: %s\nsmoke: adapter transcript follows\n' "$1" >&2
  cat "$transcript" >&2
  exit 1
}

adapter_status=0
printf '%s\n' "$request" | timeout 120 claude-agent-acp >"$transcript" || adapter_status=$?
[ "$adapter_status" -eq 0 ] || give_up "the adapter exited $adapter_status"

response="$(jq -c -s '[.[] | select(.id == 1)] | first' "$transcript")"
[ "$response" != 'null' ] || give_up 'the adapter answered no initialize request'

jq -e '
  .jsonrpc == "2.0"
  and (has("error") | not)
  and (.result.protocolVersion | type) == "number"
  and (.result.agentInfo.name | type) == "string"
  and (.result.agentInfo.name | length) > 0
' >/dev/null <<<"$response" || give_up "malformed initialize result: $response"

printf 'smoke: ACP initialize handshake completed with %s\n' \
  "$(jq -r '"\(.result.agentInfo.name) \(.result.agentInfo.version), protocol \(.result.protocolVersion)"' <<<"$response")"
