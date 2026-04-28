#!/usr/bin/env bash
# scripts/prune-bench-history.sh — rotate bench/history/ JSON runs.
#
# Issue: https://github.com/copyleftdev/tak-rs/issues/6
#
# Retention policy (mirrors common log rotation conventions):
#   - daily   : keep all files for the last 30 days
#   - weekly  : for files older than 30 days but younger than 180,
#               keep one per ISO week (Mon-Sun bucket)
#   - monthly : for files older than 180 days, keep one per calendar
#               month forever
#
# Files are matched by a UTC timestamp embedded in the filename:
#   <tag>-YYYY-MM-DDTHH-MM-SSZ.json   (the format bench-baseline.sh emits)
#
# Idempotent: dry-run by default; pass --apply to actually delete.
#
# Usage:
#   scripts/prune-bench-history.sh            # dry-run
#   scripts/prune-bench-history.sh --apply    # delete the losers
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
HISTORY_DIR="${BENCH_HISTORY_DIR:-$SCRIPT_DIR/../bench/history}"
APPLY=0
NOW_EPOCH="$(date -u +%s)"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --apply)        APPLY=1; shift ;;
        --history-dir)  HISTORY_DIR="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,18p' "$0"; exit 0 ;;
        *)              echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

if [[ ! -d "$HISTORY_DIR" ]]; then
    echo "[prune] no history dir at $HISTORY_DIR — nothing to do" >&2
    exit 0
fi

# ----------------------------------------------------------------------
# Build a list of (epoch, path, bucket-key) tuples.
# bucket-key is computed differently per age tier:
#   daily   : epoch (each file is its own bucket — keep all)
#   weekly  : YYYY-WW (ISO week)
#   monthly : YYYY-MM (calendar month)
# We then group by bucket-key and keep the newest (largest epoch) per key.
# ----------------------------------------------------------------------
day_secs=$((60 * 60 * 24))
cutoff_30d=$(( NOW_EPOCH - day_secs * 30 ))
cutoff_180d=$(( NOW_EPOCH - day_secs * 180 ))

records="$(mktemp)"
trap 'rm -f "$records"' EXIT

shopt -s nullglob
for f in "$HISTORY_DIR"/*.json; do
    base="${f##*/}"
    # tag-YYYY-MM-DDTHH-MM-SSZ.json  →  YYYY-MM-DDTHH-MM-SSZ
    ts="$(echo "$base" | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}-[0-9]{2}-[0-9]{2}Z' || true)"
    if [[ -z "$ts" ]]; then
        # Doesn't match the expected pattern — skip, never delete.
        continue
    fi
    # Convert UTC ISO timestamp to epoch. The hyphens between H/M/S
    # need to be normalized back to colons for `date -d` to parse them.
    iso="$(echo "$ts" | sed 's/T\([0-9]*\)-\([0-9]*\)-\([0-9]*\)Z/T\1:\2:\3Z/')"
    file_epoch="$(date -u -d "$iso" +%s 2> /dev/null || true)"
    [[ -z "$file_epoch" ]] && continue

    if (( file_epoch >= cutoff_30d )); then
        bucket="daily-$file_epoch"
    elif (( file_epoch >= cutoff_180d )); then
        bucket="weekly-$(date -u -d "$iso" +%G-%V 2>/dev/null)"
    else
        bucket="monthly-$(date -u -d "$iso" +%Y-%m 2>/dev/null)"
    fi
    printf '%s\t%s\t%s\n' "$file_epoch" "$bucket" "$f" >> "$records"
done
shopt -u nullglob

# ----------------------------------------------------------------------
# Per bucket, keep newest. Mark all others for delete.
# ----------------------------------------------------------------------
keepers="$(sort -k2,2 -k1,1nr "$records" | awk -F'\t' '!seen[$2]++ { print $3 }' | sort -u)"
all="$(awk -F'\t' '{ print $3 }' "$records" | sort -u)"
losers="$(comm -23 <(echo "$all") <(echo "$keepers"))"

# Stats
total_count=$(wc -l < "$records" | tr -d ' ')
keep_count=$(echo "$keepers" | grep -c . || true)
lose_count=$(echo "$losers" | grep -c . || true)

echo "[prune] history dir : $HISTORY_DIR"
echo "[prune] now (UTC)   : $(date -u -d "@$NOW_EPOCH" +%FT%TZ)"
echo "[prune] total files : $total_count"
echo "[prune] keep        : $keep_count (daily<30d; weekly<180d; monthly forever)"
echo "[prune] prune       : $lose_count"

if [[ -z "$losers" ]]; then
    exit 0
fi

if [[ "$APPLY" -ne 1 ]]; then
    echo "[prune] (dry-run; pass --apply to delete)"
    while IFS= read -r f; do
        [[ -n "$f" ]] && echo "  would-delete: $f"
    done <<< "$losers"
    exit 0
fi

while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    rm -f "$f"
    echo "  deleted: $f"
done <<< "$losers"

echo "[prune] done"
