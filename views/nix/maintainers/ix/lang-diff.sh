#!/usr/bin/env bash
# Differentially run tests/functional/lang's eval corpus under two evaluator
# arms of ONE nix build and diff the outcomes. The corpus .exp files are not
# consulted: arm A is the oracle, so the comparison is live A-vs-B on the same
# binary and can never suffer version skew or stale expectations.
#
#   lang-diff.sh NIXBINDIR ARM_A ARM_B [--only GLOB]
#   lang-diff.sh NIXBINDIR --self-diff [--only GLOB]
#
# NIXBINDIR: directory containing the nix-instantiate and nix binaries under
#            test. Passed explicitly, never taken from PATH, because a
#            differ that silently picks up an ambient nix measures the wrong
#            thing; the binary path, its sha256 and its version are printed
#            on the RESULT line.
# --only:    run only the pairs whose test name matches this shell glob
#            (e.g. --only 'eval-okay-attrs*', or --only '@(*sort*|*map*)' for
#            a set of related cases), for a seconds-long loop while
#            working one case. The RESULT line always reports corpus= (every
#            pair discovered) beside pairs= (the ones run), so a filtered run
#            can never be read as a full one, and a glob matching nothing
#            trips the same zero-pairs refusal an empty corpus does.
# ARM:       SETTING=VALUE injected via NIX_CONFIG, or "none" for the plain
#            default path. eval-backend=rust also enables the rust-eval
#            experimental feature, which that setting needs and nothing else
#            does (same shape as eval-identity-harness.sh's eval-cores rule).
#
# --self-diff runs arm "none" against arm "none": every pair must match, and
# the arms-really-differ gate is waived because identical arms are the point.
# This mode is the harness's own permanent smoke test: a runner change that
# breaks comparison shows up here as a mismatch with no evaluator involved.
#
# Arms-really-differ gate (non-self-diff runs): when both arms set the same
# SETTING, `nix config show SETTING` must return different values under the
# two arms, else the run refuses. Exists because an option that never reaches
# nix leaves two arms byte-identical and reports a pass that means nothing
# (the eval-identity-harness.sh lesson). From M1 on, the NIX_SHOW_STATS
# "evaluator" field is asserted per-case as the stronger probe when present.
#
# Per-case outcome lattice, ordered by precedence:
#   crash          either arm died by signal (exit >= 128)
#   corpus-fail    arm A (the oracle) did not behave as the corpus name says
#   unimplemented  arm B stderr says "rust-eval unimplemented:"
#   allowlisted    outcomes differ but the case is in eval-allowlist.toml
#   mismatch       eval-okay: stdout bytes or exit differ;
#                  eval-fail: arm B succeeded, or error CLASS differs
#   match / fail-as-fail
#
# Error text is never byte-compared for eval-fail pairs; only the class from
# a fixed enum {parse, throw, assert, type, missing-attr, infinite-recursion,
# stack-overflow, abort, unimplemented, unknown}. unknown-vs-unknown counts as
# a class match only when the two arms' TERMINAL error lines are identical, so
# a genuinely novel divergence cannot hide inside "unknown".
#
# Terminal line, not the whole stream: a cppnix failure prints `error:`, then
# trace notes with file/line/source excerpts, then the real message; the Rust
# arm carries no source positions (ENG-12137) and prints the message alone.
# Comparing whole streams therefore made the position block, not the error,
# decide every unknown pair. The terminal line is still compared byte for byte
# after unindenting, so two different errors still differ.
#
# `assert` also accepts cppnix's assertEqValues family ("... is not equal to
# ...", "attribute names of attribute set ... differs from ..."), which is the
# detailed diagnostic cppnix produces for a false `assert a == b`. Both arms
# raise cppnix's AssertionError; only the message differs, because the Rust
# arm reports the generic "assertion failed" (ENG-12138).
#
# Exit 0 iff pairs > 0 and mismatch = crash = corpus-fail = 0. An empty
# corpus is a failure (exit 2), never a pass: a glob that matched nothing
# must not read as "nothing diverged".
set -u
# nullglob is load-bearing: without it an empty corpus leaves the literal
# pattern in the loop, which evaluates as a file, fails, and counts as one
# fail-as-fail pair instead of tripping the zero-pairs refusal. Found by
# breaking the guard on purpose; keep the break-it test when changing this.
shopt -s nullglob
# extglob so --only can name a set: '@(*sort*|*map*)' is the shape a chunk of
# related cases wants, and a plain glob cannot express alternation. Adding it
# cannot change how an existing pattern matches; extglob only recognises the
# ?( *( +( @( !( forms, which are syntax errors without it.
shopt -s extglob

usage() { grep '^#' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }

ONLY=
rest=()
while [ $# -gt 0 ]; do
  case $1 in
    --only)
      [ $# -ge 2 ] || { echo "lang-diff: --only needs a glob" >&2; exit 2; }
      ONLY=$2; shift 2 ;;
    *) rest+=("$1"); shift ;;
  esac
done
# The +expansion guard is load-bearing under `set -u`: bash 3.2, which is what
# /bin/bash is on darwin, errors on "${rest[@]}" when the array is empty, and
# an empty array is exactly the no-arguments case that must reach usage().
set -- ${rest[@]+"${rest[@]}"}

[ $# -ge 2 ] || usage
NIXBINDIR=$1; shift
SELF_DIFF=0
if [ "$1" = --self-diff ]; then
  SELF_DIFF=1; ARM_A=none; ARM_B=none
else
  [ $# -eq 2 ] || usage
  ARM_A=$1; ARM_B=$2
fi

NIX_INSTANTIATE=$NIXBINDIR/nix-instantiate
NIX=$NIXBINDIR/nix
for b in "$NIX_INSTANTIATE" "$NIX"; do
  [ -x "$b" ] || { echo "lang-diff: not executable: $b" >&2; exit 2; }
done

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root/tests/functional" || exit 2
allowlist=$repo_root/maintainers/ix/eval-allowlist.toml

# sha256 of the binary so a report can never be attributed to the wrong build
# (shasum is darwin, sha256sum is coreutils; fail loudly rather than skip).
bin_sha() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
  else echo "lang-diff: no sha256 tool on PATH" >&2; exit 2; fi
}

arm_config() { # ARM -> NIX_CONFIG contents on stdout ("" for none)
  case $1 in
    none) ;;
    eval-backend=rust) printf 'extra-experimental-features = rust-eval\neval-backend = rust\n' ;;
    *=*) printf '%s = %s\n' "${1%%=*}" "${1#*=}" ;;
    *) echo "lang-diff: bad arm spec '$1' (want SETTING=VALUE or none)" >&2; exit 2 ;;
  esac
}

# Hashed before the first pair rather than after the last, and checked again
# at the end: a rebuild landing mid-run silently swaps the binary under the
# loop, and a hash taken afterwards then attributes every earlier pair to a
# build that never ran them. Found by doing exactly that.
BIN_SHA=$(bin_sha "$NIX_INSTANTIATE")

CONFIG_A=$(arm_config "$ARM_A")
CONFIG_B=$(arm_config "$ARM_B")

# Arms-really-differ gate. Waived for self-diff (identical arms are the point)
# and for arms naming different settings (nothing comparable to probe).
if [ "$SELF_DIFF" = 0 ] && [ "${ARM_A%%=*}" = "${ARM_B%%=*}" ] && [ "$ARM_A" != none ]; then
  setting=${ARM_A%%=*}
  eff_a=$(NIX_CONFIG=$CONFIG_A "$NIX" config show "$setting" 2>&1) || {
    echo "lang-diff: cannot read effective '$setting' under arm A; refusing a run whose arms cannot be told apart:" >&2
    echo "$eff_a" >&2; exit 2; }
  eff_b=$(NIX_CONFIG=$CONFIG_B "$NIX" config show "$setting" 2>&1) || {
    echo "lang-diff: cannot read effective '$setting' under arm B:" >&2
    echo "$eff_b" >&2; exit 2; }
  if [ "$eff_a" = "$eff_b" ]; then
    echo "lang-diff: arms are indistinguishable (effective $setting='$eff_a' in both); a pass would mean nothing" >&2
    exit 2
  fi
fi

selected() { # test name -> 0 when --only admits it (everything, when unset)
  [ -n "$ONLY" ] || return 0
  # shellcheck disable=SC2254 # $ONLY is a glob on purpose, not a literal
  case $1 in $ONLY) return 0 ;; *) return 1 ;; esac
}

# Capability probe: the arms-differ gate reads `nix config show`, which
# reports the SETTING and so passes even on a binary compiled without the
# rust backend (-Dnix:rust-eval=disabled is the default); such a run scored
# mismatch=249 while measuring a stub. Each arm must actually evaluate a
# trivial expression before the corpus counts anything.
for arm_probe in "$CONFIG_A" "$CONFIG_B"; do
  got=$(NIX_CONFIG=$arm_probe "$NIX_INSTANTIATE" --eval --strict -E 1 2>&1)
  if [ "$got" != 1 ]; then
    echo "lang-diff: arm cannot evaluate the probe expression '1'; refusing to score a corpus against it:" >&2
    echo "$got" >&2
    exit 2
  fi
done

allowlisted() { # test name -> 0 if listed
  [ -f "$allowlist" ] && grep -q "^id = \"$1\"" "$allowlist"
}

# The last "error: …" line, unindented, with ANSI stripped. This is the error
# itself; everything above it is cppnix trace decoration. Colour is stripped
# because nix writes it whenever the stream looks like a terminal, and a
# pattern matched against escaped text silently matches nothing.
last_error() {
  # LC_ALL=C so sed treats the stream as bytes: an error message can carry
  # invalid UTF-8 (eval-fail-toJSON-non-utf-8 does), and BSD sed aborts on it
  # under a UTF-8 locale rather than passing it through.
  LC_ALL=C sed -e 's/\x1b\[[0-9;]*m//g' "$1" \
    | LC_ALL=C grep -a -E '^[[:space:]]*error: ' | tail -1 | LC_ALL=C sed -e 's/^[[:space:]]*//'
}

# Every grep here runs `LC_ALL=C grep -a`, and both halves are load-bearing.
# An error message can carry bytes that are not text: eval-fail-string-nul-*
# puts a literal NUL in the message and eval-fail-toJSON-non-utf-8 puts
# invalid UTF-8 there. Without -a, grep calls the stream binary and prints no
# matching line (while still reporting a count, so -c looks fine); without
# LC_ALL=C, sed aborts on the invalid sequence. Either way every pattern comes
# back false and the pair lands in `unknown` for a reason that has nothing to
# do with the error -- which is how these two pairs stayed mismatched through
# three rounds of fixing the messages themselves.
error_class() { # stderr file -> class token
  local f=$1
  if LC_ALL=C grep -aq 'rust-eval unimplemented' "$f"; then echo unimplemented
  elif LC_ALL=C grep -aq 'rust-eval parse error' "$f"; then echo parse
  elif LC_ALL=C grep -aq 'syntax error' "$f"; then echo parse
  # cppnix raises these from the parser too, without the word "syntax".
  elif LC_ALL=C grep -aqE 'dynamic attributes not allowed in let|attribute .* already defined' "$f"; then echo parse
  # cppnix throws this one from the parser, so it is a parse-class failure
  # even though the message never says "syntax".
  elif LC_ALL=C grep -aq 'path has a trailing slash' "$f"; then echo parse
  elif LC_ALL=C grep -aq 'undefined variable' "$f"; then echo undefined-variable
  elif LC_ALL=C grep -aq 'infinite recursion encountered' "$f"; then echo infinite-recursion
  elif LC_ALL=C grep -aq 'stack overflow' "$f"; then echo stack-overflow
  elif LC_ALL=C grep -aqE "attribute '[^']*' missing" "$f"; then echo missing-attr
  elif LC_ALL=C grep -aqE 'assertion( .*)? failed' "$f"; then echo assert
  elif LC_ALL=C grep -aqE "is not equal to|differs from attribute set|is contained in '.*', but not in|is missing in '.*', but is contained in|immediate comparisons of identical functions compare as unequal" "$f"; then echo assert
  elif LC_ALL=C grep -aq 'evaluation aborted' "$f"; then echo abort
  elif LC_ALL=C grep -aq "'throw' builtin" "$f"; then echo throw
  elif LC_ALL=C grep -aqE 'expected a [a-z ]+ but found|cannot coerce|is a [a-z]+ while a [a-z]+ was expected|cannot compare|has no attribute|called with unexpected argument|called without required argument|cannot convert' "$f"; then echo type
  else echo unknown; fi
}

run_arm() { # config file flags... ; stdout->$out stderr->$err, returns exit code
  local config=$1 outf=$2 errf=$3; shift 3
  NIX_CONFIG=$config \
  NIX_CONF_DIR=$tmp/conf \
  NIX_USER_CONF_FILES='' \
  NIX_STATE_DIR=$tmp/state-nonexistent \
  NIX_PATH=lang/dir3:lang/dir4 \
  HOME=/fake-home \
  TEST_VAR=foo \
  NIX_REMOTE=dummy:// \
  NIX_STORE_DIR=/nix/store \
  timeout 60 "$NIX_INSTANTIATE" "$@" 1>"$outf" 2>"$errf"
}

# The runner isolates nix config and state the way common.sh does for
# lang.sh: without this, the machine's nix.conf and existing channel
# profiles leak two extra entries into builtins.nixPath and
# eval-okay-search-path fails on a count assertion. NIX_STATE_DIR points
# at a path that does not exist, on purpose.
tmp=$(mktemp -d /tmp/lang-diff.XXXXXX)
mkdir -p "$tmp/conf"
# Same grant as common/init.sh's test nix.conf: two lang tests parse flake
# refs and need the flakes feature. Arms still layer on top via NIX_CONFIG,
# which nix applies after conf files.
printf "experimental-features = nix-command flakes\n" > "$tmp/conf/nix.conf"
trap 'rm -rf "$tmp"' EXIT

pairs=0 corpus=0 match=0 failfail=0 mismatch=0 crash=0 unimpl=0 allow=0 corpusfail=0 skipped=0

report() { echo "$1: $2 $3"; }

for nixf in lang/eval-okay-*.nix; do
  name=$(basename "$nixf" .nix)
  corpus=$((corpus + 1))
  selected "$name" || continue
  pairs=$((pairs + 1))
  if [ -e "lang/$name.exp-disabled" ]; then skipped=$((skipped + 1)); continue; fi

  declare -a flags=()
  if [ -e "lang/$name.flags" ]; then read -r -a flags < "lang/$name.flags"; fi
  if [ -e "lang/$name.exp.xml" ]; then
    flags+=(--eval --xml --no-location --strict)
  else
    flags+=(--eval --strict)
  fi

  run_arm "$CONFIG_A" "$tmp/a.out" "$tmp/a.err" "${flags[@]}" "lang/$name.nix"; ec_a=$?
  run_arm "$CONFIG_B" "$tmp/b.out" "$tmp/b.err" "${flags[@]}" "lang/$name.nix"; ec_b=$?

  if [ "$ec_a" -ge 128 ] || [ "$ec_b" -ge 128 ]; then
    crash=$((crash + 1)); report CRASH "$name" "exit a=$ec_a b=$ec_b"; continue
  fi
  if [ "$ec_a" -ne 0 ]; then
    corpusfail=$((corpusfail + 1)); report CORPUS-FAIL "$name" "oracle arm failed (exit $ec_a)"; continue
  fi
  if grep -q 'rust-eval unimplemented' "$tmp/b.err"; then
    unimpl=$((unimpl + 1)); continue
  fi
  if [ "$ec_b" -eq 0 ] && cmp -s "$tmp/a.out" "$tmp/b.out"; then
    match=$((match + 1))
  elif allowlisted "$name"; then
    allow=$((allow + 1))
  else
    mismatch=$((mismatch + 1))
    report MISMATCH "$name" "exit a=$ec_a b=$ec_b; stdout $(cmp -s "$tmp/a.out" "$tmp/b.out" && echo equal || echo differs)"
  fi
done

for nixf in lang/eval-fail-*.nix; do
  name=$(basename "$nixf" .nix)
  corpus=$((corpus + 1))
  selected "$name" || continue
  pairs=$((pairs + 1))

  flags_str=""
  if [ -e "lang/$name.flags" ]; then
    flags_str=$(sed -e 's/#.*//' < "lang/$name.flags")
  else
    flags_str="--eval --strict --show-trace"
  fi
  # shellcheck disable=SC2086 # word splitting of flags is intended, as in lang.sh
  run_arm "$CONFIG_A" "$tmp/a.out" "$tmp/a.err" $flags_str "lang/$name.nix"; ec_a=$?
  # shellcheck disable=SC2086
  run_arm "$CONFIG_B" "$tmp/b.out" "$tmp/b.err" $flags_str "lang/$name.nix"; ec_b=$?

  if [ "$ec_a" -ge 128 ] || [ "$ec_b" -ge 128 ]; then
    crash=$((crash + 1)); report CRASH "$name" "exit a=$ec_a b=$ec_b"; continue
  fi
  if [ "$ec_a" -eq 0 ]; then
    corpusfail=$((corpusfail + 1)); report CORPUS-FAIL "$name" "oracle arm succeeded on an eval-fail case"; continue
  fi
  if grep -q 'rust-eval unimplemented' "$tmp/b.err"; then
    unimpl=$((unimpl + 1)); continue
  fi
  if [ "$ec_b" -eq 0 ]; then
    if allowlisted "$name"; then allow=$((allow + 1)); else
      mismatch=$((mismatch + 1)); report MISMATCH "$name" "arm B evaluated a must-fail case"
    fi
    continue
  fi
  class_a=$(error_class "$tmp/a.err"); class_b=$(error_class "$tmp/b.err")
  ok=0
  if [ "$class_a" = "$class_b" ]; then
    if [ "$class_a" = unknown ]; then
      ea=$(last_error "$tmp/a.err"); eb=$(last_error "$tmp/b.err")
      if [ -n "$ea" ] && [ "$ea" = "$eb" ]; then
        ok=1
      elif cmp -s "$tmp/a.err" "$tmp/b.err"; then
        # No terminal "error:" line to compare -- a message carrying invalid
        # UTF-8 has none, because stripping colour mangles it. Byte equality
        # is the only remaining evidence, and it is what --self-diff needs:
        # identical arms must match, and this pair is why. Found by the
        # self-diff smoke test, which is the whole reason it exists.
        ok=1
      fi
    else
      ok=1
    fi
  fi
  if [ "$ok" = 1 ]; then
    failfail=$((failfail + 1))
  elif allowlisted "$name"; then
    allow=$((allow + 1))
  else
    mismatch=$((mismatch + 1)); report MISMATCH "$name" "error class a=$class_a b=$class_b"
  fi
done

ver=$("$NIX_INSTANTIATE" --version | head -1)
sha_after=$(bin_sha "$NIX_INSTANTIATE")
if [ "$sha_after" != "$BIN_SHA" ]; then
  echo "lang-diff: the binary changed during the run ($BIN_SHA -> $sha_after); these counts mix two builds and mean nothing" >&2
  exit 2
fi
echo "RESULT lang-diff bin=$NIX_INSTANTIATE sha256=$BIN_SHA version='$ver' armA=$ARM_A armB=$ARM_B \
only='$ONLY' pairs=$pairs corpus=$corpus match=$match fail-as-fail=$failfail mismatch=$mismatch crash=$crash unimplemented=$unimpl allowlisted=$allow corpus-fail=$corpusfail skipped=$skipped"

if [ "$pairs" -eq 0 ]; then
  if [ -n "$ONLY" ]; then
    echo "lang-diff: --only '$ONLY' selected none of the $corpus pairs; a filter that matched nothing is a failure, not a pass" >&2
  else
    echo "lang-diff: zero pairs discovered; an empty corpus is a failure, not a pass" >&2
  fi
  exit 2
fi
[ "$mismatch" -eq 0 ] && [ "$crash" -eq 0 ] && [ "$corpusfail" -eq 0 ]
