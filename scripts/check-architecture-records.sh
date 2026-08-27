#!/usr/bin/env bash

# Keep completed or superseded designs from presenting themselves as active
# architecture. Historical research is allowed under openspec/changes/archive;
# the running stack is described by current specs and executable source.
set -euo pipefail

REPOSITORY="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPOSITORY"

required_specs=(
  openspec/specs/room-v5-replicas/spec.md
  openspec/specs/browser-replica-worker/spec.md
)

for spec in "${required_specs[@]}"; do
  if [[ ! -f "$spec" ]]; then
    echo "missing current architecture spec: $spec" >&2
    exit 1
  fi
done

retired_changes=(
  hard-cut-room-v4
  move-browser-replicas-to-worker
  rewrite-p2panda-hhhs-stack
)

for change in "${retired_changes[@]}"; do
  if [[ -e "openspec/changes/$change" ]]; then
    echo "retired change still appears active: openspec/changes/$change" >&2
    exit 1
  fi
done

for tasks in openspec/changes/*/tasks.md; do
  [[ -e "$tasks" ]] || continue
  if rg -q '^- \[[xX]\]' "$tasks" && ! rg -q '^- \[( |~)\]' "$tasks"; then
    echo "completed change has not been archived: $tasks" >&2
    exit 1
  fi
done

if rg -n -i 'p2panda|room[_ -]?v4' \
  Cargo.toml Cargo.lock src src-tauri tests relay \
  --glob '*.rs' --glob '*.toml' --glob '*.lock' --glob '*.sh'; then
  echo "retired p2panda or Room-v4 architecture leaked into the live stack" >&2
  exit 1
fi

if rg -n -i 'P2P networking.*TBD|p2panda-(sync|net)|matchbox' openspec/project.md; then
  echo "openspec/project.md advertises a retired or undecided network stack" >&2
  exit 1
fi

echo "architecture records describe the live Room-v5 worker/carrier boundary"
