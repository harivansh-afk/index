# How a change is validated in this fork

Hosted CI is gone. A change is validated by running the full test suite on a
fleet dev node, and the person making the change is the one who runs it.

This replaced GitHub Actions on 2026-08-03 by operator instruction. The workflows
removed in that commit were `.github/workflows/ci.yml`, `labels.yml` and
`backport.yml`, plus the now-orphaned `.github/labeler.yml`.

## What to run

On any dev node (`dc1` through `dc6`), in a checkout of the revision under test:

```console
$ nix develop --command bash -c 'meson setup build --prefix="$out" --buildtype=debugoptimized && ninja -C build && meson test -C build --print-errorlogs'
```

`configurePhase` and `buildPhase` from [HACKING.md](../../HACKING.md) are shell
functions defined by the dev shell's `shellHook`, so they exist only in an
interactive shell and are absent from `nix develop --command bash -c`. Call meson
and ninja directly, as above. Note `--prefix="$out"`, not `"$prefix"`: the dev
shell exports `out` but not `prefix`, and an empty prefix silently puts the
system `nix` on `$PATH` instead of the one you just built.

Also run, before committing:

```console
$ nix develop --command bash -c 'pre-commit run --all-files'
```

`--all-files` matters. The git hook runs against staged files only, so a
contributor whose own files are clean sees green while the tree is red. That is
how `ix-patched` came to sit with a red gate through two merges before anyone
noticed.

## What that costs, measured

On dev-compute-5 (AMD EPYC 9135, 32 threads), on 2026-08-03:

| step | time |
|---|---|
| `ninja -C build` from cold | `real 2m36.421s`, `user 71m0.661s` |
| `meson test -C build`, whole suite | `real 0m34.032s`, `user 2m35.565s` |

The suite reported **286 Ok, 0 Fail, 11 Skipped**. Those are the numbers to
compare against: a run that reports meaningfully fewer than 286 passing tests has
skipped something rather than proved something, and is worth reading before
believing.

Meson caches aggressively, so a one-file edit rebuilds and re-runs in seconds.
The five-minute feedback loop that hosted CI imposed is the thing this replaces.

## What is no longer covered

Two things were hosted-only and have no replacement:

- **Windows unit tests.** The `windows unit tests` job cross-built and ran the
  unit tests on a hosted runner. Nothing on the fleet does this. The last hosted
  run of it, on PR #40, passed. There is no local equivalent, so a change that
  breaks the Windows build will not be caught until somebody builds for Windows
  deliberately.
- **Darwin.** This is not a new loss. The fork already carried no macOS job, and
  the `ci.yml` comment removed with it recorded why: the job was cancelled at
  exactly 60:00 on four consecutive runs, so it had produced no completed verdict
  in days while occupying a required-looking slot. Darwin assurance here was
  already manual, and the standing record is a hand-run smoke of the built client
  against a live 2.34.7 daemon (`nix store ping --store daemon` plus a build
  through it, both exit 0), on indexable-inc/index#4483. That remains the
  template for anyone touching a Darwin-relevant path.

Also gone, and worth knowing rather than rediscovering:

- **CodeQL still runs.** It has no workflow file in this repo; it is GitHub's
  default setup, configured in repository settings, so removing the workflows did
  not remove it. Turning it off, if that is wanted, is a settings change and not
  a commit.
- **`upload-release.yml` was kept.** It is `workflow_dispatch` only and uploads a
  Hydra release rather than testing anything, and
  [release-process.md](./../release-process.md) still instructs you to trigger it.
  `.github/actions/install-nix-action` was kept for the same reason: that workflow
  uses it.

## Recording a validation run

When a change lands, say which node you ran on, which revision you tested, and
the counts, on the same line. "The suite passed" is not a record; "286 Ok, 0
Fail, 11 Skipped on 46d0769b5, dev-compute-5" is, because the next person can
tell whether your run and theirs saw the same suite.

## Measuring what write-through publication costs

[`write-through-throughput.sh`](./write-through-throughput.sh) measures the
publication cost of the `write-through-store` setting, against any destination
you point it at, in a scratch store root that leaves the host's own store alone.

```console
$ ./write-through-throughput.sh --to "file:///tmp/wt-bench-cache" --sizes 1,64,256 --reps 5
```

This exists because publication runs on the build worker thread and blocks the
loop, so a host with the setting on has its build concurrency bounded by
publication throughput rather than by `--max-jobs`. That bound cannot be read off
a derivation count, and the sizing a dispatcher was given under the old
asynchronous queue does not carry over.

Three tiers, in increasing order of what they actually tell you:

1. **A `file://` destination** measures nix-copy protocol overhead with the wire
   taken out. Runs anywhere, needs nothing.
2. **A scratch endpoint on a storage host** measures the wire and disk floor over
   the real path. Needs a node and a scratch endpoint, never a production
   namespace.
3. **The real cache** is the only number that answers the sizing question, and it
   can only be taken on a host that holds the push credential
   (`nixCachePushCredentialFile`, which today is vin-compute-1's inventory
   extras). A dev node cannot authenticate against it, so tiers 1 and 2 are not
   an approximation of tier 3, they are different measurements.

The harness refuses to report a number when the path did not reach the
destination, because a publication that silently did not happen would otherwise
read as enormous throughput. It also gives every build a fresh run id, so no
repetition can be served by a path the destination already had.

### Set `compression` on the destination, or nothing else you tune will matter

Measured on dev-compute-5 (2026-08-04), publishing a 256 MiB incompressible
output to a local `file://` cache, so no network is involved at all:

| destination | publication rate |
|---|---|
| `file:///...` (no parameter, so **xz**) | **2.6 MiB/s** |
| `file:///...?compression=zstd` | 579 MiB/s |
| `file:///...?compression=none` | 621 MiB/s |

That is a 238x spread, and the default is the slow end. The xz run spent 100
seconds of build-worker time to produce a file *larger* than its input
(`FileSize: 268449088` against `NarSize: 268435736`), because xz cannot compress
random bytes but still pays to try.

The payload is deliberately incompressible, which is xz's worst case; real build
outputs compress, so xz would both run faster and actually shrink them. The
ordering does not change. Single-threaded xz is one to ten MiB/s on compressible
input too, still two orders of magnitude under the alternatives, and it is
running on the thread that the build loop is waiting on.

`nixCacheWriteStoreUrl` already carries `?compression=none&parallel-compression=false`,
so the configured path is on the fast end. The hazard is a destination URL written
by hand without those parameters: it does not fail, it just publishes at a few
MiB/s, and a 1 GiB output blocks the build worker for several minutes in a way
that looks like a hang rather than a misconfiguration.

### The transport, measured

Same date, dc5 (192.168.0.9) to hil-stor-2 (192.168.0.8), both `bond-vrack` at
50000 Mb/s, 0.2 ms RTT, 4 GiB of incompressible payload staged in RAM on both
ends so neither source disk nor `/dev/urandom` is in the measurement:

| path | rate |
|---|---|
| hil-stor-2 local write, `/dev/shm` to `dpool` (no wire) | 1349 MB/s |
| dc5 to hil-stor-2, ssh `chacha20-poly1305`, into `dpool`, fsync included | 292 MB/s |
| dc5 to hil-stor-2, ssh `aes256-gcm`, into `dpool`, fsync included | 524 MB/s |
| the same, 4 parallel streams | 805 MB/s aggregate |
| the link itself | 6250 MB/s |

The destination dataset is `dpool/scratch-storewt-throughput` (zstd, recordsize
128K, sync=standard), removed afterwards.

Neither the wire nor the disk is the constraint. A single ssh stream is, and
publication is a single stream on one thread, so the transport ceiling for an
ssh-based destination is the 292 MB/s figure with the default cipher. hil-stor-2's
sshd offers only `chacha20-poly1305` and `aes256-gcm`, and chacha is the one that
gets negotiated by default despite being the slower of the two on these EPYC
parts, which have AES-NI.

None of this is the real-cache number. The real cache is S3 to Garage over TLS,
not ssh, and the push credential lives only on vin-compute-1.
