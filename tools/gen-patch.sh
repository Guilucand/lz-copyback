#!/usr/bin/env bash
# Regenerate external/<crate>/ggcat-lz-hook.patch: the diff of the fork against the
# pristine upstream crate, applyable with `git apply -p1` from the fork directory.
#
#   ./tools/gen-patch.sh lzma-sys liblzma-sys-0.4.7
#   ./tools/gen-patch.sh zstd-sys 'zstd-sys-2.0.16+zstd.1.5.7'
#   ./tools/gen-patch.sh lz4-flex lz4_flex-0.11.6
#
# Only files present in the fork are diffed. Pruning (whole directories dropped from the
# upstream tree) is documented in PATCH.md instead, so it does not bury the real change
# under tens of thousands of deletion lines.
set -euo pipefail

crate=$1
upstream=$2
root="$(cd "$(dirname "$0")/.." && pwd)"
reg=$(echo "$HOME"/.cargo/registry/src/*/"$upstream")
[ -d "$reg" ] || { echo "no pristine tree at $reg" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -r "$reg" "$work/a"
cp -r "$root/external/$crate" "$work/b"
chmod -R u+w "$work"

# Files describing or derived from the fork itself are not part of the diff: the patch
# must not try to create itself (the defect in the first generation of these files), and
# lzcb_prefix.h is produced by tools/gen-symbol-prefix.sh, not applied by hand.
for f in ggcat-lz-hook.patch PATCH.md lzcb_prefix.h Cargo.lock .cargo-ok Cargo.toml.orig; do
    rm -f "$work/a/$f" "$work/b/$f"
done
rm -rf "$work/a/target" "$work/b/target" "$work/b/tests"

out="$root/external/$crate/ggcat-lz-hook.patch"
: > "$out"
( cd "$work/b" && find . -type f | sed 's|^\./||' | sort ) | while read -r f; do
    if [ -f "$work/a/$f" ]; then
        cmp -s "$work/a/$f" "$work/b/$f" && continue
        ( cd "$work" && diff -u "a/$f" "b/$f" || true ) >> "$out"
    else
        ( cd "$work" && diff -uN "a/$f" "b/$f" || true ) >> "$out"
    fi
done

echo "wrote external/$crate/ggcat-lz-hook.patch ($(grep -c '^--- a/' "$out") files, $(wc -l < "$out") lines)"
