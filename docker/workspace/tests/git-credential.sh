#!/usr/bin/env bash
# What the baked git credential helper answers, and to whom. Runs inside the
# image: the config under test is the one the build wrote, and the hook under
# test is the one the build installed.
set -uo pipefail

hook='/usr/local/bin/corcode-stop-hook'
token='ghp_0123456789abcdefghijklmnopqrstuvwxyz'
github='https://github.com/CorVous/fixture.git'
foreign='https://git.example.invalid/agent/work.git'
# The container is spawned with the token in its environment or with nothing
# at all (ADR-0001), and every check here says which of the two it is asking
# about. A helper is not allowed to wait for a person either way: nothing in a
# workspace container can answer a prompt.
readonly PATIENCE=20

tmp_root="$(mktemp -d)"
trap 'rm -rf "$tmp_root"' EXIT

failures=0

pass() { printf 'ok   %s\n' "$1"; }

fail() {
  printf 'FAIL %s: %s\n' "$1" "$2" >&2
  failures=$((failures + 1))
}

# What `git credential fill` comes back with for one host, holding the token or
# holding nothing. Answering at all is the question, so a refusal is an empty
# answer rather than a failure of the check.
filled_for() {
  local host=$1
  shift
  printf 'protocol=https\nhost=%s\n\n' "$host" |
    env --unset=GIT_ASKPASS --unset=SSH_ASKPASS "$@" GIT_TERMINAL_PROMPT=0 \
      timeout "$PATIENCE" git -c core.askPass= credential fill 2>/dev/null
}

expect_password() {
  local name=$1 filled=$2 expected=$3
  case $'\n'"$filled" in
  *$'\n'"password=$expected"$'\n'* | *$'\n'"password=$expected") pass "$name" ;;
  *) fail "$name" "no credential for the token in the environment: ${filled//$token/<the token>}" ;;
  esac
}

expect_no_password() {
  local name=$1 filled=$2
  case $'\n'"$filled" in
  *$'\n'password=*) fail "$name" "a credential was handed out: ${filled//$token/<the token>}" ;;
  *) pass "$name" ;;
  esac
}

# A workspace as the core leaves one: cloned from github.com over a URL
# carrying no credential of its own, standing on a branch with work on it.
workspace_on() {
  local name=$1 origin=$2
  local repo="$tmp_root/$name"
  git init --quiet --initial-branch=main "$repo"
  git -C "$repo" remote add origin "$origin"
  printf 'seed\n' >"$repo/seed.txt"
  git -C "$repo" add seed.txt
  git -C "$repo" commit --quiet -m 'Seed the repository'
  printf 'work in progress\n' >>"$repo/seed.txt"
  printf '%s' "$repo"
}

hook_status_over() {
  local workspace=$1
  shift
  env "$@" CORCODE_WORKSPACE="$workspace" timeout "$PATIENCE" bash "$hook" \
    <<<'{"stop_hook_active": false}' >/dev/null 2>"$tmp_root/hook_stderr"
  printf '%s' "$?"
}

expect_hook_status() {
  local name=$1 got=$2 expected=$3
  if [ "$got" = "$expected" ]; then
    pass "$name"
  else
    fail "$name" "expected exit $expected, got $got (stderr: $(cat "$tmp_root/hook_stderr"))"
  fi
}

expect_password 'a container holding the token is answered for github.com' \
  "$(filled_for github.com "GH_TOKEN=$token")" "$token"

expect_no_password 'a container holding no token is answered nothing, without waiting for anyone' \
  "$(filled_for github.com --unset=GH_TOKEN --unset=GITHUB_TOKEN)"

expect_no_password 'no other host is offered the token, however it is spelled' \
  "$(filled_for git.example.invalid "GH_TOKEN=$token")"
expect_no_password 'a host that merely ends in the one we trust is another host' \
  "$(filled_for github.com.evil.example "GH_TOKEN=$token")"
# gh turns the other two spellings away by itself, so they would pass over an
# unscoped helper too. This one only passes because the helper is scoped: gh
# would spend the token on an enterprise host under github.com, and git never
# asks it to, because the scope is an exact host.
expect_no_password 'a host merely under the one we trust is another host' \
  "$(filled_for foo.github.com "GH_TOKEN=$token")"

credentialed="$(workspace_on credentialed "$github")"
expect_hook_status 'the stop hook keeps its teeth where the token could push' \
  "$(hook_status_over "$credentialed" "GH_TOKEN=$token")" 2

uncredentialed="$(workspace_on uncredentialed "$github")"
expect_hook_status 'the stop hook stands down where no token answers' \
  "$(hook_status_over "$uncredentialed" --unset=GH_TOKEN --unset=GITHUB_TOKEN)" 0

elsewhere="$(workspace_on elsewhere "$foreign")"
expect_hook_status 'the stop hook stands down over a host the token never reaches' \
  "$(hook_status_over "$elsewhere" "GH_TOKEN=$token")" 0

# An askpass answers where a person would have been asked, and no one is here
# to be asked. Whatever it says, the container still cannot push.
askpass="$tmp_root/askpass"
printf '#!/usr/bin/env bash\nprintf %%s\\\\n what-a-person-would-type\n' >"$askpass"
chmod +x "$askpass"
prompted="$(workspace_on prompted "$github")"
expect_hook_status 'the stop hook stands down where only a prompt would answer' \
  "$(hook_status_over "$prompted" --unset=GH_TOKEN --unset=GITHUB_TOKEN "GIT_ASKPASS=$askpass")" 0

if [ "$failures" -gt 0 ]; then
  printf '%d check(s) failed\n' "$failures" >&2
  exit 1
fi
printf 'all git-credential checks passed\n'
