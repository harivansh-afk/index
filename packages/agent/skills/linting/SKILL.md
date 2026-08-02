---
name: linting
description: "Which lint command each repo has -- index has `nix run .#lint`, ix has `just lint` -- and why an ambient ruff or clippy is not a substitute for either. Use before committing or when a lint check fails."
---

## There is no universal lint command; check the repo first

```sh
git remote get-url origin
```

| origin | lint the whole tree |
|--------|---------------------|
| `indexable-inc/index` | `nix run .#lint` |
| `indexable-inc/ix` | `just lint` |

`ix` vendors index's history under `ix/index/`, so one checkout holds both. The
origin is what decides, not the directory you started in: inside `ix/index/` you
are in an ix checkout, and `just lint` is the command for the whole thing.

Neither command is platform-gated. Both run on darwin and Linux.

Do not reach for the other repo's spelling when yours errors, and do not add a
wrapper so the two match. ix has no `nix run .#lint` on purpose:
`agent-context/sections/40-nix-stack.md` forbids Nix apps that invoke `nix`
internally, and `just lint` is `nix build -L --no-link .#ci-lint-checks` -- the
same derivation CI's lint phase builds, resolved to the running system.

That difference is worth stating this loudly because of what hiding it cost.
This skill used to print `nix run .#lint` as though it were universal. Nine
agents ran it in ix, got `does not provide attribute`, correctly concluded lint
was unavailable to them, and each filed the same ticket (ENG-9808 and eight
duplicates). The command had never existed in ix on any platform, so every one
of those reports diagnosed a darwin gap that was not there.

## Neither repo runs any git hook, so nothing lints for you

There is no `.githooks/` and no `.pre-commit-config.yaml` in either tree, and
nothing is installed into `.git/hooks`. Cloning is the whole setup. index's last
hook went on 2026-07-19 (the `drop-direnv-and-hooks` site update) and ix's three
went in ENG-11624, once every check they ran had a CI gate that was the same or
wider.

So a clean commit and a clean push say nothing about lint. Run the command in the
table above before you push, or read the verdict off CI after you do. Those are
the only two ways to know.

Do not install a hook to get the check back. `pre-commit install` in either repo
refuses with `No .pre-commit-config.yaml file was found`, and writing one is how
this went wrong the first time: ix had a config for its whole life that
`core.hooksPath` made unreachable, so the `no-raw-config-in-nix` rule listed in
it had never run on a single commit. A gate reporting nothing is indistinguishable
from a gate reporting clean.

## Run every gate through nix, never an ambient tool

A pinned tool and the one on your PATH are different tools. The flake fixes
versions and flags; ambient binaries do not, so a check that passes ad-hoc can
still fail the gate and vice versa. In index, ruff is the everyday example: the
flake pins it and passes a fixed `--target-version`, so version-gated rules
(`UP041`, `PERF203`) fire in one and not the other.

Rust is where the gap is widest and fails most quietly. Both repos build Rust
against a jj megamerge fork of clippy carrying eleven restriction lints that
stock clippy has never heard of (`packages/llm-clippy`, `lib/fork-packages.nix`
in index; ix reaches the same fork through its per-unit clippy graphs in
`lib/workspace-cargo-unit.nix`).

An unknown lint is not what breaks. Measured against stock clippy 0.1.97 on
2026-08-02, a fork-only lint name produces `warning[E0602]: unknown lint`, exit
0, and every real finding still reported:

```
$ cargo clippy -- -D clippy::anonymous_tuple_return_type   # fork-only name
warning[E0602]: unknown lint: `clippy::anonymous_tuple_return_type`
  = note: `#[warn(unknown_lints)]` on by default
warning: the loop variable `i` is only used to index `v`    # the real finding, still there
$ echo $?
0
```

The same holds through `[lints.clippy]` in Cargo.toml, which is how both
workspaces declare them. Under `-D warnings` the run does fail, exit 101, but
loudly rather than silently. So an ambient clippy still misses the eleven
fork-only lints, which is reason enough to use the pinned driver, but it does
not go blind.

**An unknown `clippy.toml` key is what breaks, and it breaks silently.** Clippy
reads the config before it lints anything, so one unrecognised field aborts the
run with no findings at all:

```
$ cat clippy.toml
max-fn-line-count = 100
$ cargo clippy
error: error reading Clippy's configuration file: unknown field `max-fn-line-count`,
       expected one of absolute-paths-allowed-crates, ...
$ echo $?
101
```

Zero findings, and a grep for findings sees exactly what a clean crate looks
like. That is the shape worth fearing, and `clippy.toml` is resolved from the
crate directory upward, so a single fork-only key at the repo root silences
every crate under it.

ix's committed `clippy.toml` is clean today: 16 keys, all of which stock clippy
0.1.97 accepts. The hazard is latent, and it arms the moment anyone adds a key
the fork understands and stock clippy does not.

So check the exit code, never the absence of a string in a log. `grep -c error`
returning zero is not a pass when the exit code was 101.

A `clippy.toml` key must land in the same change that ships its lint in the
fork, never ahead of it. A forward declaration, the key added first so the lint
has its config waiting, aborts clippy for every crate in the workspace until the
fork catches up.

Measured deliberately on 2026-08-02, on branch `obs/eng-11832-primitives`, by an
agent who added `instrumenting-methods` ahead of its lint, checked what it did,
and removed it before the change went anywhere near review:

    with the key    4 crates failed under clippy purely from the config error
                    (codec-macro, ix-async-utils, ix-landlock, ix-systemd)
    key removed     1 failure, pre-existing and unrelated (ix-systemd)

Nothing reached main, so read that as a branch experiment rather than an averted
outage: what it establishes is the mechanism and the collateral count, on this
repository, at that revision.

## Reading a failed build: three traps that compose

The excerpt nix prints on failure is a fixed-length tail, so a chatty epilogue
pushes the real diagnostic out of it. On a failing clippy gate on 2026-08-02 the
last twelve lines of that excerpt were `+ return 0` shell-hook noise and the
path to `nix log`, with the diagnostic surviving only higher up. Where the tail
is all epilogue, `nix log <drv>` is the only way to see anything.

Then the second trap fires on what `nix log` gives you. The stored log keeps the
builder's ANSI colour codes, even though the live `nix build` output on a pipe
has none, so an anchored match against it silently finds nothing. Same
derivation, same failure, both counts measured:

```
                     escapes   grep -c '^error'  after stripping
nix log <drv>             24                  0                2
nix build stderr           0                  2                2
```

Zero from the first row reads exactly like a clean build. Strip the escapes
before matching:

```sh
nix log <drv> | sed 's/\x1b\[[0-9;]*m//g' | grep -n '^error'
```

Then the third trap fires on *which* log you read. The derivation named in the
top-level `error: Cannot build '...'` line is the **wrapper**; the one that
actually failed has a different hash and appears earlier in the output. So
`nix log` on the obvious path returns a **successful** log, which reads as the
error having gone away between runs. On 2026-08-02 the failing gate printed
`ibqqmwv9...-ix_tracing-0.1.0.drv` at the bottom while the diagnostics lived in
`32jzbaya...-ix_tracing-clippy-0.1.0.drv`. Take the path from the *inner*
`Cannot build` line.

In **ix**:

```sh
nix build .#legacyPackages.x86_64-linux.rustClippyChecksByPackage.<crate> --no-link > gate.log 2>&1
drv=$(rg -o "Cannot build '/nix/store/[a-z0-9]+-[a-z_]+-clippy-0.1.0.drv" gate.log \
      | sed "s/Cannot build '//" | head -1)
nix log "$drv" | sed 's/\x1b\[[0-9;]*m//g' | rg -n '^error'
```

Together they are how a failing build gets looked at three times and reported
clean: the excerpt shows only hook noise, so you reach for `nix log`; the path
in front of you is the wrapper, so the log says success; and if you do find the
right one, the grep on its coloured output returns zero.

### And an empty run is the good case; the bad one is a full one

On 2026-08-01 the same command produced the opposite shape, which is worse. An
agent ran `cargo clippy -p ix2nix -- -D clippy::doc_markdown` to confirm a CI
clippy failure was fixed. Stock clippy raised `E0602` for both fork lints, as
above, **and then emitted fourteen `doc_markdown` findings anyway**, in
`lib.rs`, `ty.rs`, `map.rs`, `checker.rs` and `schema.rs`.

The crate's CI clippy check was green before that day's change, so the forked
driver flags none of those fourteen. They are the stock driver's findings for a
lint both drivers implement and disagree about.

So the abort does not reliably leave you with nothing. It can leave you with a
plausible, non-empty, entirely irrelevant report, and a report is far more
convincing than silence. An agent reading it would either chase fourteen
phantom defects or, having fixed the real one, conclude from the remaining
thirteen that the fix had failed.

The rule that survives both shapes: a stock clippy run is not evidence about
this repo's clippy, whatever it prints. Only the forked driver or the per-crate
gate answers the question.

In index, put the pinned driver on PATH directly:

```sh
nix build .#llm-clippy --no-link --print-out-paths   # then PATH=<out>/bin:$PATH
```

In **ix** there is no such output, and `just lint` does not run clippy there at
all (clippy runs in the `rust` CI phase, per cargo-unit crate). Build the
per-crate check, which carries the forked driver:

```sh
nix build .#legacyPackages.x86_64-linux.rustClippyChecksByPackage.<cargo-package-name>
```

keyed by the crate's `[package] name`. Never substitute a bare `cargo clippy`
for it -- that is the abort described above.

## The gate does not see files git does not

The nix source set is built from the tracked file list, so a **new file you have
not `git add`ed does not exist** as far as the gate is concerned. The failure
does not say that. On 2026-08-02 a new module produced:

```
error[E0583]: file not found for module `rpc_aggregate`
```

which reads as a broken `mod` declaration or a misplaced file, and is a
`git add`. It costs a full gate cycle to find, and the same shape hits any new
file a check consumes: a fixture, a snapshot, a `.nix` module.

`git add -A` before running any nix-driven gate on work in progress. The check
is `git status --short` showing no `??` entries under the paths the gate reads.

## Two lints can be jointly unsatisfiable without an `#[expect]`

`fallible_int_fallback` (fork-only) denies `.unwrap_or` on a
`Result<_, TryFromIntError>`. Write the clamp as a `match` instead and stock
clippy's `manual_unwrap_or` (pedantic, denied here) rewrites it straight back:

```
error: this pattern reimplements `Result::unwrap_or`
   help: replace with: `i64::try_from(micros).unwrap_or(i64::MAX)`
```

There is no third spelling that satisfies both: an `as` cast trips
`cast_possible_truncation` / `cast_sign_loss`, which are denied too. A genuine
clamp is the case `fallible_int_fallback`'s own documentation carves out, so
take it explicitly — and it must be `#[expect]`, not `#[allow]`, because
`allow_attributes` is denied:

```rust
#[expect(
    clippy::fallible_int_fallback,
    reason = "deliberate clamp: OpenTelemetry has no unsigned integer, and a microsecond \
              count above i64::MAX is ~292,000 years, so this cannot lose a real value"
)]
fn as_otel_int(micros: u64) -> i64 {
    i64::try_from(micros).unwrap_or(i64::MAX)
}
```

Reach for this only where the fallback provably cannot lose data, and say why in
the reason. Where it can, propagate the error instead — that is what the lint is
for.

## A green `nix run .#lint` does not mean the clone gate is green

The lint app runs the clone detector's GLOBAL gate (whole-tree duplication under
`[budget] global_pct`) and NOT its DIFF gate (duplication over the lines this
branch changed, budget `0%`). The exclusion is deliberate: the diff gate needs a
`.git` directory to resolve the merge base, and the CI lint derivation is handed
a `.git`-less source tree. It is also invisible, because a lint run says nothing
about the gate it did not run.

So `flake-check` can fail on duplication minutes after 15/15 stages passed
locally. Reproduce it before pushing.

In **index**:

```sh
nix run .#clone -- . --diff origin/main --pretty
```

`--pretty` is what makes the result actionable. The JSON names each clone
instance's `fragments` with file and line ranges, which turns "33/793 changed
lines duplicated" into "you copied `render_table` out of `drift.rs`". Without it
you get a percentage and no location.

Both gates are ratchets, so the fix is to delete the duplication rather than to
raise a budget: extract the shared thing and confirm whole-tree duplication went
DOWN. On index#4497 that was a copied markdown-table renderer plus a fifth copy
of an enum-to-string `match`; factoring them into one module took the tree from
0.2300% to 0.2246% and the diff gate to 0/865.

## index only: --json and --fix


Both are flags of index's lint app, which ix has no equivalent of. `just lint`
forwards its arguments to `nix build`, so `just lint --json` is nix's `--json`
and means something else entirely.

`nix run .#lint -- --json` prints one JSON array with a record per check --
`{check, ok, output}`, where `output` carries the stage's diagnostics on
failure -- and still exits nonzero when any check fails. Use it to load results
as a dataframe rather than grepping the log.

`nix run .#lint -- --fix` auto-applies the fixable findings (alejandra, statix
and deadnix over `.nix`, `ruff --fix` over `.py`): it builds the fixer lanes as
derivations and `git apply`s the resulting patch to the worktree. Commit your
work first -- Nix snapshots the committed state in a linked worktree, and
`git apply` refuses on context mismatch instead of clobbering uncommitted
edits. Verdict-only stages (astlog, filenames, dirnames, clone) and unfixable
findings still need hand edits; re-run the lint afterwards.
