//! Resumable frames for everything that walks a value: structural equality,
//! ordering, `with`-scope resolution, printing, string coercion, and builtins
//! mid-flight. Each is a worklist plus a cursor, so the depth of the Nix value
//! costs heap, never host stack.
//!
//! A task advances by returning a `Yield`. The machine turns `Force`, `Apply`
//! and `Sub` into frames pushed above the task and hands the resulting value
//! back on the next step; `Done` completes it. A task therefore never calls
//! back into evaluation, which is the property that keeps the interpreter
//! flat and lets a suspension unwind to the scheduler with no host frames to
//! rebuild.

use crate::builtins2::{self, Cont};
use crate::print::{Coerce, Print};
use crate::value2::{Env, EnvNode, Slot, Sym, Value, type_name};
use crate::vm::{Result, Vm, VmError, forced};
use std::cmp::Ordering;
use std::rc::Rc;

/// What a task wants next.
pub enum Yield {
    Done(Value),
    /// Force this slot and step again with its value.
    Force(Slot),
    /// Apply this function and step again with the result.
    Apply(Value, Slot),
    /// Run this task and step again with its value.
    Sub(Task),
    /// Ask the scheduler about a path and step again with its answer. The
    /// only way a task reaches the filesystem: the VM itself performs no IO,
    /// so this leaves the machine through `Step::NeedPath` and comes back
    /// through `resume` with the frame chain untouched.
    Need(NeedPath),
}

/// What the scheduler is being asked for about one path.
#[derive(Debug, Clone)]
pub enum NeedPath {
    /// Resolved path and source text, for `import`.
    Import(String),
    Contents(String),
    Exists(String),
    Entries(String),
    Kind(String),
}

pub enum Task {
    Builtin {
        idx: u16,
        args: Vec<Slot>,
        cont: Cont,
    },
    DeepEq(DeepEq),
    Compare(Compare),
    ResolveWith(ResolveWith),
    Print(Print),
    Coerce(Coerce),
    ApplyChain(ApplyChain),
}

impl Task {
    pub fn builtin(idx: u16, args: Vec<Slot>) -> Task {
        Task::Builtin {
            idx,
            args,
            cont: Cont::Args(0),
        }
    }

    pub fn deep_eq(l: Value, r: Value, negate: bool) -> Task {
        Task::DeepEq(DeepEq::new(Slot::value(l), Slot::value(r), negate))
    }

    pub fn deep_eq_slots(l: Slot, r: Slot) -> Task {
        Task::DeepEq(DeepEq::new(l, r, false))
    }

    pub fn compare(l: Value, r: Value, negate: bool) -> Task {
        Task::Compare(Compare::new(l, r, negate))
    }

    pub fn resolve_with(env: Env, sym: Sym) -> Task {
        Task::ResolveWith(ResolveWith {
            node: env,
            sym,
            stage: Stage::Walk,
        })
    }

    pub fn coerce(slot: Slot) -> Task {
        Task::Coerce(Coerce::new(slot))
    }

    /// `f a b ...`, one application per step. Backs `SlotState::PendingApply`,
    /// so a lazily-applied value costs frames rather than host stack however
    /// many arguments it carries.
    pub fn apply_chain(f: Value, args: Vec<Slot>) -> Task {
        Task::ApplyChain(ApplyChain { f: Some(f), args, i: 0 })
    }

    pub fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        match self {
            Task::Builtin { idx, args, cont } => builtins2::drive(vm, *idx, args, cont, incoming),
            Task::DeepEq(d) => d.step(),
            Task::Compare(c) => c.step(),
            Task::ResolveWith(r) => r.step(vm, incoming),
            Task::Print(p) => p.step(vm, incoming),
            Task::Coerce(c) => c.step(incoming),
            Task::ApplyChain(a) => a.step(incoming),
        }
    }

    /// `tryEval` is the only barrier in the language: it turns a catchable
    /// failure from the expression it is forcing into a value. Every other
    /// frame lets the error keep unwinding.
    pub fn catch(&self, vm: &mut Vm, e: &VmError) -> Option<Value> {
        match self {
            Task::Builtin {
                cont: Cont::TryEval { started: true },
                ..
            } => match e {
                VmError::Throw(c) if c.catchable => Some(builtins2::try_eval_result(
                    vm,
                    false,
                    Value::Bool(false),
                )),
                _ => None,
            },
            _ => None,
        }
    }
}

/// A deferred application, spent one argument at a time.
pub struct ApplyChain {
    f: Option<Value>,
    args: Vec<Slot>,
    i: usize,
}

impl ApplyChain {
    fn step(&mut self, incoming: Option<Value>) -> Result<Yield> {
        if let Some(v) = incoming {
            self.f = Some(v);
        }
        let f = self
            .f
            .take()
            .ok_or_else(|| VmError::eval("internal: apply chain lost its function"))?;
        let Some(arg) = self.args.get(self.i).cloned() else {
            return Ok(Yield::Done(f));
        };
        self.i += 1;
        Ok(Yield::Apply(f, arg))
    }
}

// -- structural equality ----------------------------------------------------

/// One deferred comparison. `Fail` is a decided inequality queued behind the
/// comparisons cppnix would have performed first, so an attrset whose names
/// diverge at position k still forces (and can still throw on) the k values
/// before it, exactly as `eqValues` does.
enum Job {
    Pair(Slot, Slot),
    Fail,
}

pub struct DeepEq {
    work: Vec<Job>,
    cur: Option<(Slot, Slot)>,
    /// 0: the left side is being forced; 1: the right side is.
    stage: u8,
    negate: bool,
}

impl DeepEq {
    fn new(l: Slot, r: Slot, negate: bool) -> Self {
        DeepEq {
            work: vec![Job::Pair(l, r)],
            cur: None,
            stage: 0,
            negate,
        }
    }

    fn step(&mut self) -> Result<Yield> {
        if let Some((l, r)) = self.cur.clone() {
            if self.stage == 0 {
                self.stage = 1;
                return Ok(Yield::Force(r));
            }
            self.cur = None;
            self.stage = 0;
            // eqValues' opening `&v1 == &v2`: the same cell is equal to
            // itself whatever it holds, which is how `[f] == [f]` is true
            // for a shared element while `f == f` is false. Checked after
            // forcing, as cppnix does, so a throwing cell still throws.
            if l.id() != r.id() {
                let (lv, rv) = (forced(&l)?, forced(&r)?);
                if !self.shallow(&lv, &rv) {
                    return Ok(Yield::Done(Value::Bool(self.negate)));
                }
            }
        }
        match self.work.pop() {
            None => Ok(Yield::Done(Value::Bool(!self.negate))),
            Some(Job::Fail) => Ok(Yield::Done(Value::Bool(self.negate))),
            Some(Job::Pair(l, r)) => {
                self.cur = Some((l.clone(), r));
                self.stage = 0;
                Ok(Yield::Force(l))
            }
        }
    }

    /// Decide the pair as far as forced values allow, queueing children.
    /// `false` means definitely unequal.
    fn shallow(&mut self, l: &Value, r: &Value) -> bool {
        match (l, r) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => {
                (*a as f64) == *b
            }
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Null, Value::Null) => true,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Path(a), Value::Path(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                for (x, y) in a.iter().zip(b.iter()).rev() {
                    self.work.push(Job::Pair(x.clone(), y.clone()));
                }
                true
            }
            (Value::Attrs(a), Value::Attrs(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                // Names decide, but only after the values before the first
                // differing name have been compared.
                let stop = a
                    .keys()
                    .zip(b.keys())
                    .position(|(ka, kb)| ka != kb)
                    .unwrap_or(a.len());
                if stop < a.len() {
                    self.work.push(Job::Fail);
                }
                for (x, y) in a.values().zip(b.values()).take(stop).rev() {
                    self.work.push(Job::Pair(x.clone(), y.clone()));
                }
                true
            }
            // cppnix's eqValues: "functions are incomparable", with no
            // pointer fallback, so `let f = x: x; in f == f` is false. Two
            // sides that are literally the same cell never reach here --
            // `step` settles those by slot identity, mirroring the
            // `&v1 == &v2` short circuit eqValues opens with.
            (Value::Closure(_) | Value::Builtin(_), Value::Closure(_) | Value::Builtin(_)) => false,
            _ => false,
        }
    }
}

// -- ordering ---------------------------------------------------------------

struct ListPos {
    a: Rc<Vec<Slot>>,
    b: Rc<Vec<Slot>>,
    i: usize,
}

/// `a < b`, optionally negated. Lists compare lexicographically with a length
/// tiebreak, which the explicit position stack turns into a depth-first walk
/// rather than recursion.
pub struct Compare {
    stack: Vec<ListPos>,
    cur: Option<(Slot, Slot)>,
    stage: u8,
    negate: bool,
}

impl Compare {
    fn new(l: Value, r: Value, negate: bool) -> Self {
        Compare {
            stack: Vec::new(),
            // Both sides arrive forced from the operator that built us.
            cur: Some((Slot::value(l), Slot::value(r))),
            stage: 1,
            negate,
        }
    }

    fn step(&mut self) -> Result<Yield> {
        loop {
            if let Some((l, r)) = self.cur.clone() {
                if self.stage == 0 {
                    self.stage = 1;
                    return Ok(Yield::Force(r));
                }
                self.cur = None;
                self.stage = 0;
                let (lv, rv) = (forced(&l)?, forced(&r)?);
                match (&lv, &rv) {
                    (Value::List(a), Value::List(b)) => self.stack.push(ListPos {
                        a: a.clone(),
                        b: b.clone(),
                        i: 0,
                    }),
                    _ => {
                        let ord = scalar_cmp(&lv, &rv)?;
                        if ord != Ordering::Equal {
                            return Ok(Yield::Done(Value::Bool(
                                (ord == Ordering::Less) != self.negate,
                            )));
                        }
                    }
                }
            }
            let Some(last) = self.stack.len().checked_sub(1) else {
                // Everything compared equal, so `a < b` is false.
                return Ok(Yield::Done(Value::Bool(self.negate)));
            };
            let (alen, blen, i, x, y) = {
                let pos = self
                    .stack
                    .get(last)
                    .ok_or_else(|| VmError::eval("internal: compare position lost"))?;
                (
                    pos.a.len(),
                    pos.b.len(),
                    pos.i,
                    pos.a.get(pos.i).cloned(),
                    pos.b.get(pos.i).cloned(),
                )
            };
            if i >= alen || i >= blen {
                self.stack.pop();
                if alen != blen {
                    return Ok(Yield::Done(Value::Bool((alen < blen) != self.negate)));
                }
                continue;
            }
            if let Some(pos) = self.stack.get_mut(last) {
                pos.i += 1;
            }
            let (Some(x), Some(y)) = (x, y) else {
                return Err(VmError::eval("internal: compare element lost"));
            };
            self.cur = Some((x.clone(), y));
            self.stage = 0;
            return Ok(Yield::Force(x));
        }
    }
}

fn scalar_cmp(l: &Value, r: &Value) -> Result<Ordering> {
    let ord = match (l, r) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
        (Value::Str(a), Value::Str(b)) => a.as_ref().cmp(b.as_ref()),
        (Value::Path(a), Value::Path(b)) => a.as_ref().cmp(b.as_ref()),
        _ => {
            return Err(VmError::eval(format!(
                "cannot compare {} with {}",
                type_name(l),
                type_name(r)
            )));
        }
    };
    Ok(ord)
}

// -- with-scope resolution --------------------------------------------------

enum Stage {
    Walk,
    AwaitSubject,
    AwaitValue,
}

pub struct ResolveWith {
    node: Env,
    sym: Sym,
    stage: Stage,
}

impl ResolveWith {
    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        let mut incoming = incoming;
        loop {
            match self.stage {
                Stage::AwaitValue => {
                    let v = incoming
                        .take()
                        .ok_or_else(|| VmError::eval("internal: with-resolve lost its value"))?;
                    return Ok(Yield::Done(v));
                }
                Stage::AwaitSubject => {
                    let v = incoming
                        .take()
                        .ok_or_else(|| VmError::eval("internal: with-resolve lost its subject"))?;
                    if let Value::Attrs(m) = v
                        && let Some(s) = m.get(&self.sym)
                    {
                        let s = s.clone();
                        self.stage = Stage::AwaitValue;
                        return Ok(Yield::Force(s));
                    }
                    self.stage = Stage::Walk;
                }
                Stage::Walk => {
                    let next = match &*self.node {
                        EnvNode::With { up, subject } => {
                            let (up, subject) = (up.clone(), subject.clone());
                            self.node = up;
                            self.stage = Stage::AwaitSubject;
                            return Ok(Yield::Force(subject));
                        }
                        EnvNode::Frame { up, .. } => up.clone(),
                        EnvNode::Root => {
                            return Err(VmError::eval(format!(
                                "undefined variable '{}'",
                                vm.sym_name(self.sym)
                            )));
                        }
                    };
                    self.node = next;
                }
            }
        }
    }
}
