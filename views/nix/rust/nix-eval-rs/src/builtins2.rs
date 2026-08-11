//! Second builtins batch -- list, attrset, string and type-test primops that
//! need no host (fs/store) access -- plus the continuations the machine drives
//! for the ones that must evaluate more Nix.
//!
//! Every builtin here is either pure over already-forced arguments or a
//! `Cont`: a cursor the machine steps, handing back one forced value or one
//! application result at a time. Nothing calls back into the interpreter, so
//! `map` over a million-element list, `sort` with a Nix comparator and
//! `deepSeq` over a self-referential attrset all run flat.

use crate::builtins::{Kind, TABLE};
use crate::task::{NeedPath, Task, Yield};
use crate::value2::{Slot, Sym, Value, type_name};
use crate::vm::{Result, Vm, VmError, forced};
use std::collections::{BTreeMap, BTreeSet, btree_map};
use std::rc::Rc;

/// How a builtin body starts: with an answer, with one thing to evaluate
/// whose value is the answer, or with a continuation to drive.
pub enum Begin {
    Done(Value),
    Force(Slot),
    Sub(Task),
    Cont(Cont),
}

/// One literal run or one `to`-index in a `replaceStrings` plan. Planning the
/// substitution before any `to` is forced is what keeps unused replacements
/// lazy, which the corpus checks with a `throw` in an unreachable slot.
pub enum Piece {
    Lit(String),
    Use(usize),
}

pub enum Cont {
    /// Forcing argument `i` before the body runs.
    Args(usize),
    /// Run the builtin body.
    Start,
    /// Hand back the next value delivered.
    Result,
    Filter {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        out: Vec<Slot>,
    },
    /// `any` (want = true) and `all` (want = false): stop at the first
    /// element whose predicate equals `want`.
    AnyAll {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        want: bool,
    },
    ConcatMap {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        out: Vec<Slot>,
    },
    /// `half` holds `f acc` between the two applications of one step.
    Foldl {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        acc: Slot,
        half: Option<Value>,
        /// Set once the list is exhausted and the accumulator is being
        /// forced: without it the forced value arrives looking like the
        /// result of `f acc` and gets applied to a list element that is not
        /// there.
        finishing: bool,
    },
    /// Insertion sort: cppnix's sort is stable and its comparator is a Nix
    /// function that can throw, which rules the standard sorts out.
    Sort {
        f: Value,
        items: Vec<Slot>,
        next: usize,
        sorted: Vec<Slot>,
        probe: usize,
        half: Option<Value>,
    },
    GroupBy {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        out: BTreeMap<Sym, Vec<Slot>>,
    },
    Partition {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        right: Vec<Slot>,
        wrong: Vec<Slot>,
    },
    Elem {
        x: Slot,
        items: Rc<Vec<Slot>>,
        i: usize,
    },
    /// Force every element, then finish purely.
    ForceEach {
        items: Rc<Vec<Slot>>,
        i: usize,
        vals: Vec<Value>,
        finish: Finish,
    },
    ListToAttrs {
        items: Rc<Vec<Slot>>,
        i: usize,
        out: BTreeMap<Sym, Slot>,
        cur: Option<Rc<BTreeMap<Sym, Slot>>>,
        name_sym: Sym,
        value_sym: Sym,
    },
    Replace {
        stage: u8,
        froms: Vec<String>,
        plan: Vec<Piece>,
        used: Vec<usize>,
        k: usize,
        tos: BTreeMap<usize, String>,
    },
    /// `seen` is keyed on cell identity, the way cppnix's forceValueDeep is:
    /// without it a self-referential attrset never bottoms out. `tail` marks
    /// the switch to the second argument, which cppnix forces only once the
    /// first is fully deep, so a throw in either lands in that order.
    DeepSeq {
        work: Vec<Slot>,
        seen: BTreeSet<usize>,
        tail: bool,
    },
    TryEval {
        started: bool,
    },
    /// Waiting on the scheduler's answer to a path question.
    Path {
        asked: bool,
        need: NeedPath,
    },
    /// A continuation owned by `builtins3`, so that batch's state machines
    /// live beside the builtins they belong to rather than here.
    Ext(crate::builtins3::Ext),
    /// `import`, in three steps: ask the scheduler for the file, compile it
    /// and force its entry, then hand that value back. Three and not two --
    /// the forced module value returns to this same continuation, and a
    /// two-state version parses it as if it were the scheduler's answer.
    Import {
        stage: u8,
    },
}

pub type Finish = fn(&mut Vm, &[Value], &[Slot]) -> Result<Value>;

// -- the driver -------------------------------------------------------------

pub fn drive(
    vm: &mut Vm,
    idx: u16,
    args: &mut [Slot],
    cont: &mut Cont,
    incoming: Option<Value>,
) -> Result<Yield> {
    let mut incoming = incoming;
    loop {
        if let Cont::Args(i) = cont {
            let lazy = TABLE.get(idx as usize).map(|b| b.lazy).unwrap_or(&[]);
            let mut at = *i;
            while lazy.contains(&at) {
                at += 1;
            }
            if let Some(s) = args.get(at).cloned() {
                *i = at + 1;
                return Ok(Yield::Force(s));
            }
        }
        if matches!(cont, Cont::Args(_)) {
            *cont = Cont::Start;
            continue;
        }
        if matches!(cont, Cont::Start) {
            let b = TABLE
                .get(idx as usize)
                .ok_or_else(|| VmError::eval("internal: bad builtin index"))?;
            match &b.kind {
                Kind::Pure(f) => return Ok(Yield::Done(f(vm, args)?)),
                Kind::Start(g) => match g(vm, args)? {
                    Begin::Done(v) => return Ok(Yield::Done(v)),
                    Begin::Force(s) => {
                        *cont = Cont::Result;
                        return Ok(Yield::Force(s));
                    }
                    Begin::Sub(t) => {
                        *cont = Cont::Result;
                        return Ok(Yield::Sub(t));
                    }
                    Begin::Cont(c) => {
                        *cont = c;
                        incoming = None;
                        continue;
                    }
                },
            }
        }
        if matches!(cont, Cont::Result) {
            return incoming
                .take()
                .map(Yield::Done)
                .ok_or_else(|| VmError::eval("internal: builtin result lost"));
        }
        return step_cont(vm, args, cont, incoming.take());
    }
}

fn step_cont(
    vm: &mut Vm,
    args: &[Slot],
    cont: &mut Cont,
    incoming: Option<Value>,
) -> Result<Yield> {
    match cont {
        Cont::Args(_) | Cont::Start | Cont::Result => {
            Err(VmError::eval("internal: driver state reached the walker"))
        }
        Cont::Ext(e) => crate::builtins3::step(vm, args, e, incoming),
        Cont::Filter { f, items, i, out } => {
            if let Some(v) = incoming {
                if want_bool(&v)? {
                    out.push(nth(items, *i)?);
                }
                *i += 1;
            }
            if *i >= items.len() {
                return Ok(Yield::Done(Value::List(Rc::new(std::mem::take(out)))));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::AnyAll { f, items, i, want } => {
            if let Some(v) = incoming {
                if want_bool(&v)? == *want {
                    return Ok(Yield::Done(Value::Bool(*want)));
                }
                *i += 1;
            }
            if *i >= items.len() {
                return Ok(Yield::Done(Value::Bool(!*want)));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::ConcatMap { f, items, i, out } => {
            if let Some(v) = incoming {
                out.extend(want_list(&v)?.iter().cloned());
                *i += 1;
            }
            if *i >= items.len() {
                return Ok(Yield::Done(Value::List(Rc::new(std::mem::take(out)))));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::Foldl {
            f,
            items,
            i,
            acc,
            half,
            finishing,
        } => {
            if *finishing {
                return incoming
                    .map(Yield::Done)
                    .ok_or_else(|| VmError::eval("internal: foldl' lost its accumulator"));
            }
            if let Some(v) = incoming {
                if half.is_none() {
                    *half = Some(v.clone());
                    return Ok(Yield::Apply(v, nth(items, *i)?));
                }
                *half = None;
                *acc = Slot::value(v);
                *i += 1;
            }
            if *i >= items.len() {
                // The initial accumulator is never forced by foldl' itself,
                // so an empty list hands it straight back unforced; anything
                // later that wants a value forces it then.
                return match acc.peek() {
                    Some(v) => Ok(Yield::Done(v)),
                    None => {
                        *finishing = true;
                        Ok(Yield::Force(acc.clone()))
                    }
                };
            }
            Ok(Yield::Apply(f.clone(), acc.clone()))
        }
        Cont::Sort {
            f,
            items,
            next,
            sorted,
            probe,
            half,
        } => {
            if let Some(v) = incoming {
                if half.is_none() {
                    *half = Some(v.clone());
                    return Ok(Yield::Apply(v, nth(sorted, *probe)?));
                }
                *half = None;
                if want_bool(&v)? {
                    let item = nth(items, *next)?;
                    sorted.insert(*probe, item);
                    *next += 1;
                    *probe = 0;
                } else {
                    *probe += 1;
                }
            }
            loop {
                if *next >= items.len() {
                    return Ok(Yield::Done(Value::List(Rc::new(std::mem::take(sorted)))));
                }
                if *probe >= sorted.len() {
                    let item = nth(items, *next)?;
                    sorted.push(item);
                    *next += 1;
                    *probe = 0;
                    continue;
                }
                return Ok(Yield::Apply(f.clone(), nth(items, *next)?));
            }
        }
        Cont::GroupBy { f, items, i, out } => {
            if let Some(v) = incoming {
                let key = want_str(&v)?;
                let sym = vm.intern(&key);
                out.entry(sym).or_default().push(nth(items, *i)?);
                *i += 1;
            }
            if *i >= items.len() {
                let map: BTreeMap<Sym, Slot> = std::mem::take(out)
                    .into_iter()
                    .map(|(k, v)| (k, Slot::value(Value::List(Rc::new(v)))))
                    .collect();
                return Ok(Yield::Done(Value::Attrs(Rc::new(map))));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::Partition {
            f,
            items,
            i,
            right,
            wrong,
        } => {
            if let Some(v) = incoming {
                if want_bool(&v)? {
                    right.push(nth(items, *i)?);
                } else {
                    wrong.push(nth(items, *i)?);
                }
                *i += 1;
            }
            if *i >= items.len() {
                let mut map = BTreeMap::new();
                let r = vm.intern("right");
                map.insert(r, Slot::value(Value::List(Rc::new(std::mem::take(right)))));
                let w = vm.intern("wrong");
                map.insert(w, Slot::value(Value::List(Rc::new(std::mem::take(wrong)))));
                return Ok(Yield::Done(Value::Attrs(Rc::new(map))));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::Elem { x, items, i } => {
            if matches!(incoming, Some(Value::Bool(true))) {
                return Ok(Yield::Done(Value::Bool(true)));
            }
            if incoming.is_some() {
                *i += 1;
            }
            if *i >= items.len() {
                return Ok(Yield::Done(Value::Bool(false)));
            }
            Ok(Yield::Sub(Task::deep_eq_slots(
                x.clone(),
                nth(items, *i)?,
            )))
        }
        Cont::ForceEach {
            items,
            i,
            vals,
            finish,
        } => {
            if let Some(v) = incoming {
                vals.push(v);
                *i += 1;
            }
            if *i >= items.len() {
                let f = *finish;
                return Ok(Yield::Done(f(vm, vals, args)?));
            }
            Ok(Yield::Force(nth(items, *i)?))
        }
        Cont::ListToAttrs {
            items,
            i,
            out,
            cur,
            name_sym,
            value_sym,
        } => {
            match (incoming, cur.take()) {
                (Some(v), None) => {
                    let m = want_attrs(&v)?;
                    let ns = m
                        .get(name_sym)
                        .cloned()
                        .ok_or_else(|| VmError::eval("attribute 'name' missing"))?;
                    *cur = Some(m);
                    return Ok(Yield::Force(ns));
                }
                (Some(v), Some(m)) => {
                    let name = want_str(&v)?;
                    let sym = vm.intern(&name);
                    // First binding wins in cppnix.
                    if let btree_map::Entry::Vacant(slot) = out.entry(sym) {
                        let val = m
                            .get(value_sym)
                            .cloned()
                            .ok_or_else(|| VmError::eval("attribute 'value' missing"))?;
                        slot.insert(val);
                    }
                    *i += 1;
                }
                (None, _) => {}
            }
            if *i >= items.len() {
                return Ok(Yield::Done(Value::Attrs(Rc::new(std::mem::take(out)))));
            }
            Ok(Yield::Force(nth(items, *i)?))
        }
        Cont::Replace {
            stage,
            froms,
            plan,
            used,
            k,
            tos,
        } => {
            let from_items = want_list(&argv(args, 0)?)?;
            let to_items = want_list(&argv(args, 1)?)?;
            let mut incoming = incoming;
            if *stage == 0 {
                if let Some(v) = incoming.take() {
                    froms.push(want_str(&v)?);
                }
                if froms.len() < from_items.len() {
                    let at = froms.len();
                    return Ok(Yield::Force(nth(&from_items, at)?));
                }
                let subject = want_str(&argv(args, 2)?)?;
                *plan = build_plan(froms, &subject);
                *used = distinct_uses(plan);
                *stage = 1;
            }
            if let Some(v) = incoming.take() {
                let j = *used
                    .get(*k)
                    .ok_or_else(|| VmError::eval("internal: replaceStrings index lost"))?;
                tos.insert(j, want_str(&v)?);
                *k += 1;
            }
            if *k < used.len() {
                let j = *used
                    .get(*k)
                    .ok_or_else(|| VmError::eval("internal: replaceStrings index lost"))?;
                return Ok(Yield::Force(nth(&to_items, j)?));
            }
            let mut out = String::new();
            for p in plan.iter() {
                match p {
                    Piece::Lit(s) => out.push_str(s),
                    Piece::Use(j) => out.push_str(tos.get(j).map(String::as_str).unwrap_or("")),
                }
            }
            Ok(Yield::Done(Value::Str(out.into())))
        }
        Cont::DeepSeq { work, seen, tail } => {
            if *tail {
                return incoming
                    .map(Yield::Done)
                    .ok_or_else(|| VmError::eval("internal: deepSeq lost its result"));
            }
            if let Some(v) = incoming {
                match v {
                    Value::List(l) => work.extend(l.iter().cloned()),
                    Value::Attrs(m) => work.extend(m.values().cloned()),
                    _ => {}
                }
            }
            while let Some(s) = work.pop() {
                if !seen.insert(s.id()) {
                    continue;
                }
                return Ok(Yield::Force(s));
            }
            *tail = true;
            Ok(Yield::Force(arg(args, 1)?.clone()))
        }
        Cont::Path { asked, need } => {
            if !*asked {
                *asked = true;
                return Ok(Yield::Need(need.clone()));
            }
            incoming
                .map(Yield::Done)
                .ok_or_else(|| VmError::eval("internal: path answer lost"))
        }
        Cont::Import { stage } => {
            if *stage == 0 {
                *stage = 1;
                let p = want_path_str(&argv(args, 0)?)?;
                return Ok(Yield::Need(NeedPath::Import(p)));
            }
            if *stage == 2 {
                return incoming
                    .map(Yield::Done)
                    .ok_or_else(|| VmError::eval("internal: import value lost"));
            }
            *stage = 2;
            let answer = incoming.ok_or_else(|| VmError::eval("internal: import answer lost"))?;
            let m = want_attrs(&answer)?;
            let key = |vm: &mut Vm, name: &str| -> Result<String> {
                let sym = vm.intern(name);
                let slot = m
                    .get(&sym)
                    .ok_or_else(|| VmError::eval("internal: malformed import answer"))?;
                want_str(&forced(slot)?)
            };
            let path = key(vm, "path")?;
            let text = key(vm, "text")?;
            // The imported file's own directory is what its relative paths
            // resolve against, and it is the RESOLVED path's parent: a
            // directory import reads default.nix, so using the argument here
            // would resolve one level too high.
            let base = match path.rfind('/') {
                Some(0) => "/".to_owned(),
                Some(i) => path.get(..i).unwrap_or("/").to_owned(),
                None => ".".to_owned(),
            };
            let module = vm.import_module(&path, &text, &base)?;
            let entry = module.entry;
            Ok(Yield::Force(Slot::thunk(
                module,
                entry,
                Rc::new(crate::value2::EnvNode::Root),
            )))
        }
        Cont::TryEval { started } => {
            if !*started {
                *started = true;
                return Ok(Yield::Force(nth(args, 0)?));
            }
            let v = incoming.ok_or_else(|| VmError::eval("internal: tryEval lost its value"))?;
            Ok(Yield::Done(try_eval_result(vm, true, v)))
        }
    }
}

pub fn try_eval_result(vm: &mut Vm, success: bool, value: Value) -> Value {
    let mut out = BTreeMap::new();
    let s = vm.intern("success");
    out.insert(s, Slot::value(Value::Bool(success)));
    let v = vm.intern("value");
    out.insert(v, Slot::value(value));
    Value::Attrs(Rc::new(out))
}

// -- shared helpers ---------------------------------------------------------

pub fn arg(args: &[Slot], i: usize) -> Result<&Slot> {
    args.get(i)
        .ok_or_else(|| VmError::eval("internal: missing builtin argument"))
}

/// The already-forced value of argument `i`.
pub fn argv(args: &[Slot], i: usize) -> Result<Value> {
    forced(arg(args, i)?)
}

fn nth(items: &[Slot], i: usize) -> Result<Slot> {
    items
        .get(i)
        .cloned()
        .ok_or_else(|| VmError::eval("internal: list position lost"))
}

pub(crate) fn want_list(v: &Value) -> Result<Rc<Vec<Slot>>> {
    match v {
        Value::List(l) => Ok(l.clone()),
        other => Err(VmError::eval(format!(
            "expected a list but found {}",
            type_name(other)
        ))),
    }
}

pub(crate) fn want_attrs(v: &Value) -> Result<Rc<BTreeMap<Sym, Slot>>> {
    match v {
        Value::Attrs(m) => Ok(m.clone()),
        other => Err(VmError::eval(format!(
            "expected a set but found {}",
            type_name(other)
        ))),
    }
}

pub(crate) fn want_int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(VmError::eval(format!(
            "expected an integer but found {}",
            type_name(other)
        ))),
    }
}

pub(crate) fn want_str(v: &Value) -> Result<String> {
    match v {
        Value::Str(s) => Ok(s.to_string()),
        other => Err(VmError::eval(format!(
            "expected a string but found {}",
            type_name(other)
        ))),
    }
}

pub(crate) fn want_bool(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(VmError::eval(format!(
            "expected a Boolean but found {}",
            type_name(other)
        ))),
    }
}

// -- list builtins ----------------------------------------------------------

pub fn bi_length(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Int(want_list(&argv(args, 0)?)?.len() as i64))
}

pub fn bi_head(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let l = want_list(&argv(args, 0)?)?;
    match l.first() {
        Some(s) => Ok(Begin::Force(s.clone())),
        None => Err(VmError::eval("list index 0 is out of bounds")),
    }
}

pub fn bi_tail(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let l = want_list(&argv(args, 0)?)?;
    if l.is_empty() {
        return Err(VmError::eval("'tail' called on an empty list"));
    }
    Ok(Value::List(Rc::new(l.iter().skip(1).cloned().collect())))
}

pub fn bi_elem_at(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let l = want_list(&argv(args, 0)?)?;
    let i = want_int(&argv(args, 1)?)?;
    let s = usize::try_from(i)
        .ok()
        .and_then(|i| l.get(i))
        .ok_or_else(|| VmError::eval(format!("list index {i} is out of bounds")))?;
    Ok(Begin::Force(s.clone()))
}

pub fn bi_elem(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::Elem {
        x: arg(args, 0)?.clone(),
        items,
        i: 0,
    }))
}

/// cppnix builds each element with mkApp, so `map f xs` applies nothing
/// until an element is forced; the corpus catches the difference through
/// `mapAttrs throw`, and the same rule governs map and genList.
pub fn bi_map(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    let out: Vec<Slot> = items
        .iter()
        .map(|s| Slot::pending(f.clone(), vec![s.clone()]))
        .collect();
    Ok(Begin::Done(Value::List(Rc::new(out))))
}

pub fn bi_filter(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::Filter {
        f,
        items,
        i: 0,
        out: Vec::new(),
    }))
}

pub fn bi_any(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    any_all(args, true)
}

pub fn bi_all(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    any_all(args, false)
}

fn any_all(args: &[Slot], want: bool) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::AnyAll {
        f,
        items,
        i: 0,
        want,
    }))
}

pub fn bi_concat_lists(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let items = want_list(&argv(args, 0)?)?;
    Ok(Begin::Cont(Cont::ForceEach {
        items,
        i: 0,
        vals: Vec::new(),
        finish: finish_concat_lists,
    }))
}

fn finish_concat_lists(_vm: &mut Vm, vals: &[Value], _args: &[Slot]) -> Result<Value> {
    let mut out = Vec::new();
    for v in vals {
        out.extend(want_list(v)?.iter().cloned());
    }
    Ok(Value::List(Rc::new(out)))
}

pub fn bi_concat_map(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::ConcatMap {
        f,
        items,
        i: 0,
        out: Vec::new(),
    }))
}

pub fn bi_gen_list(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let n = want_int(&argv(args, 1)?)?;
    let n = usize::try_from(n)
        .map_err(|_| VmError::eval(format!("cannot create list of size {n}")))?;
    let out: Vec<Slot> = (0..n)
        .map(|i| Slot::pending(f.clone(), vec![Slot::value(Value::Int(i as i64))]))
        .collect();
    Ok(Begin::Done(Value::List(Rc::new(out))))
}

/// cppnix's foldl' is strict in the accumulator it PRODUCES, not in the one
/// it is given: `foldl' op (throw "x") [ … ]` is fine as long as op ignores it.
pub fn bi_foldl_strict(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let acc = arg(args, 1)?.clone();
    let items = want_list(&argv(args, 2)?)?;
    Ok(Begin::Cont(Cont::Foldl {
        f,
        items,
        i: 0,
        acc,
        half: None,
        finishing: false,
    }))
}

pub fn bi_sort(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let l = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::Sort {
        f,
        items: l.iter().cloned().collect(),
        next: 0,
        sorted: Vec::with_capacity(l.len()),
        probe: 0,
        half: None,
    }))
}

// -- attrset builtins -------------------------------------------------------

pub fn bi_attr_names(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let m = want_attrs(&argv(args, 0)?)?;
    let mut names: Vec<String> = m.keys().map(|k| vm.sym_name(*k).to_owned()).collect();
    names.sort();
    Ok(Value::List(Rc::new(
        names
            .into_iter()
            .map(|n| Slot::value(Value::Str(n.into())))
            .collect(),
    )))
}

pub fn bi_attr_values(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let m = want_attrs(&argv(args, 0)?)?;
    let mut entries: Vec<(String, Slot)> = m
        .iter()
        .map(|(k, s)| (vm.sym_name(*k).to_owned(), s.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Value::List(Rc::new(
        entries.into_iter().map(|(_, s)| s).collect(),
    )))
}

pub fn bi_get_attr(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let name = want_str(&argv(args, 0)?)?;
    let m = want_attrs(&argv(args, 1)?)?;
    let sym = vm.intern(&name);
    match m.get(&sym) {
        Some(s) => Ok(Begin::Force(s.clone())),
        None => Err(VmError::eval(format!("attribute '{name}' missing"))),
    }
}

pub fn bi_has_attr(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let name = want_str(&argv(args, 0)?)?;
    let m = want_attrs(&argv(args, 1)?)?;
    let sym = vm.intern(&name);
    Ok(Value::Bool(m.contains_key(&sym)))
}

pub fn bi_remove_attrs(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    want_attrs(&argv(args, 0)?)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::ForceEach {
        items,
        i: 0,
        vals: Vec::new(),
        finish: finish_remove_attrs,
    }))
}

fn finish_remove_attrs(vm: &mut Vm, vals: &[Value], args: &[Slot]) -> Result<Value> {
    let m = want_attrs(&argv(args, 0)?)?;
    let mut out = (*m).clone();
    for v in vals {
        let name = want_str(v)?;
        let sym = vm.intern(&name);
        out.remove(&sym);
    }
    Ok(Value::Attrs(Rc::new(out)))
}

pub fn bi_intersect_attrs(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let a = want_attrs(&argv(args, 0)?)?;
    let b = want_attrs(&argv(args, 1)?)?;
    let out: BTreeMap<Sym, Slot> = b
        .iter()
        .filter(|(k, _)| a.contains_key(k))
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    Ok(Value::Attrs(Rc::new(out)))
}

pub fn bi_cat_attrs(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    want_str(&argv(args, 0)?)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::ForceEach {
        items,
        i: 0,
        vals: Vec::new(),
        finish: finish_cat_attrs,
    }))
}

fn finish_cat_attrs(vm: &mut Vm, vals: &[Value], args: &[Slot]) -> Result<Value> {
    let name = want_str(&argv(args, 0)?)?;
    let sym = vm.intern(&name);
    let mut out = Vec::new();
    for v in vals {
        if let Some(s) = want_attrs(v)?.get(&sym) {
            out.push(s.clone());
        }
    }
    Ok(Value::List(Rc::new(out)))
}

pub fn bi_list_to_attrs(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let items = want_list(&argv(args, 0)?)?;
    let name_sym = vm.intern("name");
    let value_sym = vm.intern("value");
    Ok(Begin::Cont(Cont::ListToAttrs {
        items,
        i: 0,
        out: BTreeMap::new(),
        cur: None,
        name_sym,
        value_sym,
    }))
}

pub fn bi_map_attrs(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let m = want_attrs(&argv(args, 1)?)?;
    let mut out = BTreeMap::new();
    for (k, s) in m.iter() {
        let name = Slot::value(Value::Str(vm.sym_name(*k).into()));
        out.insert(*k, Slot::pending(f.clone(), vec![name, s.clone()]));
    }
    Ok(Begin::Done(Value::Attrs(Rc::new(out))))
}

pub fn bi_group_by(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::GroupBy {
        f,
        items,
        i: 0,
        out: BTreeMap::new(),
    }))
}

pub fn bi_partition(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::Partition {
        f,
        items,
        i: 0,
        right: Vec::new(),
        wrong: Vec::new(),
    }))
}

// -- string builtins --------------------------------------------------------

pub fn bi_string_length(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Int(want_str(&argv(args, 0)?)?.len() as i64))
}

pub fn bi_substring(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let start = want_int(&argv(args, 0)?)?;
    let len = want_int(&argv(args, 1)?)?;
    let s = want_str(&argv(args, 2)?)?;
    if start < 0 {
        return Err(VmError::eval("negative start position in 'substring'"));
    }
    let start = start as usize;
    let out: String = if start >= s.len() {
        String::new()
    } else if len < 0 {
        s.get(start..).unwrap_or("").to_owned()
    } else {
        let end = start.saturating_add(len as usize).min(s.len());
        s.get(start..end).unwrap_or("").to_owned()
    };
    Ok(Value::Str(out.into()))
}

pub fn bi_concat_strings_sep(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    want_str(&argv(args, 0)?)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::ForceEach {
        items,
        i: 0,
        vals: Vec::new(),
        finish: finish_concat_sep,
    }))
}

fn finish_concat_sep(_vm: &mut Vm, vals: &[Value], args: &[Slot]) -> Result<Value> {
    let sep = want_str(&argv(args, 0)?)?;
    let mut parts = Vec::with_capacity(vals.len());
    for v in vals {
        parts.push(match v {
            Value::Str(s) => s.to_string(),
            Value::Path(p) => p.to_string(),
            other => {
                return Err(VmError::eval(format!(
                    "cannot coerce {} to a string",
                    type_name(other)
                )));
            }
        });
    }
    Ok(Value::Str(parts.join(&sep).into()))
}

pub fn bi_replace_strings(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let from = want_list(&argv(args, 0)?)?;
    let to = want_list(&argv(args, 1)?)?;
    want_str(&argv(args, 2)?)?;
    if from.len() != to.len() {
        return Err(VmError::eval(
            "'from' and 'to' arguments to 'replaceStrings' have different lengths",
        ));
    }
    Ok(Begin::Cont(Cont::Replace {
        stage: 0,
        froms: Vec::with_capacity(from.len()),
        plan: Vec::new(),
        used: Vec::new(),
        k: 0,
        tos: BTreeMap::new(),
    }))
}

fn build_plan(froms: &[String], s: &str) -> Vec<Piece> {
    let mut plan = Vec::new();
    let mut lit = String::new();
    let mut i = 0usize;
    while i <= s.len() {
        let mut matched = false;
        for (j, f) in froms.iter().enumerate() {
            let hit = if f.is_empty() {
                true
            } else {
                s.get(i..)
                    .map(|rest| rest.starts_with(f.as_str()))
                    .unwrap_or(false)
            };
            if !hit {
                continue;
            }
            if !lit.is_empty() {
                plan.push(Piece::Lit(std::mem::take(&mut lit)));
            }
            plan.push(Piece::Use(j));
            if f.is_empty() {
                // Empty match: emit one source char and advance.
                if let Some(c) = s.get(i..).and_then(|r| r.chars().next()) {
                    lit.push(c);
                    i += c.len_utf8();
                } else {
                    i += 1;
                }
            } else {
                i += f.len();
            }
            matched = true;
            break;
        }
        if !matched {
            if let Some(c) = s.get(i..).and_then(|r| r.chars().next()) {
                lit.push(c);
                i += c.len_utf8();
            } else {
                break;
            }
        }
    }
    if !lit.is_empty() {
        plan.push(Piece::Lit(lit));
    }
    plan
}

fn distinct_uses(plan: &[Piece]) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for p in plan {
        if let Piece::Use(j) = p
            && seen.insert(*j)
        {
            out.push(*j);
        }
    }
    out
}

pub fn bi_split_version(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let s = want_str(&argv(args, 0)?)?;
    Ok(Value::List(Rc::new(
        version_parts(&s)
            .into_iter()
            .map(|p| Slot::value(Value::Str(p.into())))
            .collect(),
    )))
}

// -- evaluation control -----------------------------------------------------

pub fn bi_seq(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    argv(args, 0)?;
    argv(args, 1)
}

pub fn bi_deep_seq(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::DeepSeq {
        work: vec![arg(args, 0)?.clone()],
        seen: BTreeSet::new(),
        tail: false,
    }))
}

pub fn bi_try_eval(_vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::TryEval { started: false }))
}

// -- paths ------------------------------------------------------------------

/// A path argument, as the string the host is asked about. cppnix accepts a
/// string here as well as a path, and the difference (string context) is not
/// modelled yet.
fn want_path_str(v: &Value) -> Result<String> {
    match v {
        Value::Path(p) => Ok(p.to_string()),
        Value::Str(s) => Ok(s.to_string()),
        other => Err(VmError::eval(format!(
            "cannot coerce {} to a path",
            type_name(other)
        ))),
    }
}

fn ask(args: &[Slot], mk: fn(String) -> NeedPath) -> Result<Begin> {
    let p = want_path_str(&argv(args, 0)?)?;
    Ok(Begin::Cont(Cont::Path {
        asked: false,
        need: mk(p),
    }))
}

pub fn bi_import(_vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::Import { stage: 0 }))
}

pub fn bi_read_file(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    ask(args, NeedPath::Contents)
}

pub fn bi_path_exists(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    ask(args, NeedPath::Exists)
}

pub fn bi_read_dir(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    ask(args, NeedPath::Entries)
}

pub fn bi_read_file_type(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    ask(args, NeedPath::Kind)
}

pub fn bi_function_args(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let f = argv(args, 0)?;
    match &f {
        Value::Closure(c) => {
            let unit = c
                .module
                .units
                .get(c.unit as usize)
                .ok_or_else(|| VmError::eval("internal: bad closure unit"))?;
            let mut out = BTreeMap::new();
            if let Some(crate::ir::Param::Formals { fields, .. }) = &unit.param {
                for (sym_local, default) in fields {
                    let name = c
                        .module
                        .symbols
                        .get(*sym_local as usize)
                        .cloned()
                        .unwrap_or_default();
                    let g = vm.intern(&name);
                    out.insert(g, Slot::value(Value::Bool(default.is_some())));
                }
            }
            Ok(Value::Attrs(Rc::new(out)))
        }
        Value::Builtin(_) => Ok(Value::Attrs(Rc::new(BTreeMap::new()))),
        other => Err(VmError::eval(format!(
            "'functionArgs' requires a function, got {}",
            type_name(other)
        ))),
    }
}

// -- type tests and arithmetic ----------------------------------------------

pub fn bi_type_of(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let t = match argv(args, 0)? {
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::Null => "null",
        Value::Str(_) => "string",
        Value::Path(_) => "path",
        Value::List(_) => "list",
        Value::Attrs(_) => "set",
        Value::Closure(_) | Value::Builtin(_) => "lambda",
    };
    Ok(Value::Str(t.into()))
}

macro_rules! type_test {
    ($name:ident, $pat:pat) => {
        pub fn $name(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
            Ok(Value::Bool(matches!(argv(args, 0)?, $pat)))
        }
    };
}

type_test!(bi_is_int, Value::Int(_));
type_test!(bi_is_float, Value::Float(_));
type_test!(bi_is_bool, Value::Bool(_));
type_test!(bi_is_string, Value::Str(_));
type_test!(bi_is_path, Value::Path(_));
type_test!(bi_is_list, Value::List(_));
type_test!(bi_is_attrs, Value::Attrs(_));
type_test!(bi_is_function, Value::Closure(_) | Value::Builtin(_));

pub fn bi_add(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    num_op(vm, args, i64::checked_add, |a, b| a + b)
}

pub fn bi_sub(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    num_op(vm, args, i64::checked_sub, |a, b| a - b)
}

pub fn bi_mul(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    num_op(vm, args, i64::checked_mul, |a, b| a * b)
}

pub fn bi_div(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let a = argv(args, 0)?;
    let b = argv(args, 1)?;
    if matches!(b, Value::Int(0)) {
        return Err(VmError::eval("division by zero"));
    }
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        return x
            .checked_div(*y)
            .map(Value::Int)
            .ok_or_else(|| VmError::eval("integer overflow"));
    }
    let (x, y) = (as_f64(&a)?, as_f64(&b)?);
    Ok(Value::Float(x / y))
}

fn num_op(
    _vm: &mut Vm,
    args: &[Slot],
    int_op: fn(i64, i64) -> Option<i64>,
    float_op: fn(f64, f64) -> f64,
) -> Result<Value> {
    let a = argv(args, 0)?;
    let b = argv(args, 1)?;
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        return int_op(*x, *y)
            .map(Value::Int)
            .ok_or_else(|| VmError::eval("integer overflow"));
    }
    let (x, y) = (as_f64(&a)?, as_f64(&b)?);
    Ok(Value::Float(float_op(x, y)))
}

fn as_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        other => Err(VmError::eval(format!(
            "expected an integer or float but found {}",
            type_name(other)
        ))),
    }
}

/// cppnix's lessThan is the same CompareValues `<` uses, so it orders lists
/// lexicographically too; a scalar-only version fails eval-okay-sort, which
/// sorts a list of lists.
pub fn bi_less_than(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let a = argv(args, 0)?;
    let b = argv(args, 1)?;
    Ok(Begin::Sub(Task::compare(a, b, false)))
}

pub fn bi_bit_and(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    bit_op(vm, args, |a, b| a & b)
}

pub fn bi_bit_or(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    bit_op(vm, args, |a, b| a | b)
}

pub fn bi_bit_xor(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    bit_op(vm, args, |a, b| a ^ b)
}

fn bit_op(_vm: &mut Vm, args: &[Slot], op: fn(i64, i64) -> i64) -> Result<Value> {
    let a = want_int(&argv(args, 0)?)?;
    let b = want_int(&argv(args, 1)?)?;
    Ok(Value::Int(op(a, b)))
}

pub fn bi_floor(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Int(as_f64(&argv(args, 0)?)?.floor() as i64))
}

pub fn bi_ceil(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Int(as_f64(&argv(args, 0)?)?.ceil() as i64))
}

pub fn bi_compare_versions(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let a = want_str(&argv(args, 0)?)?;
    let b = want_str(&argv(args, 1)?)?;
    Ok(Value::Int(match compare_versions(&a, &b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }))
}

/// Split into digit and non-digit runs; '.' and '-' separate.
fn version_parts(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut cur_digit = None::<bool>;
    for c in s.chars() {
        if c == '.' || c == '-' {
            if !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
            cur_digit = None;
            continue;
        }
        let d = c.is_ascii_digit();
        if cur_digit.is_some() && cur_digit != Some(d) && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        }
        cur.push(c);
        cur_digit = Some(d);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// cppnix compareVersions: numeric runs compare numerically, "pre" sorts
/// before everything, letters compare lexically, absent sorts before present
/// except "pre".
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (pa, pb) = (version_parts(a), version_parts(b));
    let n = pa.len().max(pb.len());
    for i in 0..n {
        let x = pa.get(i).map(String::as_str).unwrap_or("");
        let y = pb.get(i).map(String::as_str).unwrap_or("");
        if x == y {
            continue;
        }
        let xn = x.parse::<i64>().ok();
        let yn = y.parse::<i64>().ok();
        let ord = match (xn, yn) {
            (Some(xi), Some(yi)) => xi.cmp(&yi),
            _ => {
                if x == "pre" {
                    Ordering::Less
                } else if y == "pre" || xn.is_some() {
                    Ordering::Greater
                } else if yn.is_some() {
                    Ordering::Less
                } else {
                    x.cmp(y)
                }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}
