#!/usr/bin/env bash
# Keep the idle-poll call-site guard separate from its regression fixtures.
# Bare multiline trait-method signatures are declarations, not timed polls.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

violations() {
  grep -F 'poll_input(' |
    grep -vE ':[0-9]+:[[:space:]]*//' |
    grep -vE ':[0-9]+:[[:space:]]*fn poll_input\([[:space:]]*$' |
    grep -vE 'poll_input\(None\)|Duration::ZERO\)|poll_input\(timeout\)'
}

if [[ ${1:-} == --self-test ]]; then
  allowed=(
    'sample.rs:1:    fn poll_input('
    'sample.rs:1: // poll_input(Some(Duration::from_millis(100)))'
    'sample.rs:1: term.poll_input(None)'
    'sample.rs:1: term.poll_input(Some(std::time::Duration::ZERO))'
    'sample.rs:1: term.poll_input(timeout)'
  )
  rejected=(
    'sample.rs:1: term.poll_input(Some(Duration::from_millis(100)))'
    'sample.rs:1: term.poll_input('
    'sample.rs:1: fn poll_input(&mut self) { term.poll_input(delay); }'
    'sample.rs:1: other.poll_input(delay)'
  )
  for line in "${allowed[@]}"; do
    if printf '%s\n' "$line" | violations >/dev/null; then
      echo "ERROR: idle-poll guard rejected permitted line: $line" >&2
      exit 1
    fi
  done
  for line in "${rejected[@]}"; do
    if ! printf '%s\n' "$line" | violations >/dev/null; then
      echo "ERROR: idle-poll guard accepted forbidden line: $line" >&2
      exit 1
    fi
  done
  echo "idle-poll guard: 9 regression cases passed"
  exit 0
fi

poll_sites=$(grep -rIn 'poll_input(' crates/thegn-host/src --include='*.rs')
if printf '%s\n' "$poll_sites" | violations; then
  echo 'ERROR: a timed poll_input outside idle_poll::poll_timeout — the idle loop must never poll (CLAUDE.md)' >&2
  exit 1
fi
if [[ $(printf '%s\n' "$poll_sites" | grep -F 'poll_input(timeout)' | grep -vcE ':[0-9]+:[[:space:]]*//') != 1 ]]; then
  echo 'ERROR: expected exactly one poll_input(timeout) site (run.rs)' >&2
  exit 1
fi
