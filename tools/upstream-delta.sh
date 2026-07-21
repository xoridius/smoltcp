#!/usr/bin/env bash

set -euo pipefail

readonly BASE_TAG=v0.13.1
readonly UPSTREAM_URL=https://github.com/smoltcp-rs/smoltcp.git
readonly LEDGER_START='<!-- upstream-ledger:start -->'
readonly LEDGER_END='<!-- upstream-ledger:end -->'

if (( $# != 0 )); then
    echo "usage: tools/upstream-delta.sh" >&2
    exit 2
fi

cd "$(git rev-parse --show-toplevel)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf -- "$tmp_dir"' EXIT
ledger="$tmp_dir/ledger.tsv"
upstream_prs="$tmp_dir/upstream.tsv"

if ! awk -v start="$LEDGER_START" -v end="$LEDGER_END" '
    function trim(value) {
        sub(/^[[:space:]]+/, "", value)
        sub(/[[:space:]]+$/, "", value)
        return value
    }
    function fail(message) {
        print "error: " message > "/dev/stderr"
        failed = 1
    }
    $0 == start {
        starts++
        if (inside || ends) fail("duplicate or misplaced upstream ledger start marker")
        inside = 1
        next
    }
    $0 == end {
        ends++
        if (!inside) fail("upstream ledger end marker has no matching start")
        inside = 0
        next
    }
    inside {
        if ($0 ~ /^[[:space:]]*$/) next
        count = split($0, field, "|")
        if (count != 5 || trim(field[1]) != "" || trim(field[5]) != "") {
            fail("malformed upstream ledger row at FORK.md:" NR)
            next
        }
        first = trim(field[2])
        second = trim(field[3])
        third = trim(field[4])
        if (!header) {
            if (first != "PR" || second != "Outcome" || third != "Note")
                fail("invalid upstream ledger header")
            header = 1
            next
        }
        if (!separator) {
            if (first !~ /^:?-{3,}:?$/ || second !~ /^:?-{3,}:?$/ || third !~ /^:?-{3,}:?$/)
                fail("invalid upstream ledger separator")
            separator = 1
            next
        }
        if (first !~ /^#[0-9]+$/) {
            fail("upstream ledger PR must be numeric at FORK.md:" NR)
            next
        }
        pr = substr(first, 2)
        if (seen[pr]++) {
            fail("duplicate upstream ledger PR #" pr)
            next
        }
        if (second != "integrated" && second != "adapted" &&
            second != "superseded" && second != "skipped") {
            fail("unknown outcome for upstream PR #" pr ": " second)
            next
        }
        if (third == "" || third ~ /\t/) {
            fail("missing or invalid note for upstream PR #" pr)
            next
        }
        print pr "\t" second "\t" third
        rows++
    }
    END {
        if (starts != 1 || ends != 1 || inside)
            fail("FORK.md must contain exactly one complete upstream ledger block")
        if (!header || !separator || rows == 0)
            fail("upstream ledger must contain a header and at least one row")
        if (failed) exit 1
    }
' FORK.md > "$ledger"; then
    exit 2
fi

if ! git remote get-url upstream >/dev/null 2>&1; then
    git remote add upstream "$UPSTREAM_URL"
fi
echo "fetching upstream..."
git fetch -q upstream \
    '+refs/heads/main:refs/remotes/upstream/main' \
    'refs/tags/v0.13.1:refs/tags/v0.13.1'

if ! git rev-parse -q --verify "$BASE_TAG^{commit}" >/dev/null; then
    echo "error: fixed base tag $BASE_TAG is unavailable" >&2
    exit 2
fi
if ! git merge-base --is-ancestor "$BASE_TAG" upstream/main; then
    echo "error: $BASE_TAG is not an ancestor of upstream/main" >&2
    exit 2
fi

status=0
: > "$upstream_prs"
declare -A ledger_outcomes upstream_seen
while IFS=$'\t' read -r pr outcome _note; do
    ledger_outcomes["$pr"]="$outcome"
done < "$ledger"

git rev-list --first-parent "$BASE_TAG..upstream/main" > "$tmp_dir/commits"
while IFS= read -r commit; do
    read -r -a parents <<< "$(git rev-list --parents -n 1 "$commit")"
    if (( ${#parents[@]} == 2 )); then
        echo "error: direct-to-main upstream commit $commit" >&2
        status=1
        continue
    fi
    subject="$(git show -s --format=%s "$commit")"
    if (( ${#parents[@]} != 3 )) ||
        [[ ! "$subject" =~ ^Merge\ pull\ request\ \#([0-9]+)\ from\ .+$ ]]; then
        echo "error: unrecognized upstream merge $commit: $subject" >&2
        status=1
        continue
    fi
    pr="${BASH_REMATCH[1]}"
    title="$(git show -s --format=%b "$commit" | awk 'NF { print; exit }')"
    if [[ -z "$title" || "$title" == *$'\t'* ]]; then
        echo "error: upstream merge $commit has no valid display title" >&2
        status=1
        continue
    fi
    if [[ -n "${upstream_seen[$pr]+present}" ]]; then
        echo "error: duplicate upstream pull request #$pr at $commit" >&2
        status=1
        continue
    fi
    upstream_seen["$pr"]=1
    printf '%s\t%s\t%s\n' "$pr" "$title" "$commit" >> "$upstream_prs"
done < "$tmp_dir/commits"

for pr in "${!ledger_outcomes[@]}"; do
    if [[ -z "${upstream_seen[$pr]+present}" ]]; then
        echo "error: ledger PR #$pr is absent from $BASE_TAG..upstream/main" >&2
        exit 2
    fi
done

echo
echo "Upstream pull requests since $BASE_TAG:"
while IFS=$'\t' read -r pr title _commit; do
    outcome="${ledger_outcomes[$pr]-}"
    if [[ -z "$outcome" ]]; then
        printf '  [NEW]        #%s %s\n' "$pr" "$title"
        status=1
    else
        printf '  [%-10s] #%s %s\n' "$outcome" "$pr" "$title"
    fi
done < "$upstream_prs"

if (( status != 0 )); then
    echo
    echo "Upstream classification is incomplete." >&2
    exit 1
fi

echo
echo "All upstream pull requests are classified."
