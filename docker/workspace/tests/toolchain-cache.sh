#!/usr/bin/env bash
set -uo pipefail

workspace="${CORCODE_WORKSPACE:-/workspace}"
probe="$workspace/toolchain-cache-probe"
scratch_cache='/tmp/cache'

failures=0

pass() { printf 'ok   %s\n' "$1"; }

fail() {
  printf 'FAIL %s: %s\n' "$1" "$2" >&2
  failures=$((failures + 1))
}

expect_on_scratch() {
  local name=$1 path=$2
  case "$path" in
  "$scratch_cache"/*) pass "$name" ;;
  *) fail "$name" "'$path' is not under $scratch_cache, the only place this container may write toolchain state" ;;
  esac
}

expect_succeeds() {
  local name=$1
  shift
  local output
  if output="$("$@" 2>&1)"; then
    pass "$name"
  else
    fail "$name" "$* exited $?: $output"
  fi
}

expect_on_scratch 'cargo keeps its registry off the read-only rootfs' "${CARGO_HOME-}"
expect_on_scratch 'npm keeps its cache off the read-only rootfs' "$(npm config get cache)"
expect_on_scratch 'pip keeps its cache off the read-only rootfs' "${PIP_CACHE_DIR-}"
expect_on_scratch 'everything XDG keeps its cache off the read-only rootfs' "${XDG_CACHE_HOME-}"
expect_on_scratch 'gh keeps its config off the read-only rootfs' "${GH_CONFIG_DIR-}"

expect_succeeds 'the baked Rust toolchain is still reachable' cargo --version
expect_succeeds 'the baked Rust compiler is still reachable' rustc --version

mkdir -p "$probe"
trap 'rm -rf "$probe"' EXIT

expect_succeeds 'a real dependency installs into the workspace' \
  npm install --prefix "$probe" --no-audit --no-fund --loglevel=error is-number@7.0.0

printf '#!/bin/sh\necho built\n' >"$probe/built.sh"
chmod +x "$probe/built.sh"
expect_succeeds 'what a build writes into the workspace can be run' "$probe/built.sh"

if [ "$failures" -gt 0 ]; then
  printf '%d check(s) failed\n' "$failures" >&2
  exit 1
fi
printf 'all toolchain-cache checks passed\n'
