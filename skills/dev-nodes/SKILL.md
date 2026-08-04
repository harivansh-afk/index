---
name: dev-nodes
description: "Decide whether a dev-compute box is free before taking it, and how to claim and release one. Five occupancy signals, each of which has been observed reporting free on a box that was not, and why the cheap-looking version of each is wrong. Use before deploying to, benchmarking on, or running anything long on dev-compute-1 through 6."
---

# Taking a dev node

`dev-compute-1` through `dev-compute-6` exist so a change is proven on one box
before it reaches the fleet. They are shared, nothing locks them, and the
convention below is the only thing between two people and one overwritten
experiment. ENG-9965 tracks real locking.

Announce the claim before you use a node, and say when you release it.

## No single signal tells you a box is free

Five have now each been observed reporting free on a box that was not. They
work as a conjunction, and anyone reaching for whichever is cheapest will take
somebody's node.

**Running root-owned jobs.** The primary signal, because they leave no trace in
anyone's home directory at all. Measured 2026-08-02: dev-compute-1 at load 2.4
with `golden` running 715s at 30% of a core, dev-compute-4 at 0.87 with
`nix-store` at 51%. Both live, neither attributable to a user.

**Top-level mtimes under `/home`. Do not descend.** A person working on a box
touches their own files before anything else shows it. Read the directory
mtimes only:

```sh
ls -l /home/
```

A recursive scan reports every box busy by the act of checking, because your own
ssh connection creates sockets under `~/.ssh/agent/` at the instant you connect.
Measured on dev-compute-1: connecting at 03:31:30 stamped `~/.ssh/agent/` and
its socket 03:31:30, while `/home/andrew` stayed at 2026-07-30T23:21. The
top-level mtime is not disturbed by the observer; anything below it is.

That is why this is the top-level form rather than a deep scan with
`~/.ssh/agent` excluded. An exclusion list is a guard that rots in silence: that
socket is the one anyone has found, and the next tool that writes under a home
directory on connect re-breaks the check with no error. Prefer the shape that
cannot be disturbed over the one that lists its known disturbances.

**Load trend, not the instantaneous number.** Read all three averages.
Measured 2026-08-02: dev-compute-6 was taken because 1/5/15 was falling
(0.44/1.14/0.89) while dev-compute-3's was rising (1.65/0.63/0.34). A single
figure called both wrong.

**Virtual machines and running units.** This is what caught someone's two
virtual machines on dev-compute-4 in an earlier session. It found nothing on
2026-08-02: no dev box had any (`/var/lib/ix/vms` absent on all six) and all six
carried the same 55 to 56 baseline fleet units.

**The generation stamp.** An old one proves nothing, since a box last switched
yesterday can be carrying an experiment that has run since. A recent one means
somebody deployed and may be mid-run.

`who` is not on the list and should not be counted. On 2026-08-02 it was empty
on all six boxes while three were occupied.

## Releasing is not the same statement as free

Announcing that your own claim has ended says your claim ended. It does not say
the box is unoccupied. On 2026-08-02 dev-compute-3 was reported released while
somebody else was working on it, and the next person to read that announcement
as "free" would have taken an occupied node.

## These signals decay, and one of them is a fence

Every measurement above is dated because every one of them is a fact about the
world at that moment, not a property of the fleet. Units get added, `/var/lib`
paths move, tools start writing to new places on connect. A reader in six
months should treat an undated signal here as unverified.

One of them is different and worth keeping even if the numbers rot. `ls -l
/home/` beats a deep scan for a structural reason rather than an empirical one:
the top-level mtime cannot be disturbed by the act of reading it, and no list of
exclusions is needed to keep it that way. If you find yourself adding an
exclusion to this page, that is the signal to ask whether a simpler check would
not have needed one.

## Prefer the quiet one, except for races

Among genuinely unclaimed nodes, take the least loaded. Race conditions are the
exception: they only appear on a busy host, so put those under load
deliberately rather than hoping the node supplies it.

## Nothing here is shipped by observability

The dev inventory sets `observability.enable = false`, so no dev-compute host
ships logs or metrics, and a question about one still needs ssh. Anything
verified only on a dev box is verified against a host the pipeline cannot see
(ENG-10011).
