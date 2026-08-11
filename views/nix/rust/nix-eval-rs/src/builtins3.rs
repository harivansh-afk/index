//! Third builtins batch: the primops that need a worklist of their own or a
//! data format behind them -- `genericClosure`, JSON, TOML, regular
//! expressions, `hashString` -- plus the environment and attrset primops
//! batch two left out.
//!
//! Same contract as `builtins2`: a body is either pure over already-forced
//! arguments or hands back a continuation the machine steps, so nothing here
//! re-enters the interpreter and no walk costs host stack proportional to a
//! Nix value.

use crate::builtins2::{Begin, Cont, argv, want_attrs, want_list, want_str};
use crate::task::Yield;
use crate::value2::{Slot, Sym, Value, type_name};
use crate::vm::{Result, Vm, VmError};
use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

/// A continuation owned by this module. One `Cont` variant carries all of
/// them so batch two's enum does not grow a case per builtin added here.
pub enum Ext {
    Generic(Generic),
    ToJson(ToJson),
}

pub fn step(vm: &mut Vm, args: &[Slot], ext: &mut Ext, incoming: Option<Value>) -> Result<Yield> {
    match ext {
        Ext::Generic(g) => g.step(vm, args, incoming),
        Ext::ToJson(j) => j.step(vm, incoming),
    }
}

// -- genericClosure ---------------------------------------------------------

/// Where a `genericClosure` run is: which forced value the machine is about
/// to hand back.
enum Stage {
    StartSet,
    Operator,
    /// The element popped off the work queue.
    Elem,
    /// That element's `key` attribute.
    Key,
    /// What the operator returned.
    OpResult,
    /// One element of the operator's result. cppnix forces each before
    /// queueing it, so a throw there surfaces during the closure walk and
    /// not later when the element is inspected.
    OpElem,
}

/// A key already emitted, reduced to what cppnix's `CompareValues` looks at.
/// `Other` keeps the type name so a comparison against it can raise the
/// message cppnix raises, and `List` is a key shape cppnix compares
/// element-wise -- a walk that would have to run through the machine, so it
/// is reported rather than approximated.
enum Key {
    Int(i64),
    Float(f64),
    Str(String),
    Path(String),
    List,
    Other(&'static str),
}

impl Key {
    fn of(v: &Value) -> Key {
        match v {
            Value::Int(n) => Key::Int(*n),
            Value::Float(x) => Key::Float(*x),
            Value::Str(s) => Key::Str(s.to_string()),
            Value::Path(p) => Key::Path(p.to_string()),
            Value::List(_) => Key::List,
            other => Key::Other(type_name(other)),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Key::Int(_) => "an integer",
            Key::Float(_) => "a float",
            Key::Str(_) => "a string",
            Key::Path(_) => "a path",
            Key::List => "a list",
            Key::Other(t) => t,
        }
    }

    /// cppnix's `CompareValues`: int and float compare across the two, every
    /// other pair of differing types is an error, and a same-typed pair its
    /// switch has no case for ("values of that type are incomparable").
    fn cmp_nix(&self, other: &Key) -> Result<Ordering> {
        let cmp = |a: f64, b: f64| a.partial_cmp(&b).unwrap_or(Ordering::Equal);
        match (self, other) {
            (Key::Int(a), Key::Int(b)) => Ok(a.cmp(b)),
            (Key::Float(a), Key::Float(b)) => Ok(cmp(*a, *b)),
            (Key::Int(a), Key::Float(b)) => Ok(cmp(*a as f64, *b)),
            (Key::Float(a), Key::Int(b)) => Ok(cmp(*a, *b as f64)),
            (Key::Str(a), Key::Str(b)) => Ok(a.cmp(b)),
            (Key::Path(a), Key::Path(b)) => Ok(a.cmp(b)),
            (Key::List, _) | (_, Key::List) => Err(VmError::Unimplemented(
                "builtins.genericClosure with list keys".to_owned(),
            )),
            (a, b) if a.type_name() == b.type_name() => Err(VmError::eval(format!(
                "cannot compare {0} with {0}; values of that type are incomparable",
                a.type_name()
            ))),
            (a, b) => Err(VmError::eval(format!(
                "cannot compare {} with {}",
                a.type_name(),
                b.type_name()
            ))),
        }
    }
}

pub struct Generic {
    /// The `startSet` slot, held until the driver's first step: a `Begin::Cont`
    /// arrives with nothing forced yet, so the first thing this does is ask
    /// for it.
    start: Option<Slot>,
    stage: Stage,
    work: VecDeque<Slot>,
    res: Vec<Slot>,
    /// Emitted keys in `CompareValues` order. A sorted `Vec` searched by
    /// hand rather than a `BTreeMap`, because the comparison is fallible and
    /// `Ord` is not: cppnix gets the same behavior from `std::map`, whose
    /// comparator throws out of the insert.
    keys: Vec<Key>,
    op: Option<Value>,
    /// The element being processed, kept because its `key` is forced between
    /// popping it and emitting it.
    cur: Option<Slot>,
    /// The operator's result, forced element by element before queueing.
    pending: Vec<Slot>,
    pi: usize,
}

/// cppnix reads `startSet` and `operator` out of the one attrset argument,
/// and an empty `startSet` returns before `operator` is looked at all.
pub fn bi_generic_closure(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let attrs = want_attrs(&argv(args, 0)?)?;
    let sym = vm.intern("startSet");
    let start = attrs
        .get(&sym)
        .cloned()
        .ok_or_else(|| VmError::eval("attribute 'startSet' missing"))?;
    Ok(Begin::Cont(Cont::Ext(Ext::Generic(Generic {
        start: Some(start),
        stage: Stage::StartSet,
        work: VecDeque::new(),
        res: Vec::new(),
        keys: Vec::new(),
        op: None,
        cur: None,
        pending: Vec::new(),
        pi: 0,
    }))))
}

impl Generic {
    fn step(&mut self, vm: &mut Vm, args: &[Slot], incoming: Option<Value>) -> Result<Yield> {
        let Some(v) = incoming else {
            // The driver's first step into a fresh continuation, before
            // anything has been forced.
            let s = self
                .start
                .take()
                .ok_or_else(|| VmError::eval("internal: genericClosure restarted"))?;
            return Ok(Yield::Force(s));
        };
        match self.stage {
            Stage::StartSet => {
                let items = want_list(&v)?;
                // cppnix hands the startSet value straight back when it is
                // empty, which is why a missing `operator` is not an error
                // for an empty closure.
                if items.is_empty() {
                    return Ok(Yield::Done(v));
                }
                self.work.extend(items.iter().cloned());
                let attrs = want_attrs(&argv(args, 0)?)?;
                let sym = vm.intern("operator");
                let op = attrs
                    .get(&sym)
                    .cloned()
                    .ok_or_else(|| VmError::eval("attribute 'operator' missing"))?;
                self.stage = Stage::Operator;
                Ok(Yield::Force(op))
            }
            Stage::Operator => {
                if !matches!(v, Value::Closure(_) | Value::Builtin(_)) {
                    return Err(VmError::eval(format!(
                        "expected a function but found {}",
                        type_name(&v)
                    )));
                }
                self.op = Some(v);
                self.next_element()
            }
            Stage::Elem => {
                let attrs = want_attrs(&v)?;
                let sym = vm.intern("key");
                let key = attrs
                    .get(&sym)
                    .cloned()
                    .ok_or_else(|| VmError::eval("attribute 'key' missing"))?;
                self.stage = Stage::Key;
                Ok(Yield::Force(key))
            }
            Stage::Key => {
                if !insert_key(&mut self.keys, Key::of(&v))? {
                    // Already closed over: cppnix skips the element without
                    // calling the operator on it.
                    return self.next_element();
                }
                let cur = self
                    .cur
                    .clone()
                    .ok_or_else(|| VmError::eval("internal: genericClosure lost its element"))?;
                self.res.push(cur.clone());
                let op = self
                    .op
                    .clone()
                    .ok_or_else(|| VmError::eval("internal: genericClosure lost its operator"))?;
                self.stage = Stage::OpResult;
                Ok(Yield::Apply(op, cur))
            }
            Stage::OpResult => {
                self.pending = want_list(&v)?.iter().cloned().collect();
                self.pi = 0;
                self.force_pending()
            }
            Stage::OpElem => {
                self.pi += 1;
                self.force_pending()
            }
        }
    }

    fn next_element(&mut self) -> Result<Yield> {
        match self.work.pop_front() {
            Some(s) => {
                self.cur = Some(s.clone());
                self.stage = Stage::Elem;
                Ok(Yield::Force(s))
            }
            None => Ok(Yield::Done(Value::List(Rc::new(std::mem::take(
                &mut self.res,
            ))))),
        }
    }

    fn force_pending(&mut self) -> Result<Yield> {
        match self.pending.get(self.pi).cloned() {
            Some(s) => {
                self.stage = Stage::OpElem;
                Ok(Yield::Force(s))
            }
            None => {
                self.work.extend(self.pending.drain(..));
                self.next_element()
            }
        }
    }
}

/// Insert into the sorted key list, reporting whether the key was new.
/// `false` means an equal key is already closed over.
fn insert_key(keys: &mut Vec<Key>, k: Key) -> Result<bool> {
    let mut lo = 0usize;
    let mut hi = keys.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let existing = keys
            .get(mid)
            .ok_or_else(|| VmError::eval("internal: genericClosure key index lost"))?;
        match k.cmp_nix(existing)? {
            Ordering::Less => hi = mid,
            Ordering::Greater => lo = mid + 1,
            Ordering::Equal => return Ok(false),
        }
    }
    keys.insert(lo, k);
    Ok(true)
}

// -- environment ------------------------------------------------------------

/// cppnix returns the empty string for an unset variable rather than
/// failing, and (under `pure-eval` or `restrict-eval`, neither of which this
/// backend is reachable under) for every variable.
pub fn bi_get_env(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let name = want_str(&argv(args, 0)?)?;
    Ok(Value::Str(
        std::env::var(name).unwrap_or_default().as_str().into(),
    ))
}

// -- zipAttrsWith -----------------------------------------------------------

/// Every attrset in the list contributes its value to that name's list, in
/// list order. cppnix builds each result entry as an unapplied `f name vals`
/// so an entry nobody reads never calls `f`, which is what makes
/// `zipAttrsWith (n: v: throw n)` on an unread attribute succeed.
pub fn bi_zip_attrs_with(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    if !matches!(f, Value::Closure(_) | Value::Builtin(_)) {
        return Err(VmError::eval(format!(
            "expected a function but found {}",
            type_name(&f)
        )));
    }
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::ForceEach {
        items,
        i: 0,
        vals: Vec::new(),
        finish: zip_finish,
    }))
}

fn zip_finish(vm: &mut Vm, vals: &[Value], args: &[Slot]) -> Result<Value> {
    let f = argv(args, 0)?;
    let mut seen: BTreeMap<Sym, Vec<Slot>> = BTreeMap::new();
    for v in vals {
        for (k, s) in want_attrs(v)?.iter() {
            seen.entry(*k).or_default().push(s.clone());
        }
    }
    let mut out: BTreeMap<Sym, Slot> = BTreeMap::new();
    for (sym, items) in seen {
        let name: Rc<str> = vm.sym_name(sym).into();
        out.insert(
            sym,
            Slot::pending(
                f.clone(),
                vec![
                    Slot::value(Value::Str(name)),
                    Slot::value(Value::List(Rc::new(items))),
                ],
            ),
        );
    }
    Ok(Value::Attrs(Rc::new(out)))
}

// -- hashString -------------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        for nibble in [b >> 4, b & 0x0f] {
            if let Some(c) = HEX.get(usize::from(nibble)) {
                out.push(char::from(*c));
            }
        }
    }
    out
}

/// The four algorithms cppnix's `parseHashAlgo` accepts, rendered base-16
/// without the `sha256:` prefix (`to_string(HashFormat::Base16, false)`).
pub fn bi_hash_string(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    use sha2::Digest;
    let algo = want_str(&argv(args, 0)?)?;
    let s = want_str(&argv(args, 1)?)?;
    let bytes = s.as_bytes();
    let hex = match algo.as_str() {
        "md5" => hex_of(&md5::Md5::digest(bytes)),
        "sha1" => hex_of(&sha1::Sha1::digest(bytes)),
        "sha256" => hex_of(&sha2::Sha256::digest(bytes)),
        "sha512" => hex_of(&sha2::Sha512::digest(bytes)),
        other => {
            return Err(VmError::eval(format!("unknown hash algorithm '{other}'")));
        }
    };
    Ok(Value::Str(hex.as_str().into()))
}

// -- match / split ----------------------------------------------------------

/// cppnix builds every pattern with `std::regex::extended`: POSIX ERE, where
/// `[[:space:]]` and friends are bracket classes and there are no
/// lookarounds, lazy quantifiers or backreferences to lose.
///
/// `(?s)` is the one syntax adjustment: in POSIX a period matches every
/// character, newline included, while this crate excludes newline unless
/// asked. eval-okay-regex-match2 catches the difference with
/// `^.*CONFIG_BOARD_DIRECTORY="([a-zA-Z0-9_]+)".*$` over a multi-line
/// subject, which cppnix matches and a newline-excluding period does not.
///
/// The remaining dialect gap is that POSIX picks the longest alternative
/// where this crate picks the leftmost one; no corpus pattern, including
/// the ~200 nixpkgs-derived ones in eval-okay-regex-match2, distinguishes
/// them.
fn compile_re(re: &str) -> Result<regex::Regex> {
    regex::Regex::new(&format!("(?s){re}"))
        .map_err(|_| VmError::eval(format!("invalid regular expression '{re}'")))
}

fn group_list(caps: &regex::Captures<'_>) -> Value {
    // Group 0 is the whole match, which cppnix drops: "the first match is
    // the whole string". An unmatched group is null, not the empty string.
    let items: Vec<Slot> = (1..caps.len())
        .map(|i| {
            Slot::value(match caps.get(i) {
                Some(m) => Value::Str(m.as_str().into()),
                None => Value::Null,
            })
        })
        .collect();
    Value::List(Rc::new(items))
}

/// `std::regex_match`: the pattern must cover the whole subject, so the
/// pattern is anchored rather than searched for.
pub fn bi_match(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let re = want_str(&argv(args, 0)?)?;
    let s = want_str(&argv(args, 1)?)?;
    let anchored = compile_re(&format!(r"\A(?:{re})\z"))
        .map_err(|_| VmError::eval(format!("invalid regular expression '{re}'")))?;
    match anchored.captures(&s) {
        Some(caps) => Ok(group_list(&caps)),
        None => Ok(Value::Null),
    }
}

/// Non-matching runs interleaved with one group list per match, starting and
/// ending with a (possibly empty) run: `2 * matches + 1` elements. A pattern
/// that matches nothing hands the subject back as a one-element list.
pub fn bi_split(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let re = want_str(&argv(args, 0)?)?;
    let s = want_str(&argv(args, 1)?)?;
    let rx = compile_re(&re)?;
    let mut out: Vec<Slot> = Vec::new();
    let mut last = 0usize;
    for caps in rx.captures_iter(&s) {
        let Some(whole) = caps.get(0) else { continue };
        let prefix = s
            .get(last..whole.start())
            .ok_or_else(|| VmError::eval("internal: split cut a character in half"))?;
        out.push(Slot::value(Value::Str(prefix.into())));
        out.push(Slot::value(group_list(&caps)));
        last = whole.end();
    }
    if out.is_empty() {
        return Ok(Value::List(Rc::new(vec![Slot::value(Value::Str(
            s.as_str().into(),
        ))])));
    }
    let suffix = s
        .get(last..)
        .ok_or_else(|| VmError::eval("internal: split cut a character in half"))?;
    out.push(Slot::value(Value::Str(suffix.into())));
    Ok(Value::List(Rc::new(out)))
}

// -- fromJSON ---------------------------------------------------------------

pub fn bi_from_json(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let src = want_str(&argv(args, 0)?)?;
    let doc: serde_json::Value = serde_json::from_str(&src)
        .map_err(|e| VmError::eval(format!("while decoding a JSON string: {e}")))?;
    json_to_value(vm, &doc)
}

/// Recursive over the parsed document rather than over Nix values: the depth
/// is whatever serde_json already accepted, so this adds no reach the parser
/// did not already have.
fn json_to_value(vm: &mut Vm, j: &serde_json::Value) -> Result<Value> {
    Ok(match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => json_number(n)?,
        serde_json::Value::String(s) => {
            crate::vm::check_no_nul(s)?;
            Value::Str(s.as_str().into())
        }
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(Slot::value(json_to_value(vm, it)?));
            }
            Value::List(Rc::new(out))
        }
        serde_json::Value::Object(map) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                crate::vm::check_no_nul(k)?;
                let sym = vm.intern(k);
                out.insert(sym, Slot::value(json_to_value(vm, v)?));
            }
            Value::Attrs(Rc::new(out))
        }
    })
}

/// cppnix keeps JSON's int/float distinction: `1` is a Nix integer and `1.0`
/// a Nix float. A JSON integer past `i64` is refused rather than silently
/// widened to a float.
fn json_number(n: &serde_json::Number) -> Result<Value> {
    if let Some(i) = n.as_i64() {
        return Ok(Value::Int(i));
    }
    if let Some(u) = n.as_u64() {
        return Err(VmError::eval(format!(
            "unsigned json number {u} outside of Nix integer range"
        )));
    }
    match n.as_f64() {
        Some(f) => Ok(Value::Float(f)),
        None => Err(VmError::eval(format!(
            "json number {n} outside of Nix integer range"
        ))),
    }
}

// -- fromTOML ---------------------------------------------------------------

pub fn bi_from_toml(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let src = want_str(&argv(args, 0)?)?;
    let doc: toml::Value = toml::from_str(&src)
        .map_err(|e| VmError::eval(format!("while parsing TOML: {}", e.message())))?;
    toml_to_value(vm, &doc)
}

fn toml_to_value(vm: &mut Vm, t: &toml::Value) -> Result<Value> {
    Ok(match t {
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Integer(i) => Value::Int(*i),
        toml::Value::Float(f) => Value::Float(*f),
        toml::Value::String(s) => {
            // cppnix runs its NUL check inside the parse, so the message
            // arrives wrapped: "while parsing TOML: error: input string ...".
            wrap_toml(crate::vm::check_no_nul(s))?;
            Value::Str(s.as_str().into())
        }
        toml::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(Slot::value(toml_to_value(vm, it)?));
            }
            Value::List(Rc::new(out))
        }
        toml::Value::Table(map) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                wrap_toml(crate::vm::check_no_nul(k))?;
                let sym = vm.intern(k);
                out.insert(sym, Slot::value(toml_to_value(vm, v)?));
            }
            Value::Attrs(Rc::new(out))
        }
        // cppnix only turns a TOML date/time into `{ _type = "timestamp"; }`
        // under the parse-toml-timestamps experimental feature, and refuses
        // it otherwise. The C ABI carries source text and a base directory
        // and no settings, so this arm cannot tell the two cases apart;
        // reporting the gap keeps both corpus pairs honest rather than
        // guessing one of them right and the other wrong. ENG-12068.
        toml::Value::Datetime(_) => {
            return Err(VmError::Unimplemented(
                "builtins.fromTOML timestamps (the parse-toml-timestamps feature does not reach this backend)"
                    .to_owned(),
            ));
        }
    })
}

fn wrap_toml(r: Result<()>) -> Result<()> {
    match r {
        Err(VmError::Throw(c)) => Err(VmError::eval(format!(
            "while parsing TOML: error: {}",
            c.message
        ))),
        other => other,
    }
}

// -- toJSON -----------------------------------------------------------------

/// cppnix's `max-call-depth`, which its JSON walk takes a slot of per level
/// (`printValueAsJSON`'s recurse opens with `state.addCallDepth`). This
/// walker is flat and would happily serialise a value cppnix refuses, so the
/// limit is mirrored rather than inherited: eval-fail-toJSON-stack-overflow
/// builds a 100k-deep linked list and expects the refusal.
const MAX_DEPTH: usize = 10_000;

enum Job {
    Lit(String),
    /// Render this slot's forced value, at this nesting depth.
    Val(Slot, usize),
}

/// What the value the machine is about to hand back means.
enum Await {
    /// A queued slot's value, to be rendered at this depth.
    Value(usize),
    /// The `__toString` attribute of this attrset, about to be applied to it.
    ToStrFn(Value),
    /// What `__toString` returned, to be coerced and emitted as a string.
    ToStrResult,
}

pub struct ToJson {
    work: Vec<Job>,
    out: String,
    awaiting: Await,
}

pub fn bi_to_json(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let root = crate::builtins2::arg(args, 0)?.clone();
    Ok(Begin::Cont(Cont::Ext(Ext::ToJson(ToJson {
        work: vec![Job::Val(root, 0)],
        out: String::new(),
        awaiting: Await::Value(0),
    }))))
}

impl ToJson {
    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        if let Some(v) = incoming {
            match std::mem::replace(&mut self.awaiting, Await::Value(0)) {
                Await::Value(d) => {
                    if let Some(y) = self.render(vm, &v, d)? {
                        return Ok(y);
                    }
                }
                Await::ToStrFn(attrs) => {
                    self.awaiting = Await::ToStrResult;
                    return Ok(Yield::Apply(v, Slot::value(attrs)));
                }
                Await::ToStrResult => {
                    // cppnix coerces the result with coerceMore = false, so
                    // only a string or a path is accepted here.
                    let s = match &v {
                        Value::Str(s) => s.to_string(),
                        Value::Path(p) => p.to_string(),
                        other => {
                            return Err(VmError::eval(format!(
                                "cannot coerce {} to a string",
                                type_name(other)
                            )));
                        }
                    };
                    json_string(&s, &mut self.out);
                }
            }
        }
        while let Some(job) = self.work.pop() {
            match job {
                Job::Lit(s) => self.out.push_str(&s),
                Job::Val(slot, d) => {
                    self.awaiting = Await::Value(d);
                    return Ok(Yield::Force(slot));
                }
            }
        }
        Ok(Yield::Done(Value::Str(
            std::mem::take(&mut self.out).as_str().into(),
        )))
    }

    /// `Some(yield)` means the machine has to answer something before this
    /// value can be finished; `None` means it is written and the worklist
    /// should carry on.
    fn render(&mut self, vm: &mut Vm, v: &Value, d: usize) -> Result<Option<Yield>> {
        if d > MAX_DEPTH {
            return Err(VmError::eval("stack overflow; max-call-depth exceeded"));
        }
        match v {
            Value::Int(n) => self.out.push_str(&n.to_string()),
            Value::Float(x) => json_float(*x, &mut self.out),
            Value::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            Value::Null => self.out.push_str("null"),
            Value::Str(s) => json_string(s, &mut self.out),
            // cppnix copies a path to the store and emits the store path,
            // which needs a store this backend has no handle on.
            Value::Path(_) => {
                return Err(VmError::Unimplemented(
                    "builtins.toJSON of a path (cppnix copies it to the store)".to_owned(),
                ));
            }
            Value::List(items) => {
                self.out.push('[');
                let mut queued = Vec::with_capacity(items.len() * 2 + 1);
                for (i, s) in items.iter().enumerate() {
                    if i > 0 {
                        queued.push(Job::Lit(",".to_owned()));
                    }
                    queued.push(Job::Val(s.clone(), d + 1));
                }
                queued.push(Job::Lit("]".to_owned()));
                self.queue(queued);
            }
            Value::Attrs(m) => {
                // tryAttrsToString first: an attrset carrying __toString
                // becomes that function's result, not an object. Then
                // outPath, which a derivation is serialised through.
                let to_string = vm.intern("__toString");
                if let Some(f) = m.get(&to_string) {
                    let f = f.clone();
                    self.awaiting = Await::ToStrFn(v.clone());
                    return Ok(Some(Yield::Force(f)));
                }
                let out_path = vm.intern("outPath");
                if let Some(p) = m.get(&out_path) {
                    self.work.push(Job::Val(p.clone(), d + 1));
                    return Ok(None);
                }
                // lexicographicOrder: sorted by name, not by symbol id.
                let mut entries: Vec<(String, Slot)> = m
                    .iter()
                    .map(|(k, s)| (vm.sym_name(*k).to_owned(), s.clone()))
                    .collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                self.out.push('{');
                let mut queued = Vec::with_capacity(entries.len() * 2 + 1);
                for (i, (name, s)) in entries.into_iter().enumerate() {
                    let mut lead = String::new();
                    if i > 0 {
                        lead.push(',');
                    }
                    json_string(&name, &mut lead);
                    lead.push(':');
                    queued.push(Job::Lit(lead));
                    queued.push(Job::Val(s, d + 1));
                }
                queued.push(Job::Lit("}".to_owned()));
                self.queue(queued);
            }
            Value::Closure(_) | Value::Builtin(_) => {
                return Err(VmError::eval(format!(
                    "cannot convert {} to JSON",
                    type_name(v)
                )));
            }
        }
        Ok(None)
    }

    fn queue(&mut self, items: Vec<Job>) {
        for it in items.into_iter().rev() {
            self.work.push(it);
        }
    }
}

/// nlohmann's dump with the default `ensure_ascii = false`: the seven named
/// escapes, `\u00xx` for the remaining control characters, everything else
/// (including non-ASCII) verbatim.
fn json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// nlohmann prints a double with the shortest representation that round
/// trips, which is also what Rust's `Display` does, and always leaves a
/// decimal point behind so the value reads back as a float. The two disagree
/// on magnitudes where nlohmann switches to an exponent and Rust does not;
/// no corpus value reaches that range.
fn json_float(x: f64, out: &mut String) {
    if !x.is_finite() {
        // nlohmann emits null rather than an invalid JSON token.
        out.push_str("null");
        return;
    }
    let s = format!("{x}");
    out.push_str(&s);
    if !s.contains(['.', 'e', 'E']) {
        out.push_str(".0");
    }
}

#[cfg(test)]
mod tests {
    use crate::eval::eval_str;

    /// Value renders as-is, errors as their debug form; assertions never
    /// panic by hand (the workspace denies `panic`, tests included).
    fn render(src: &str) -> String {
        match eval_str(src) {
            Ok(v) => v,
            Err(e) => format!("{e:?}"),
        }
    }

    #[test]
    fn generic_closure_closes_over_keys_breadth_first() {
        assert_eq!(
            render(
                "builtins.genericClosure {
                   startSet = [ { key = 1; } ];
                   operator = x: if x.key >= 4 then [ ] else [ { key = x.key + 1; } ];
                 }"
            ),
            "[ { key = 1; } { key = 2; } { key = 3; } { key = 4; } ]"
        );
        // A key already closed over is skipped and its operator never runs,
        // which is what stops a cyclic graph from diverging.
        assert_eq!(
            render(
                "builtins.genericClosure {
                   startSet = [ { key = 1; } { key = 1; } ];
                   operator = x: [ { key = 1; } ];
                 }"
            ),
            "[ { key = 1; } ]"
        );
    }

    #[test]
    fn generic_closure_argument_errors_match_cpp_classes() {
        // An empty startSet returns before `operator` is looked at, so a
        // missing operator is not an error there.
        assert_eq!(
            render("builtins.genericClosure { startSet = [ ]; }"),
            "[ ]"
        );
        assert_eq!(
            render("builtins.genericClosure { operator = x: [ ]; }"),
            "Eval(Eval, \"attribute 'startSet' missing\")"
        );
        assert_eq!(
            render("builtins.genericClosure { startSet = [ { key = 1; } ]; }"),
            "Eval(Eval, \"attribute 'operator' missing\")"
        );
        assert_eq!(
            render("builtins.genericClosure { startSet = [ { nokey = 1; } ]; operator = x: [ ]; }"),
            "Eval(Eval, \"attribute 'key' missing\")"
        );
        assert_eq!(
            render(
                "builtins.genericClosure {
                   startSet = [ { key = 1; } { key = \"s\"; } ];
                   operator = x: [ ];
                 }"
            ),
            "Eval(Eval, \"cannot compare a string with an integer\")"
        );
        // Two set-valued keys: the first insert compares against nothing and
        // succeeds, exactly as std::map's does, so the failure needs two.
        assert_eq!(
            render(
                "builtins.genericClosure {
                   startSet = [ { key = { }; } { key = { }; } ];
                   operator = x: [ ];
                 }"
            ),
            "Eval(Eval, \"cannot compare a set with a set; values of that type are incomparable\")"
        );
    }

    #[test]
    fn get_env_reads_the_environment_and_defaults_to_empty() {
        // Set rather than assumed: the corpus runner exports TEST_VAR=foo,
        // and a unit test has no such runner.
        // SAFETY: single-threaded test, no other thread reads the
        // environment concurrently.
        unsafe { std::env::set_var("IXE_TEST_VAR", "foo") };
        assert_eq!(render("builtins.getEnv \"IXE_TEST_VAR\""), "\"foo\"");
        assert_eq!(render("builtins.getEnv \"IXE_NO_SUCH_VAR\""), "\"\"");
    }

    #[test]
    fn zip_attrs_with_groups_by_name_and_stays_lazy() {
        assert_eq!(
            render("builtins.zipAttrsWith (n: vs: { inherit n vs; }) [ { a = 1; b = 2; } { a = 3; } ]"),
            "{ a = { n = \"a\"; vs = [ 1 3 ]; }; b = { n = \"b\"; vs = [ 2 ]; }; }"
        );
        // cppnix builds each entry as an unapplied `f name values`, so an
        // entry nobody reads never calls `f`.
        assert_eq!(
            render("(builtins.zipAttrsWith (n: v: throw n) [ { a = 1; b = 2; } ]).a or \"untouched\""),
            "Eval(Thrown, \"a\")"
        );
        assert_eq!(
            render("builtins.attrNames (builtins.zipAttrsWith (n: v: throw n) [ { a = 1; b = 2; } ])"),
            "[ \"a\" \"b\" ]"
        );
    }

    #[test]
    fn hash_string_matches_the_corpus_digests() {
        // The empty-string row of eval-okay-hashstring, all four algorithms.
        assert_eq!(
            render("builtins.hashString \"md5\" \"\""),
            "\"d41d8cd98f00b204e9800998ecf8427e\""
        );
        assert_eq!(
            render("builtins.hashString \"sha1\" \"\""),
            "\"da39a3ee5e6b4b0d3255bfef95601890afd80709\""
        );
        assert_eq!(
            render("builtins.hashString \"sha256\" \"text 1\""),
            "\"900a4469df00ccbfd0c145c6d1e4b7953dd0afafadd7534e3a4019e8d38fc663\""
        );
        assert_eq!(
            render("builtins.hashString \"sha512\" \"\""),
            "\"cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e\""
        );
        assert_eq!(
            render("builtins.hashString \"sha3\" \"\""),
            "Eval(Eval, \"unknown hash algorithm 'sha3'\")"
        );
    }

    #[test]
    fn match_is_anchored_posix_ere() {
        // regex_match, not regex_search: a pattern covering part of the
        // subject does not match.
        assert_eq!(render("builtins.match \"fo*\" \"foobar\""), "null");
        assert_eq!(render("builtins.match \"foobar\" \"foobar\""), "[ ]");
        assert_eq!(
            render("builtins.match \"(.*)\\\\.nix\" \"foobar.nix\""),
            "[ \"foobar\" ]"
        );
        // POSIX bracket classes, which an ECMAScript engine reads as a set
        // of punctuation instead.
        assert_eq!(
            render("builtins.match \"[[:space:]]+([[:upper:]]+)[[:space:]]+\" \"  FOO   \""),
            "[ \"FOO\" ]"
        );
        // An optional group that did not participate is null, not "".
        assert_eq!(
            render("builtins.match \"((.*)/)?([^/]*)\\\\.(nix|cc)\" \"foobar.cc\""),
            "[ null null \"foobar\" \"cc\" ]"
        );
        // A period spans a newline in POSIX, so this matches across lines.
        assert_eq!(
            render("builtins.match \".*b.*\" \"a\\nb\\nc\""),
            "[ ]"
        );
    }

    #[test]
    fn split_interleaves_runs_and_group_lists() {
        assert_eq!(
            render("builtins.split \"(a)b\" \"abc\""),
            "[ \"\" [ \"a\" ] \"c\" ]"
        );
        assert_eq!(
            render("builtins.split \"(a)|(c)\" \"abc\""),
            "[ \"\" [ \"a\" null ] \"b\" [ null \"c\" ] \"\" ]"
        );
        // No match at all hands the subject back as one element, not three.
        assert_eq!(render("builtins.split \"fo+\" \"f\""), "[ \"f\" ]");
        assert_eq!(
            render("builtins.split \"fo*\" \"foobar\""),
            "[ \"\" [ ] \"bar\" ]"
        );
    }

    #[test]
    fn from_json_keeps_the_int_float_distinction() {
        assert_eq!(render("builtins.fromJSON \"1\""), "1");
        assert_eq!(render("builtins.fromJSON \"1.0\""), "1");
        assert_eq!(render("builtins.typeOf (builtins.fromJSON \"1\")"), "\"int\"");
        assert_eq!(
            render("builtins.typeOf (builtins.fromJSON \"1.0\")"),
            "\"float\""
        );
        assert_eq!(
            render("builtins.fromJSON \"{\\\"x\\\": [1, 2], \\\"y\\\": null}\""),
            "{ x = [ 1 2 ]; y = null; }"
        );
        assert_eq!(
            render("builtins.fromJSON \"18446744073709551615\""),
            "Eval(Eval, \"unsigned json number 18446744073709551615 outside of Nix integer range\")"
        );
        // A NUL cannot live in a Nix string, in a value or in a key.
        assert_eq!(
            render("builtins.fromJSON ''\"a\\u0000b\"''"),
            "Eval(Eval, \"input string 'a\u{2400}b' cannot be represented as Nix string because it contains null bytes\")"
        );
    }

    #[test]
    fn from_toml_builds_tables_and_refuses_what_cpp_refuses() {
        assert_eq!(
            render("builtins.fromTOML ''\n  x=1\n  s=\"a\"\n  [table]\n  y=2\n''"),
            "{ s = \"a\"; table = { y = 2; }; x = 1; }"
        );
        assert_eq!(
            render("builtins.fromTOML ''arr = [ 1, 2, 3 ]''"),
            "{ arr = [ 1 2 3 ]; }"
        );
        assert_eq!(
            render("builtins.fromTOML ''k = \"a\\u0000b\"''"),
            "Eval(Eval, \"while parsing TOML: error: input string 'a\u{2400}b' cannot be represented as Nix string because it contains null bytes\")"
        );
        // The parse-toml-timestamps feature decides whether cppnix builds a
        // { _type = \"timestamp\"; } attrset or refuses, and the C ABI carries
        // no settings, so neither answer can be given honestly.
        assert_eq!(
            render("builtins.fromTOML ''d = 1979-05-27T07:32:00''"),
            "Unimplemented(\"builtins.fromTOML timestamps (the parse-toml-timestamps feature does not reach this backend)\")"
        );
    }

    #[test]
    fn to_json_matches_nlohmann_dump() {
        assert_eq!(
            render("builtins.toJSON { a = 123; b = -456; c = \"foo\"; }"),
            r#""{\"a\":123,\"b\":-456,\"c\":\"foo\"}""#
        );
        // Names come out in lexicographic order whatever order they were
        // written in, and there is no whitespace anywhere.
        assert_eq!(
            render("builtins.toJSON { z = 1; a = 2; }"),
            r#""{\"a\":2,\"z\":1}""#
        );
        assert_eq!(
            render("builtins.toJSON [ 1 [ \"b\" { } ] ]"),
            r#""[1,[\"b\",{}]]""#
        );
        assert_eq!(render("builtins.toJSON 1.44"), "\"1.44\"");
        // An integral float keeps a decimal point, so it reads back a float.
        assert_eq!(render("builtins.toJSON 5.0"), "\"5.0\"");
        assert_eq!(render("builtins.toJSON null"), "\"null\"");
        // The escapes nlohmann names, plus \u00xx for the rest of C0.
        assert_eq!(
            render("builtins.toJSON \"a\\nb\\\"c\\td\""),
            r#""\"a\\nb\\\"c\\td\"""#
        );
        assert_eq!(
            render("builtins.toJSON (x: x)"),
            "Eval(Eval, \"cannot convert a function to JSON\")"
        );
    }

    #[test]
    fn to_json_takes_a_set_through_its_to_string_or_out_path() {
        // tryAttrsToString: __toString wins over the object form, and is
        // called with the set itself.
        assert_eq!(
            render("builtins.toJSON { __toString = self: self.a; a = \"foo\"; }"),
            "\"\\\"foo\\\"\""
        );
        // A derivation is serialised through outPath.
        assert_eq!(
            render("builtins.toJSON { outPath = \"/nix/store/x\"; drvPath = \"ignored\"; }"),
            "\"\\\"/nix/store/x\\\"\""
        );
    }

    /// cppnix's JSON walk spends one `max-call-depth` slot per level, so a
    /// value nested past the limit is refused rather than serialised. This
    /// walker is flat and has to be told; eval-fail-toJSON-stack-overflow is
    /// the pair that notices.
    #[test]
    fn to_json_refuses_a_value_deeper_than_max_call_depth() {
        let deep = "builtins.foldl' (tail: head: { inherit head tail; }) null (builtins.genList (x: x) 20000)";
        assert_eq!(
            render(&format!("builtins.toJSON ({deep})")),
            "Eval(Eval, \"stack overflow; max-call-depth exceeded\")"
        );
        // Just under the limit still serialises, so the cap is a cap and not
        // a blanket refusal.
        let shallow = "builtins.foldl' (tail: head: { inherit head tail; }) null (builtins.genList (x: x) 100)";
        assert!(render(&format!("builtins.toJSON ({shallow})")).starts_with('"'));
    }

    /// The two out-of-range TOML integers are allowlisted as an error-text
    /// divergence; that entry is only honest while toml-rs still refuses
    /// them, so a dependency bump that started accepting one must break here
    /// rather than in the corpus differ.
    #[test]
    fn from_toml_still_refuses_integers_past_the_nix_range() {
        for src in [
            "builtins.fromTOML ''attr = 9223372036854775808''",
            "builtins.fromTOML ''attr = -9223372036854775809''",
        ] {
            let got = render(src);
            assert!(
                got.starts_with("Eval(Eval, \"while parsing TOML:"),
                "expected a TOML parse refusal, got {got}"
            );
        }
    }
}
