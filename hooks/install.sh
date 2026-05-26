#!/usr/bin/env bash
set -euo pipefail

HOOK_SOURCE_DIR="$(git rev-parse --show-toplevel)/hooks"
GIT_DIR="$(git rev-parse --git-dir)"

copy_hook() {
  local src="$1"
  local dst="$2"
  mkdir -p "$(dirname "$dst")"
  cp "$src" "$dst"
  chmod +x "$dst"
}

install_for_dir() {
  local hook_dir="$1"
  mkdir -p "$hook_dir"
  for hook in "$HOOK_SOURCE_DIR"/*; do
    local name
    name="$(basename "$hook")"
    if [ "$name" = "install.sh" ]; then
      continue
    fi
    copy_hook "$hook" "$hook_dir/$name"
  done
}

install_for_dir "$GIT_DIR/hooks"
for wt in "$GIT_DIR"/worktrees/*; do
  if [ -d "$wt" ]; then
    install_for_dir "$wt/hooks"
  fi
done
