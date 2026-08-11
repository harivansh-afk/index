# Pure-eval micro benchmarks: the M2.5 go/no-go record

Five expressions both backends evaluate identically (verified before
timing), timed with hyperfine (1 warmup, 5 runs) on hydra
(aarch64-darwin), build sha256 1ae9e492c968 (ix-patched @ 044b6187e,
rust-eval enabled). Startup floor measured with `--eval -E 1` and
subtracted: cpp 203ms, rust 200ms.

| bench   | cpp work | rust work | rust/cpp |
|---------|---------:|----------:|---------:|
| fib     |    448ms |      91ms |    0.20x |
| fold    |     87ms |      47ms |    0.54x |
| attrs   |     37ms |     186ms |    5.03x |
| strings |     29ms |     274ms |    9.51x |
| sort    |     11ms |     679ms |   64.11x |

Verdict: GO. The prior art's central risk (tvix measured 5.7-11.4x
slower than cppnix overall) does not reproduce here: the explicit-frame
VM beats cppnix 5x on call-heavy and 2x on fold work. The three slow
paths are named mechanisms, not the architecture:

- sort: the deliberate O(n^2) insertion-sort stopgap in builtins2.rs
  (throwing comparators forced the simple shape); replace with a stable
  merge sort driven through the continuation machine.
- attrs: BTreeMap<Sym, Slot> plus per-name interner round trips versus
  cppnix's sorted arrays; the planned two-word value + sorted-vec
  representation work.
- strings: Rc<str> reallocation churn in toString/concat paths; a
  rope-or-builder path in ConcatStrings and coerce.

Regenerate: run the .nix files through both arms of one binary (the
capability probe rule applies; see lang-diff.sh), verify outputs agree
before timing anything, subtract the -E 1 floor per arm.
