#!/usr/bin/env bash

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf -- "$tmp_dir"' EXIT

git -C "$tmp_dir" init -q
git -C "$tmp_dir" remote add upstream https://example.invalid/smoltcp.git
mkdir "$tmp_dir/tools"
cp "$repo_root/tools/upstream-delta.sh" "$tmp_dir/tools/"
cat > "$tmp_dir/FORK.md" <<'EOF'
<!-- upstream-ledger:start -->
| PR | Outcome | Note |
|---:|---|---|
| #1 | skipped | Test fixture. |
<!-- upstream-ledger:end -->
EOF

if output="$(cd "$tmp_dir" && bash tools/upstream-delta.sh 2>&1)"; then
    echo "error: upstream-delta accepted a noncanonical upstream remote" >&2
    exit 1
fi
if [[ "$output" != *"upstream remote URL mismatch"* ]]; then
    echo "error: unexpected upstream-delta failure: $output" >&2
    exit 1
fi
