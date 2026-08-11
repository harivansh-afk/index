# What agents get wrong in this repo

Short claims first; each has a pointer. Verify at the pointer, not by grep
memory.

## There are two evaluators, and one of them is Rust

- `rust/nix-eval-rs` is a bytecode VM for the Nix language (fixed-width-op
  IR, compiler, explicit-frame lazy stack VM), linked into the `nix` binary
  behind `eval-backend = rust` (experimental feature; default is `cpp`).
  `rust/ix-kernel` is the memo-table kernel beside it. Workstream: ENG-12068;
  active branches `claude/eng-12068-*`.
- A grep for "bytecode" scoped to `src/libexpr` finds nothing and has already
  produced a confident "no bytecode VM exists" answer. Search `rust/` too.
- Equivalence between the backends is behavioral, gated by
  `maintainers/ix/lang-diff.sh` against `maintainers/ix/eval-allowlist.toml`
  (every accepted divergence needs a reason; semantic ones need a human name).

## Incremental eval is C++ and has named unsoundness

- `nix eval-persistent --retain` (`src/nix/eval-persistent.cc`) reuses a live
  evaluator across runs. The only accepted correctness evidence is comparison
  against a fresh process; the retained process agreeing with itself proves
  nothing.
- What invalidation reaches and what it misses is edit-class dependent.
  Read `maintainers/ix/read-set-recall.md` before claiming a recall number;
  the 22/22 figure is one edit class, not a general property.
- Direction, not yet built: the durable home for incrementality is the Rust
  VM, where code units are content-addressed CAS objects and `ix-kernel`'s
  memo table is the eval cache (one invalidation story). The C++ retained
  evaluator is the bridge, not the destination.

## Branch model: one branch, never rewritten

- `ix-patched` is the only branch that matters. Ordinary commits, one per
  patch, pushed directly. Upstream moves land as two-parent merges, never a
  rebase. Delta over upstream:
  `git log upstream/main..ix-patched --first-parent --no-merges`
  (both flags load-bearing).
- flake.locks in other repos pin these revs, so force-push is exceptional and
  needs a `refs/pins/<date>-<sha12>` ref for every pinned rev in the same
  operation.

## Merging and shipping

- This repo does NOT allow auto-merge: `gh pr merge --auto --merge` merges
  immediately, silently. Check before arming.
- A fix here reaches the fleet only after ix bumps its nix-src pin and
  deploys. "Merged" is not "running anywhere".

## Build loop

- `nix-dev-build` recompiles one edited file in 2-9s; a whole-package
  `nix build` recompiles the closure. `nix develop --command bash -c
  'configurePhase'` fails (stdenv shell functions undefined); call `meson
  setup` and `ninja` directly.
- A checkout build's `--version` carries no revision. Identify a measured
  binary by store path or file path, never by version string.

## Tests

- Baseline and recording rules are in `maintainers/ix/testing.md` (the lang
  suite baseline is 286 Ok; report counts on the same line as the claim).
