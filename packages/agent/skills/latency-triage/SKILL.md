---
name: latency-triage
description: "Answer 'why was that slow' from traces already being emitted: the busy/idle split every ix span carries, pulling a trace id out of a command's own debug log, and why eBPF and on-CPU profiling cannot see a blocked await. Use when a command or an RPC took longer than expected, before adding instrumentation or reaching for a profiler."
---

## Start with busy versus idle, not with a profiler

Every ix span already carries `busy_ns` and `idle_ns`. A span's duration minus
its busy time is time it spent waiting, and that one split answers most latency
questions before any new instrumentation. It went unused for months because
nobody knew the attributes were there.

Get the trace id of a slow command from its own debug log. This works even when
OpenTelemetry export is broken, because the file layer is separate from the
exporter:

```sh
IX_DEBUG_FILE=/tmp/t.jsonl ix <command> --debug
grep -o '"trace_id":"[a-f0-9]*"' /tmp/t.jsonl | sort -u
```

Then split the trace on `clickhouse.ix.internal` (hil-compute-2):

```sql
SELECT SpanName, count() n,
       round(sum(Duration)/1e9, 2) dur_s,
       round(sum(toUInt64OrZero(SpanAttributes['busy_ns']))/1e9, 3) busy_s,
       round(sum(toUInt64OrZero(SpanAttributes['idle_ns']))/1e9, 2) idle_s
FROM otel.otel_traces WHERE TraceId = '<id>'
GROUP BY SpanName ORDER BY dur_s DESC
```

A root at 35.578s duration and 0.400s busy is not a slow service. It is a
service waiting on something. That distinction took a night to establish by
reading source, and it is one query.

## Cost hiding in uninstrumented code: do not grep for it, query it

`Duration - busy_ns - sum(children)` is time a span cannot account for, and it
finds gaps in code nobody thought to instrument. Do not rewrite the SQL: the
maintained copy is the `span-unexplained-time` KPI in
`nix/modules/services/observability/kpi/spec.nix`, with its `breakdownSql` as
the drill-down. Read the `why` field before changing its filters, because the
`children >= 10` clause looks arbitrary and is not.

## Two things not to reach for, and why

**eBPF (OBI or Beyla) cannot see the ix RPC path.** It instruments HTTP and
gRPC over TCP, recovering TLS through OpenSSL and Go uprobes. ix RPC is
WebTransport over QUIC via quinn plus rustls: no QUIC support, no rustls
support. It would show UDP byte counters and nothing else. Kernels are fine at
7.1.3; that is not the blocker.

**On-CPU profiling cannot see a blocked await, by construction.**
`profiles.samples` is `perf_event_open` on `PERF_COUNT_SW_CPU_CLOCK`. During a
35.6s stall it recorded 1.175 CPU-seconds and had nothing to say, because
waiting burns no CPU. Use the busy and idle split instead. Off-CPU profiling is
tracked separately.

## Check this page still works

Three of the four commands above depend on `busy_ns` existing. If it stops
being emitted they return zeros rather than erroring, so this page would go
quietly wrong rather than visibly stale:

```sql
SELECT countIf(mapContains(SpanAttributes, 'busy_ns')) with_busy, count() total
FROM otel.otel_traces WHERE Timestamp > now() - INTERVAL 1 HOUR
```

`with_busy` near zero means this page is dead and should be fixed or deleted,
not worked around. Last verified 2026-08-02: 408,197 of 408,853.

## A zero is scoped to what the query could see

The liveness check above is one case of a general one, and the general one bit
the author of this page while writing it. Two things make a clean-looking zero
mean less than it appears.

**Independence is a property of what the checks look at, not of the commands
being different.** Three tools agreeing raises confidence exactly as much as
three independent checks would, and it is worth nothing when they share a blind
spot. Searching for a KPI that turned out to live on an unmerged branch:

```
rg over the working tree      0 matches
gh pr list --search           none
gh search code                no results
```

Three tools, one observation, all scoped to main. The agreement is what made it
feel settled enough to act on.

**"No results" from a search tool is a claim about its index, not about the
world.** `gh search code` does not cover unmerged branch heads. That is a fact
about GitHub which the zero silently inherited and did not mention. Every search
tool has a corpus, no output states it, and the answer is scoped to it whether
or not the corpus was on your mind when you read the number.

So before a zero decides anything, say what would have to be true for the query
to have missed, and check that instead. Here it was one command:
`gh api ".../contents/<path>?ref=<branch>"`.

## Not yet covered

Joining a command to the server work it caused. The client's trace id does not
currently appear on server spans (ENG-11829); a line lands here when that is
proven end to end. Its absence is not a claim that propagation is impossible.
Canonical join-key spellings across `logs`, `metrics`, `kpi` and `otel` are
owned by ENG-11833 and belong in the same section when that lands.
