#!/usr/bin/env bash
set -uo pipefail

workspace="${CORCODE_WORKSPACE:-/workspace}"

in_workspace() { git -C "$workspace" "$@"; }

block() {
  printf 'Stop blocked — this work is not safe in git yet:\n%s\nCommit it with a meaningful message and push, then stop.\n' \
    "$1" >&2
  exit 2
}

hook_input="$(cat)"
[ "$(jq -r '.stop_hook_active // false' <<<"$hook_input")" != 'true' ] || exit 0
in_workspace rev-parse --git-dir >/dev/null 2>&1 || exit 0

problems=()
[ -z "$(in_workspace status --porcelain)" ] ||
  problems+=('- the working tree has uncommitted changes')

if in_workspace rev-parse --verify --quiet HEAD >/dev/null; then
  upstream="$(in_workspace rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null)"
  if [ -z "$upstream" ]; then
    problems+=('- the branch has no upstream, so none of its commits are on the remote')
  else
    unpushed="$(in_workspace rev-list --count "$upstream..HEAD")"
    [ "$unpushed" -eq 0 ] ||
      problems+=("- $unpushed commit(s) are not pushed to $upstream")
  fi
fi

[ "${#problems[@]}" -gt 0 ] || exit 0
block "$(printf '%s\n' "${problems[@]}")"
