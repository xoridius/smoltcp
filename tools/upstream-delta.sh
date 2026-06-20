#!/usr/bin/env bash
#
# upstream-delta.sh — show which upstream smoltcp PRs have landed since the
# fork's base tag and are NOT yet recorded in FORK.md §16 (backported or
# deliberately skipped).
#
# The fork branches from upstream at v0.13.1, but changes are cherry-picked
# rather than merged (the local edits to tcp.rs / congestion*.rs make a full
# merge conflict-heavy). This script answers "what is new upstream that we
# haven't triaged yet?". It only inspects the upstream side, so it works even
# in a shallow clone where `git merge-base` against upstream would fail. See
# FORK.md §2 for the shallow-clone caveat.
#
# Usage:
#   tools/upstream-delta.sh [base-tag]      # default base tag: v0.13.1
#
# It adds the `upstream` remote if missing and fetches it.

set -euo pipefail

BASE_TAG="${1:-v0.13.1}"
UPSTREAM_URL="https://github.com/smoltcp-rs/smoltcp.git"

cd "$(git rev-parse --show-toplevel)"

if ! git remote get-url upstream >/dev/null 2>&1; then
    echo "adding 'upstream' remote -> ${UPSTREAM_URL}"
    git remote add upstream "${UPSTREAM_URL}"
fi
echo "fetching upstream..."
git fetch -q upstream

if ! git rev-parse -q --verify "${BASE_TAG}^{commit}" >/dev/null; then
    echo "error: base tag '${BASE_TAG}' not found (fetch tags from upstream?)" >&2
    exit 1
fi

# PR numbers already recorded in FORK.md §16 (backported OR deliberately skipped).
recorded="$(grep -oE '#[0-9]+' FORK.md | tr -d '#' | sort -u)"

total=0
new=0
echo
echo "Upstream PRs merged since ${BASE_TAG} (newest first):"
echo "  [status] #PR    title"
echo "  -------------------------------------------------------------"
while read -r subject; do
    [ -z "${subject}" ] && continue
    total=$((total + 1))
    pr="$(printf '%s' "${subject}" | grep -oE 'pull request #[0-9]+' | grep -oE '[0-9]+' || true)"
    title="$(printf '%s' "${subject}" | sed 's/^Merge pull request #[0-9]* from //')"
    if [ -z "${pr}" ]; then
        status="?      "
    elif printf '%s\n' "${recorded}" | grep -qx "${pr}"; then
        status="handled"
    else
        status="NEW    "
        new=$((new + 1))
    fi
    printf '  [%s] #%-5s %s\n' "${status}" "${pr:-?}" "${title}"
done < <(git log --merges --format='%s' "${BASE_TAG}..upstream/main")

# Direct-to-main (non-merge, first-parent) commits are rare upstream but would
# be missed by the merge listing above; surface a count as a tripwire.
direct="$(git log --no-merges --first-parent --format='%h' "${BASE_TAG}..upstream/main" | wc -l | tr -d ' ')"

echo
echo "  ${total} merge-PRs total, ${new} NEW (untriaged), ${direct} direct-to-main commits."
echo
echo "NEW = not referenced in FORK.md §16. Triage each: cherry-pick the"
echo "relevant commits onto a feature branch, run the §3 test matrix, then"
echo "record the outcome (backported / adapted / skipped) in FORK.md §16."
echo "If 'direct-to-main commits' is non-zero, inspect them with:"
echo "    git log --no-merges --first-parent ${BASE_TAG}..upstream/main"
