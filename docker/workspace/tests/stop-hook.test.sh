#!/usr/bin/env bash
set -uo pipefail

hook_script="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/stop-hook.sh"
tmp_root="$(mktemp -d)"
trap 'rm -rf "$tmp_root"' EXIT

failures=0
hook_status=0
hook_stderr=''

pass() { printf 'ok   %s\n' "$1"; }

fail() {
  printf 'FAIL %s: %s\n' "$1" "$2" >&2
  failures=$((failures + 1))
}

run_hook() {
  local workspace=$1 hook_input=$2
  local err_file="$tmp_root/stderr"
  hook_status=0
  CORCODE_WORKSPACE="$workspace" bash "$hook_script" <<<"$hook_input" \
    >/dev/null 2>"$err_file" || hook_status=$?
  hook_stderr="$(cat "$err_file")"
}

expect_status() {
  local name=$1 expected=$2
  if [ "$hook_status" -eq "$expected" ]; then
    pass "$name"
  else
    fail "$name" "expected exit $expected, got $hook_status (stderr: $hook_stderr)"
  fi
}

expect_stderr_mentions() {
  local name=$1 needle=$2
  case "$hook_stderr" in
    *"$needle"*) pass "$name" ;;
    *) fail "$name" "stderr never mentioned '$needle': $hook_stderr" ;;
  esac
}

seeded_repo() {
  local name=$1
  local repo="$tmp_root/$name"
  git init --quiet --initial-branch=main "$repo"
  git -C "$repo" config user.name 'Test Agent'
  git -C "$repo" config user.email 'test@example.com'
  git -C "$repo" config commit.gpgsign false
  git init --quiet --bare "$repo.git"
  git -C "$repo" remote add origin "$repo.git"
  printf 'seed\n' >"$repo/seed.txt"
  git -C "$repo" add seed.txt
  git -C "$repo" commit --quiet -m 'Seed the repository'
  printf '%s' "$repo"
}

pushed_repo() {
  local repo
  repo="$(seeded_repo "$1")"
  git -C "$repo" push --quiet --set-upstream origin main
  printf '%s' "$repo"
}

clean="$(pushed_repo clean)"
run_hook "$clean" '{"stop_hook_active": false}'
expect_status 'clean and pushed workspace allows the stop' 0

dirty="$(pushed_repo dirty)"
printf 'work in progress\n' >"$dirty/scratch.txt"
run_hook "$dirty" '{"stop_hook_active": false}'
expect_status 'uncommitted changes block the stop' 2
expect_stderr_mentions 'uncommitted changes are named in the block reason' 'uncommitted'

run_hook "$dirty" '{"stop_hook_active": true}'
expect_status 'stop_hook_active lets the second stop through' 0

unpushed="$(pushed_repo unpushed)"
printf 'more\n' >>"$unpushed/seed.txt"
git -C "$unpushed" commit --quiet --all -m 'Extend the seed'
run_hook "$unpushed" '{"stop_hook_active": false}'
expect_status 'unpushed commits block the stop' 2
expect_stderr_mentions 'unpushed commits are counted in the block reason' '1 commit'

no_upstream="$(seeded_repo no-upstream)"
run_hook "$no_upstream" '{"stop_hook_active": false}'
expect_status 'a branch with no upstream blocks the stop' 2
expect_stderr_mentions 'the missing upstream is named in the block reason' 'upstream'

run_hook "$tmp_root" '{"stop_hook_active": false}'
expect_status 'a workspace that is not a git repository allows the stop' 0

run_hook "$clean" '{}'
expect_status 'a hook input without stop_hook_active is treated as the first stop' 0

if [ "$failures" -gt 0 ]; then
  printf '%d test(s) failed\n' "$failures" >&2
  exit 1
fi
printf 'all stop-hook tests passed\n'
