#!/usr/bin/env bash

source common.sh

TODO_NixOS

# `write-through-store` is read by whichever process runs the build, so through
# the daemon it would be the daemon's setting rather than the one under test.
needLocalStore "'write-through-store' is read by the process that performs the build"

# The builder announces itself, so a later realisation that does not print this
# came from the cache rather than from running the build again.
builderMarker="building-the-write-through-store-fixture"

wtCache="$TEST_ROOT/wtcache"
rm -rf "$wtCache"

clearStore

# The output is published by the build, so it is on the destination by the time
# the build command returns.
outPath=$(nix-build write-through-store.nix --no-out-link \
    --option write-through-store "file://$wtCache")

hashPart=$(basename "$outPath")
hashPart=${hashPart%%-*}
narinfo="$wtCache/$hashPart.narinfo"

[[ -e $narinfo ]]
grepQuiet "^StorePath: $outPath\$" "$narinfo"
# The NAR the narinfo points at is there too, not just the metadata.
narUrl=$(sed -n 's/^URL: //p' "$narinfo")
[[ -e "$wtCache/$narUrl" ]]

# Having been published, the output can be dropped locally and fetched back.
nix-store --delete "$outPath"
[[ ! -e $outPath ]]

rebuildLog="$TEST_ROOT/write-through-rebuild.log"
nix-build write-through-store.nix --no-out-link \
    --substituters "file://$wtCache" --no-require-sigs \
    2> "$rebuildLog"
[[ -e $outPath ]]
grepQuietInverse "$builderMarker" "$rebuildLog"
grepQuiet 'copying path' "$rebuildLog"

# A destination that cannot be written fails the build, rather than leaving a
# build that reports success with outputs in only one place.
touch "$TEST_ROOT/write-through-not-a-directory"

clearStore

failLog="$TEST_ROOT/write-through-fail.log"
if nix-build write-through-store.nix --no-out-link \
    --option write-through-store "file://$TEST_ROOT/write-through-not-a-directory/cache" \
    2> "$failLog"; then
    echo "the build should have failed: its write-through destination is unusable" >&2
    exit 1
fi
grepQuiet 'were built but not published' "$failLog"
grepQuiet 'write-through' "$failLog"

# Unset is off: nothing reaches the destination of the earlier runs, and no new
# one is created.
before="$TEST_ROOT/write-through-cache-before"
after="$TEST_ROOT/write-through-cache-after"
find "$wtCache" | sort > "$before"

clearStore

nix-build write-through-store.nix --no-out-link > /dev/null

find "$wtCache" | sort > "$after"
diff -u "$before" "$after"
[[ ! -e "$TEST_ROOT/write-through-not-a-directory/cache" ]]
