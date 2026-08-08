#!/usr/bin/env bash
set -uo pipefail

workspace="${CORCODE_WORKSPACE:-/workspace}"

in_workspace() { git -C "$workspace" "$@"; }

block() {
  printf 'Stop blocked — this work is not safe in git yet:\n%s\nCommit it with a meaningful message and push, then stop.\n' \
    "$1" >&2
  exit 2
}

stand_down() {
  printf 'Stop allowed — this workspace could not push if it tried: %s.\n' "$1" >&2
  exit 0
}

# Only a stored credential counts. Nothing in a workspace container can answer
# a prompt, so every route to one is closed here: what an askpass would type is
# not something this workspace could push over.
credential_answers_for() {
  local filled
  filled="$(
    unset GIT_ASKPASS SSH_ASKPASS
    GIT_TERMINAL_PROMPT=0 in_workspace -c core.askPass= credential fill 2>/dev/null <<EOF
protocol=https
host=$1

EOF
  )" || return 1
  case $'\n'"$filled" in
  *$'\n'password=?*) return 0 ;;
  *) return 1 ;;
  esac
}

# Git's credential machinery only speaks for http(s). Any other transport
# carries its own keys, and a hook is not the place to second-guess those.
unpushable_because() {
  local url=$1 host
  case $url in
  http://* | https://*) ;;
  *) return 1 ;;
  esac
  host="${url#*://}"
  host="${host#*@}"
  host="${host%%/*}"
  credential_answers_for "$host" && return 1
  printf 'no credential answers for %s' "$host"
}

hook_input="$(cat)"
[ "$(jq -r '.stop_hook_active // false' <<<"$hook_input")" != 'true' ] || exit 0
in_workspace rev-parse --git-dir >/dev/null 2>&1 || exit 0

origin_url="$(in_workspace remote get-url origin 2>/dev/null)"
[ -n "$origin_url" ] || stand_down 'it has no origin remote'
unpushable="$(unpushable_because "$origin_url")" && stand_down "$unpushable"

problems=()
[ -z "$(in_workspace status --porcelain --untracked-files=no)" ] ||
  problems+=('- tracked files have uncommitted changes')

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
