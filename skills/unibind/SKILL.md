---
name: unibind
description: "How ix generates its SDKs: unibind lowers one Rust surface into TypeScript, Python, JVM and Elixir bindings, and the rule is that the IR carries intent while each backend renders its own language's idiom. Use when changing the SDK surface (crates/ix/sdk-bind), adding or fixing a unibind feature, or when a generated API reads unidiomatically in any language."
---

## The rule

One Rust surface, four idiomatic SDKs. Not four transliterations of one.

The IR carries **intent**: this constructs the object, this owns something that
must be released, this is a stream, this is a closed set of variants. Each
backend renders its own language's idiom for that intent. A shape that only one
language has never reaches the IR.

The failure mode this exists to prevent is lowest-common-denominator design.
When the IR could only say "free function", "method with a receiver" and "sync
constructor", every language got

    client.machines().create({ template: "docker.io/library/python:3.12" })

because that was the only sentence all four backends could speak. The idiomatic
call is `Machine.oci(...)` in all four, and it looks different in each: a static
method returning a promise, a static coroutine, a static method returning a
future, a module function returning a tagged tuple. One intent, four renderings.

Reviewing a generated surface, ask what a native library in that language would
look like, not whether the shape matches the Rust. A `.d.ts` that reads like
Rust with braces is a defect, and so is a `.pyi` that reads like TypeScript.

## Intent table

| Intent | Rust | TypeScript | Python | JVM | Elixir |
|---|---|---|---|---|---|
| construct, async | associated `async fn` returning `Self` | `static oci(): Promise<Machine>` | `@staticmethod async def oci() -> Machine` | static method returning a future | `Machine.oci(...) -> {:ok, machine}` |
| release at scope end | `object(resource)` + `close` | `Symbol.asyncDispose`, so `await using` | `__aenter__`/`__aexit__`, so `async with` | `AutoCloseable`, so try-with-resources | nothing; the BEAM drops the `ResourceArc`, or take a function |
| closed variant set | `enum Status { Running, .. }` | `'running' \| 'stopped'` | `StrEnum` / `Literal` | `enum` | atoms |
| variants with data | `enum Frame { Phase{..}, Done{..} }` | discriminated union, exhaustive `switch` | frozen dataclasses under a `Literal` tag, `match` | sealed interface over records | tagged tuples, `case` |
| identity | `MachineId(Uuid)` | branded string | `NewType` | record wrapper | opaque type |
| sequence | `UniStream<T>` | `AsyncIterable` | `__aiter__`/`__anext__` | iterator | `Stream` |
| failure | error enum | error subclasses | exception hierarchy | checked exceptions | `{:error, reason}` |

Elixir keeps the table honest. It has no classes to hang a static method on and
no scope-bound cleanup at all, so any IR concept that survives contact with
Elixir is intent rather than syntax. When a new concept has no Elixir answer,
that is a signal the concept is a Node or Python fact wearing a general name.

## Types are the product

A generated type that says `string` where the value is one of ten known words
has moved the specification into a doc comment, where no compiler reads it. Two
rules follow.

**A closed set is an enum, never a string.** `status`, `kind`, `phase`, `level`
and every other field whose doc comment lists its legal values belongs in a Rust
enum, so each backend can render its own closed type.

**Variants that carry different data are one sum type, never one struct with
optional fields.** The anti-pattern in our own surface today is
`SwitchProgress`: a `kind: String` beside `phase`, `stdout`, `system` and
`error`, each set on a different frame, every one optional because none of them
is always there. The caller reconstructs by hand (`if (frame.finished &&
frame.system)`) what a tagged union would have decided at compile time. It is a
Rust enum flattened into a struct because the IR had nowhere to put it.

## State of the backends

Do not promise a surface a backend cannot render. As of 2026-08-04:

| | TypeScript | Python | JVM | Elixir |
|---|---|---|---|---|
| objects | yes | yes | **rejected outright** (`backend-jvm/src/module.rs`), no handle registry | yes, as `ResourceArc` handles |
| async | yes | yes | **rejected outright** (`backend-jvm/src/function.rs`) | free functions only; object members rejected (`backend-ex/src/object.rs`) |
| resource close | `close()` + leak warning | `close` + `__aenter__`/`__aexit__` + `ResourceWarning` | none | flag ignored; the BEAM drop runs `Drop` |
| `await using` / dispose | **not emitted**; napi-rs has no attribute for it, so it belongs in the generated JS wrapper | n/a (`async with` covers it) | none | n/a |
| data enums | **rejected** (`backend-ts/src/module.rs`) | **rejected** (`backend-py/src/module.rs`) | no | no |
| error enums | yes | yes | yes | yes |

So the shipped ix SDKs are TypeScript and Python. The JVM and Elixir backends
serve other unibind consumers, and their gaps are stated in their own rejection
messages, which are worth reading before designing around them.

## Adding an intent

The IR's function kind lives in lowering, not in the IR data: `Kind` in
`core/src/lower/func.rs` is `Free | Method | Constructor`, and an object carries
`constructor: Option<Function>` beside `methods: Vec<Function>`, so kind is
positional. A new intent is therefore a new arm there plus a new field on
`ir::Object`, and then one rendering decision per backend:

1. `core/src/lower/func.rs`, `core/src/lower/object.rs`: accept the shape, and
   say why in the rejection message for the shapes still refused.
2. `core/src/ir/data.rs`: carry it.
3. Each backend: render the local idiom, or refuse with a message naming the
   idiom it would have rendered. A backend that cannot do it yet refuses
   loudly; it never falls back to a shape from another language.
4. Each conformance suite (`conformance/`, `conformance-ts/`, `conformance-jvm/`,
   `conformance-ex/`): the fixture Rust surface plus assertions in the target
   language. A feature with no conformance test in a language is not supported
   in that language, whatever the renderer does.
5. Break it and watch it fail. A renderer's snapshot test passes just as well
   against the wrong output.

Anything that has to be hand-written on top of a generated surface is a bug
report against unibind, not a layer to grow. `packages/sdk/src/repl.ts` is the
one sanctioned exception, and it earns it by being protocol logic rather than a
second spelling of a generated verb.
