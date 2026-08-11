//! The evaluator's machine. Everything the interpreter does lives on an
//! explicit heap-allocated frame stack: forcing a thunk, applying a closure,
//! running a builtin and walking a value all push frames rather than host
//! stack. Nothing here recurses on the host stack proportionally to the Nix
//! value or call depth, so a 100k-deep list is an ordinary allocation rather
//! than a SIGSEGV.
//!
//! The loop is poll-shaped: `poll` runs until the program produces a value or
//! asks the scheduler for something (`Step::Perform` / `Step::NeedPath`), and
//! the scheduler answers with `resume`. The VM itself performs no IO. Nothing
//! suspends yet -- the effects kernel (ENG-12068) is the first producer -- but
//! the frame chain is already the only interpreter state, so a suspension is a
//! plain return from `poll` rather than a stack unwind.

use crate::builtins;
use crate::ir::{Const, Module, Op, Param};
use crate::print;
use crate::task::{NeedPath, Task, Yield};
use crate::value2::{
    BuiltinData, ClosureData, Env, EnvNode, Slot, SlotState, Sym, Value, format_g6, type_name,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// An error the language can observe (tryEval) or report. `catchable` marks
/// the classes tryEval intercepts (throw and assert, not abort or type
/// errors), mirroring cppnix.
#[derive(Debug, Clone)]
pub struct Catchable {
    pub message: String,
    pub catchable: bool,
    pub kind: ErrKind,
}

/// Which cppnix exception class a failure corresponds to. The bridge needs
/// it to raise the matching C++ type and trace note: cppnix reports a throw
/// as ThrownError under "while calling the 'throw' builtin", and a reader
/// (or a differ classifying by text) cannot tell a throw from any other
/// failure without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// An ordinary evaluation error (type errors, missing attributes, ...).
    Eval,
    /// `builtins.throw`.
    Thrown,
    /// A failed `assert`.
    Assertion,
}

#[derive(Debug)]
pub enum VmError {
    Throw(Catchable),
    Unimplemented(String),
}

impl VmError {
    pub fn eval(msg: impl Into<String>) -> Self {
        VmError::Throw(Catchable {
            message: msg.into(),
            catchable: false,
            kind: ErrKind::Eval,
        })
    }

    /// `builtins.throw`: catchable by tryEval, reported as cppnix's
    /// ThrownError.
    pub fn thrown(msg: impl Into<String>) -> Self {
        VmError::Throw(Catchable {
            message: msg.into(),
            catchable: true,
            kind: ErrKind::Thrown,
        })
    }

    /// A failed assertion: catchable, reported as cppnix's AssertionError.
    pub fn assertion(msg: impl Into<String>) -> Self {
        VmError::Throw(Catchable {
            message: msg.into(),
            catchable: true,
            kind: ErrKind::Assertion,
        })
    }
}

pub type Result<T> = std::result::Result<T, VmError>;

/// What one `poll` returns. `Perform` and `NeedPath` hand control to the
/// scheduler, which answers through `Vm::resume`; the frame chain stays
/// intact across the gap, so a suspended evaluation is resumable and (once
/// frames serialize) snapshotable.
#[derive(Debug)]
pub enum Step {
    Done(Value),
    Perform {
        domain: String,
        request: Vec<u8>,
        resume: ResumeToken,
    },
    NeedPath {
        need: NeedPath,
        resume: ResumeToken,
    },
}

/// Names one outstanding suspension. Minted by a suspend, spent by `resume`;
/// a stale token is refused rather than silently answering the wrong wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeToken(u64);

/// One operand-stack entry: strict value, lazy slot, or the soft-select miss
/// marker.
#[derive(Debug, Clone)]
pub enum StackEntry {
    Val(Value),
    Lazy(Slot),
    Miss,
}

/// Where a completed sub-evaluation's value lands in the frame that asked
/// for it: appended, or written back over the entry that was forced in place.
#[derive(Debug)]
enum Dest {
    Push,
    Stack(usize),
}

#[derive(Debug)]
struct UnitFrame {
    module: Rc<Module>,
    unit: u32,
    ip: usize,
    stack: Vec<StackEntry>,
    env: Env,
    dest: Dest,
}

/// Forcing one slot. `entered` marks that the slot has been blackholed and
/// its thunk is running above us, which is what makes write-back and failure
/// memoization on the way out unambiguous.
#[derive(Debug)]
struct ForceFrame {
    slot: Slot,
    entered: bool,
}

#[derive(Debug)]
struct ApplyFrame {
    f: Value,
    arg: Slot,
}

enum Frame {
    Unit(UnitFrame),
    Force(ForceFrame),
    Apply(ApplyFrame),
    Task(Task),
}

/// The one piece of interpreter state outside the frames: what to do next.
enum Flow {
    /// Advance the topmost frame.
    Advance,
    /// Hand this value to the topmost frame (or halt when there is none).
    Deliver(Value),
    /// Unwind, letting `Force` frames memoize failures and `tryEval` catch.
    Unwind(VmError),
}

pub struct Vm {
    /// Global interner; module-local symbols map through `msym`.
    pub interner: Vec<String>,
    interner_idx: BTreeMap<String, Sym>,
    frames: Vec<Frame>,
    flow: Flow,
    /// Set while the scheduler owes us an answer.
    suspended: Option<u64>,
    next_token: u64,
    /// A path question raised by the frame on top, waiting to leave `poll`.
    pending_need: Option<NeedPath>,
    /// Compiled imports, keyed by resolved path. cppnix caches the same way,
    /// and without it a file imported from n places is compiled n times.
    modules: BTreeMap<String, Rc<Module>>,
}

impl Default for Vm {
    fn default() -> Self {
        Vm::new()
    }
}

impl Vm {
    pub fn new() -> Self {
        Vm {
            interner: Vec::new(),
            interner_idx: BTreeMap::new(),
            frames: Vec::new(),
            flow: Flow::Advance,
            suspended: None,
            next_token: 0,
            pending_need: None,
            modules: BTreeMap::new(),
        }
    }

    /// The compiled module for an imported file, compiling it on first use.
    /// Compilation is pure, so it happens here rather than in the scheduler.
    pub fn import_module(&mut self, path: &str, text: &str, base: &str) -> Result<Rc<Module>> {
        if let Some(m) = self.modules.get(path) {
            return Ok(m.clone());
        }
        let module = Rc::new(crate::compile::compile_source(text, base).map_err(|e| match e {
            crate::compile::CompileError::Unimplemented(w) => VmError::Unimplemented(w),
            crate::compile::CompileError::UndefinedVariable(n) => {
                VmError::eval(format!("undefined variable '{n}'"))
            }
            crate::compile::CompileError::Parse(m) => {
                VmError::eval(format!("in imported file '{path}': {m}"))
            }
        })?);
        self.modules.insert(path.to_owned(), module.clone());
        Ok(module)
    }

    pub fn intern(&mut self, s: &str) -> Sym {
        if let Some(&i) = self.interner_idx.get(s) {
            return i;
        }
        let i = self.interner.len() as Sym;
        self.interner.push(s.to_owned());
        self.interner_idx.insert(s.to_owned(), i);
        i
    }

    pub fn sym_name(&self, s: Sym) -> &str {
        self.interner
            .get(s as usize)
            .map(String::as_str)
            .unwrap_or("<sym?>")
    }

    /// Map a module-local symbol to the global interner.
    fn msym(&mut self, module: &Module, sym: u32) -> Result<Sym> {
        let name = module
            .symbols
            .get(sym as usize)
            .ok_or_else(|| VmError::eval("internal: bad symbol index"))?
            .clone();
        Ok(self.intern(&name))
    }

    // -- scheduler surface -------------------------------------------------

    /// Seed the machine with a module's entry unit.
    pub fn start_module(&mut self, module: &Rc<Module>) {
        self.frames.clear();
        self.flow = Flow::Advance;
        self.suspended = None;
        self.pending_need = None;
        let env: Env = Rc::new(EnvNode::Root);
        self.push_unit(module.clone(), module.entry, env);
    }

    /// Seed the machine with the strict printer over an already-evaluated
    /// value. The result is a `Value::Str` holding the rendered text; going
    /// through the machine is what keeps printing a deep structure iterative.
    pub fn start_print(&mut self, v: Value) {
        self.frames.clear();
        self.flow = Flow::Advance;
        self.suspended = None;
        self.pending_need = None;
        self.frames
            .push(Frame::Task(Task::Print(print::Print::new(v))));
    }

    /// Run until the program finishes or asks the scheduler for something.
    pub fn poll(&mut self) -> Result<Step> {
        if self.suspended.is_some() {
            return Err(VmError::eval("internal: poll while a suspension is open"));
        }
        loop {
            if let Some(need) = self.pending_need.take() {
                return Ok(self.suspend_path(need));
            }
            match std::mem::replace(&mut self.flow, Flow::Advance) {
                Flow::Advance => {
                    if let Err(e) = self.advance() {
                        self.flow = Flow::Unwind(e);
                    }
                }
                Flow::Deliver(v) => {
                    if self.frames.is_empty() {
                        return Ok(Step::Done(v));
                    }
                    if let Err(e) = self.deliver(v) {
                        self.flow = Flow::Unwind(e);
                    }
                }
                Flow::Unwind(e) => {
                    if let Some(err) = self.unwind(e) {
                        return Err(err);
                    }
                }
            }
        }
    }

    /// Suspend for an effect the scheduler must run. The frame chain stays
    /// put; the answer arrives through `resume`.
    pub fn suspend_perform(&mut self, domain: String, request: Vec<u8>) -> Step {
        Step::Perform {
            domain,
            request,
            resume: self.mint_token(),
        }
    }

    /// Suspend for a path the scheduler must answer.
    pub fn suspend_path(&mut self, need: NeedPath) -> Step {
        Step::NeedPath {
            need,
            resume: self.mint_token(),
        }
    }

    /// Answer an outstanding suspension. The value is delivered to the frame
    /// that suspended, exactly as a completed sub-evaluation would be.
    pub fn resume(&mut self, token: ResumeToken, value: Value) -> Result<()> {
        if self.suspended != Some(token.0) {
            return Err(VmError::eval("internal: resume with a stale token"));
        }
        self.suspended = None;
        self.flow = Flow::Deliver(value);
        Ok(())
    }

    fn mint_token(&mut self) -> ResumeToken {
        self.next_token = self.next_token.wrapping_add(1);
        self.suspended = Some(self.next_token);
        ResumeToken(self.next_token)
    }

    // -- the loop ----------------------------------------------------------

    fn advance(&mut self) -> Result<()> {
        let Some(frame) = self.frames.pop() else {
            return Err(VmError::eval("internal: nothing left to advance"));
        };
        match frame {
            Frame::Unit(u) => self.advance_unit(u),
            Frame::Force(f) => self.advance_force(f),
            Frame::Apply(a) => self.advance_apply(a),
            Frame::Task(t) => self.advance_task(t, None),
        }
    }

    fn deliver(&mut self, v: Value) -> Result<()> {
        let Some(frame) = self.frames.pop() else {
            return Err(VmError::eval("internal: delivery with no frame"));
        };
        match frame {
            Frame::Unit(mut u) => {
                match std::mem::replace(&mut u.dest, Dest::Push) {
                    Dest::Push => u.stack.push(StackEntry::Val(v)),
                    Dest::Stack(i) => {
                        let Some(e) = u.stack.get_mut(i) else {
                            return Err(VmError::eval("internal: stale force destination"));
                        };
                        *e = StackEntry::Val(v);
                    }
                }
                self.frames.push(Frame::Unit(u));
                self.flow = Flow::Advance;
                Ok(())
            }
            Frame::Force(f) => {
                if !f.entered {
                    return Err(VmError::eval("internal: delivery to an unentered force"));
                }
                *f.slot.0.borrow_mut() = SlotState::Value(v.clone());
                self.flow = Flow::Deliver(v);
                Ok(())
            }
            Frame::Apply(a) => self.advance_apply(a),
            Frame::Task(t) => self.advance_task(t, Some(v)),
        }
    }

    /// Pop frames until something catches. `Force` frames memoize the failure
    /// into their slot on the way past, which is how cppnix makes re-forcing a
    /// throwing thunk rethrow the same error.
    fn unwind(&mut self, e: VmError) -> Option<VmError> {
        while let Some(frame) = self.frames.pop() {
            match frame {
                Frame::Force(f) => {
                    if f.entered && let VmError::Throw(c) = &e {
                        *f.slot.0.borrow_mut() = SlotState::Failed(Rc::new(c.clone()));
                    }
                }
                Frame::Task(t) => {
                    if let Some(v) = t.catch(self, &e) {
                        self.flow = Flow::Deliver(v);
                        return None;
                    }
                }
                Frame::Unit(_) | Frame::Apply(_) => {}
            }
        }
        Some(e)
    }

    fn advance_force(&mut self, f: ForceFrame) -> Result<()> {
        if f.entered {
            return Err(VmError::eval("internal: re-entered a running force"));
        }
        let state = std::mem::replace(&mut *f.slot.0.borrow_mut(), SlotState::Blackhole);
        match state {
            SlotState::Value(v) => {
                *f.slot.0.borrow_mut() = SlotState::Value(v.clone());
                self.flow = Flow::Deliver(v);
                Ok(())
            }
            // The slot was already blackholed by a force frame further down,
            // so this is a genuine cycle rather than a second reader.
            SlotState::Blackhole => Err(VmError::eval("infinite recursion encountered")),
            SlotState::Unimplemented(name) => {
                let msg = name.to_string();
                *f.slot.0.borrow_mut() = SlotState::Unimplemented(name);
                Err(VmError::Unimplemented(msg))
            }
            SlotState::Failed(c) => {
                let err = (*c).clone();
                *f.slot.0.borrow_mut() = SlotState::Failed(c);
                Err(VmError::Throw(err))
            }
            SlotState::Thunk { module, unit, env } => {
                self.frames.push(Frame::Force(ForceFrame {
                    slot: f.slot.clone(),
                    entered: true,
                }));
                self.push_unit(module, unit, env);
                self.flow = Flow::Advance;
                Ok(())
            }
            SlotState::PendingApply { f: func, args } => {
                self.frames.push(Frame::Force(ForceFrame {
                    slot: f.slot.clone(),
                    entered: true,
                }));
                self.frames.push(Frame::Task(Task::apply_chain(func, args)));
                self.flow = Flow::Advance;
                Ok(())
            }
        }
    }

    fn advance_apply(&mut self, a: ApplyFrame) -> Result<()> {
        match &a.f {
            Value::Closure(c) => {
                let c = c.clone();
                let param = c
                    .module
                    .units
                    .get(c.unit as usize)
                    .ok_or_else(|| VmError::eval("internal: bad closure unit"))?
                    .param
                    .clone();
                match param {
                    Some(Param::Ident(_)) => {
                        let frame: Env = Rc::new(EnvNode::Frame {
                            up: c.env.clone(),
                            slots: RefCell::new(vec![a.arg.clone()]),
                        });
                        self.push_unit(c.module.clone(), c.unit, frame);
                        self.flow = Flow::Advance;
                        Ok(())
                    }
                    Some(Param::Formals {
                        fields,
                        ellipsis,
                        bind,
                    }) => {
                        let Some(v) = a.arg.peek() else {
                            // Destructuring needs the argument itself; come
                            // back to this same dispatch once it is forced.
                            let slot = a.arg.clone();
                            self.frames.push(Frame::Apply(a));
                            self.frames.push(Frame::Force(ForceFrame {
                                slot,
                                entered: false,
                            }));
                            self.flow = Flow::Advance;
                            return Ok(());
                        };
                        let map = match &v {
                            Value::Attrs(m) => m.clone(),
                            other => {
                                return Err(VmError::eval(format!(
                                    "expected a set but found {}: {other}",
                                    type_name(other)
                                )));
                            }
                        };
                        // Slot order: fields in declaration order, then @.
                        let frame: Env = Rc::new(EnvNode::Frame {
                            up: c.env.clone(),
                            slots: RefCell::new(Vec::new()),
                        });
                        let mut slots = Vec::new();
                        for (fsym_local, default_unit) in &fields {
                            let name = c
                                .module
                                .symbols
                                .get(*fsym_local as usize)
                                .cloned()
                                .unwrap_or_default();
                            let g = self.intern(&name);
                            match map.get(&g) {
                                Some(s) => slots.push(s.clone()),
                                None => match default_unit {
                                    Some(unit) => slots.push(Slot::thunk(
                                        c.module.clone(),
                                        *unit,
                                        frame.clone(),
                                    )),
                                    None => {
                                        return Err(VmError::eval(format!(
                                            "function called without required argument '{name}'"
                                        )));
                                    }
                                },
                            }
                        }
                        if bind.is_some() {
                            slots.push(a.arg.clone());
                        }
                        if !ellipsis {
                            for k in map.keys() {
                                let name = self.sym_name(*k).to_owned();
                                let known = fields.iter().any(|(fs, _)| {
                                    c.module.symbols.get(*fs as usize).map(String::as_str)
                                        == Some(name.as_str())
                                });
                                if !known {
                                    return Err(VmError::eval(format!(
                                        "function called with unexpected argument '{name}'"
                                    )));
                                }
                            }
                        }
                        if let EnvNode::Frame { slots: fslots, .. } = &*frame {
                            *fslots.borrow_mut() = slots;
                        }
                        self.push_unit(c.module.clone(), c.unit, frame);
                        self.flow = Flow::Advance;
                        Ok(())
                    }
                    None => Err(VmError::eval("internal: closure without param")),
                }
            }
            Value::Builtin(b) => {
                let arity = builtins::TABLE
                    .get(b.idx as usize)
                    .ok_or_else(|| VmError::eval("internal: bad builtin index"))?
                    .arity;
                let mut args = b.args.clone();
                args.push(a.arg.clone());
                if args.len() < arity {
                    self.flow = Flow::Deliver(Value::Builtin(Rc::new(BuiltinData {
                        idx: b.idx,
                        args,
                    })));
                } else {
                    self.frames.push(Frame::Task(Task::builtin(b.idx, args)));
                    self.flow = Flow::Advance;
                }
                Ok(())
            }
            other => Err(VmError::eval(format!(
                "attempt to call something which is not a function but {}",
                type_name(other)
            ))),
        }
    }

    fn advance_task(&mut self, t: Task, incoming: Option<Value>) -> Result<()> {
        match self.advance_task_step(t, incoming)? {
            None => Ok(()),
            Some(need) => {
                self.pending_need = Some(need);
                Ok(())
            }
        }
    }

    /// Returns the path question the task asked for, if any; the caller turns
    /// it into the `Step` that leaves `poll`.
    fn advance_task_step(
        &mut self,
        mut t: Task,
        incoming: Option<Value>,
    ) -> Result<Option<NeedPath>> {
        let y = match t.step(self, incoming) {
            Ok(y) => y,
            Err(e) => {
                // Put the frame back so `unwind` still gives it the chance to
                // catch, which is what makes tryEval's barrier unconditional.
                self.frames.push(Frame::Task(t));
                return Err(e);
            }
        };
        match y {
            Yield::Need(need) => {
                self.frames.push(Frame::Task(t));
                return Ok(Some(need));
            }
            Yield::Done(v) => self.flow = Flow::Deliver(v),
            Yield::Force(slot) => {
                self.frames.push(Frame::Task(t));
                self.frames.push(Frame::Force(ForceFrame {
                    slot,
                    entered: false,
                }));
                self.flow = Flow::Advance;
            }
            Yield::Apply(f, arg) => {
                self.frames.push(Frame::Task(t));
                self.frames.push(Frame::Apply(ApplyFrame { f, arg }));
                self.flow = Flow::Advance;
            }
            Yield::Sub(sub) => {
                self.frames.push(Frame::Task(t));
                self.frames.push(Frame::Task(sub));
                self.flow = Flow::Advance;
            }
        }
        Ok(None)
    }

    fn push_unit(&mut self, module: Rc<Module>, unit: u32, env: Env) {
        self.frames.push(Frame::Unit(UnitFrame {
            module,
            unit,
            ip: 0,
            stack: Vec::new(),
            env,
            dest: Dest::Push,
        }));
    }

    fn yield_force(&mut self, u: UnitFrame, slot: Slot) -> Result<()> {
        self.frames.push(Frame::Unit(u));
        self.frames.push(Frame::Force(ForceFrame {
            slot,
            entered: false,
        }));
        self.flow = Flow::Advance;
        Ok(())
    }

    fn yield_task(&mut self, u: UnitFrame, t: Task) -> Result<()> {
        self.frames.push(Frame::Unit(u));
        self.frames.push(Frame::Task(t));
        self.flow = Flow::Advance;
        Ok(())
    }

    // -- executing one code unit -------------------------------------------

    fn advance_unit(&mut self, mut u: UnitFrame) -> Result<()> {
        loop {
            let fetched = u
                .module
                .units
                .get(u.unit as usize)
                .and_then(|c| c.ops.get(u.ip))
                .copied();
            let Some(op) = fetched else {
                // Falling off the end returns the stack top, as `Ret` does.
                if let Some(s) = strict_gap(&mut u, 1) {
                    return self.yield_force(u, s);
                }
                let v = pop_value(&mut u.stack)?;
                self.flow = Flow::Deliver(v);
                return Ok(());
            };
            match op {
                Op::Ret => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let v = pop_value(&mut u.stack)?;
                    self.flow = Flow::Deliver(v);
                    return Ok(());
                }
                Op::Const(idx) => {
                    let c = u
                        .module
                        .consts
                        .get(idx as usize)
                        .ok_or_else(|| VmError::eval("internal: bad const index"))?;
                    if let Const::Str(s) = c {
                        check_no_nul(s)?;
                    }
                    u.stack.push(StackEntry::Val(const_value(c)));
                    u.ip += 1;
                }
                Op::GetLocal { depth, slot } => {
                    let s = lookup_local(&u.env, depth, slot)?;
                    u.ip += 1;
                    return self.yield_force(u, s);
                }
                Op::GetLocalLazy { depth, slot } => {
                    let s = lookup_local(&u.env, depth, slot)?;
                    u.stack.push(StackEntry::Lazy(s));
                    u.ip += 1;
                }
                Op::Builtin { idx } => {
                    u.stack.push(StackEntry::Val(builtins::mk_value(idx)));
                    u.ip += 1;
                }
                Op::BuiltinsSet => {
                    let v = builtins::builtins_set(self);
                    u.stack.push(StackEntry::Val(v));
                    u.ip += 1;
                }
                Op::UnimplementedGlobal { sym } => {
                    let g = self.msym(&u.module, sym)?;
                    return Err(VmError::Unimplemented(format!(
                        "global {}",
                        self.sym_name(g)
                    )));
                }
                Op::Thunk { unit } => {
                    u.stack.push(StackEntry::Lazy(Slot::thunk(
                        u.module.clone(),
                        unit,
                        u.env.clone(),
                    )));
                    u.ip += 1;
                }
                Op::Closure { unit } => {
                    u.stack.push(StackEntry::Val(Value::Closure(Rc::new(ClosureData {
                        module: u.module.clone(),
                        unit,
                        env: u.env.clone(),
                    }))));
                    u.ip += 1;
                }
                Op::Apply => {
                    // The callee sits under the argument; only it has to be
                    // strict here, because a lambda's argument stays lazy.
                    if let Some(s) = strict_at(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let f = entry_value(&u, 2)?;
                    let arg = pop_slot(&mut u.stack)?;
                    u.stack.pop();
                    u.ip += 1;
                    self.frames.push(Frame::Unit(u));
                    self.frames.push(Frame::Apply(ApplyFrame { f, arg }));
                    self.flow = Flow::Advance;
                    return Ok(());
                }
                Op::PushEnv { n } => {
                    let mut slots = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        slots.push(pop_slot(&mut u.stack)?);
                    }
                    slots.reverse();
                    // Bindings were compiled against the frame they live in:
                    // thunks on the stack captured the OLD env, but let/rec
                    // bodies need self-reference. compile_bindings compiled
                    // value thunks inside the new scope, so re-point them at
                    // the new frame.
                    let frame: Env = Rc::new(EnvNode::Frame {
                        up: u.env.clone(),
                        slots: RefCell::new(Vec::new()),
                    });
                    let repointed: Vec<Slot> = slots
                        .into_iter()
                        .map(|s| repoint_thunk(&s, &frame))
                        .collect();
                    if let EnvNode::Frame { slots, .. } = &*frame {
                        *slots.borrow_mut() = repointed;
                    }
                    u.env = frame;
                    u.ip += 1;
                }
                Op::PopEnv => {
                    u.env = match &*u.env {
                        EnvNode::Frame { up, .. } | EnvNode::With { up, .. } => up.clone(),
                        EnvNode::Root => return Err(VmError::eval("internal: env underflow")),
                    };
                    u.ip += 1;
                }
                Op::PushWith => {
                    let subject = pop_slot(&mut u.stack)?;
                    u.env = Rc::new(EnvNode::With {
                        up: u.env.clone(),
                        subject,
                    });
                    u.ip += 1;
                }
                Op::JumpIfFalse { target } => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let v = pop_value(&mut u.stack)?;
                    match v {
                        Value::Bool(false) => u.ip += 1 + target as usize,
                        Value::Bool(true) => u.ip += 1,
                        other => {
                            return Err(VmError::eval(format!(
                                "expected a Boolean but found {}: {other}",
                                type_name(&other)
                            )));
                        }
                    }
                }
                Op::Jump { target } => u.ip += 1 + target as usize,
                Op::Add | Op::Sub | Op::Mul | Op::Div => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    let out = self.arith(op, l, r)?;
                    u.stack.push(StackEntry::Val(out));
                    u.ip += 1;
                }
                Op::Eq | Op::Neq => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    u.ip += 1;
                    let negate = matches!(op, Op::Neq);
                    return self.yield_task(u, Task::deep_eq(l, r, negate));
                }
                Op::Lt | Op::Leq | Op::Gt | Op::Geq => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    u.ip += 1;
                    // Only `<` exists underneath: the other three are it with
                    // the operands swapped, negated, or both.
                    let (a, b, negate) = match op {
                        Op::Lt => (l, r, false),
                        Op::Gt => (r, l, false),
                        Op::Leq => (r, l, true),
                        _ => (l, r, true),
                    };
                    return self.yield_task(u, Task::compare(a, b, negate));
                }
                Op::Not => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    match pop_value(&mut u.stack)? {
                        Value::Bool(b) => u.stack.push(StackEntry::Val(Value::Bool(!b))),
                        other => {
                            return Err(VmError::eval(format!(
                                "expected a Boolean but found {}: {other}",
                                type_name(&other)
                            )));
                        }
                    }
                    u.ip += 1;
                }
                Op::Negate => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let neg = match pop_value(&mut u.stack)? {
                        Value::Int(n) => n
                            .checked_neg()
                            .map(Value::Int)
                            .ok_or_else(|| VmError::eval("integer overflow in negation"))?,
                        Value::Float(x) => Value::Float(-x),
                        other => {
                            return Err(VmError::eval(format!(
                                "expected an integer or float but found {}",
                                type_name(&other)
                            )));
                        }
                    };
                    u.stack.push(StackEntry::Val(neg));
                    u.ip += 1;
                }
                Op::ConcatStrings { n } => {
                    if let Some(s) = strict_gap(&mut u, n as usize) {
                        return self.yield_force(u, s);
                    }
                    let mut parts = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        parts.push(pop_value(&mut u.stack)?);
                    }
                    parts.reverse();
                    let mut out = String::new();
                    for p in parts {
                        out.push_str(&coerce_interpolated(&p)?);
                    }
                    check_no_nul(&out)?;
                    u.stack.push(StackEntry::Val(Value::Str(out.into())));
                    u.ip += 1;
                }
                Op::MkList { n } => {
                    let mut items = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        items.push(pop_slot(&mut u.stack)?);
                    }
                    items.reverse();
                    u.stack.push(StackEntry::Val(Value::List(Rc::new(items))));
                    u.ip += 1;
                }
                Op::ConcatLists => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    match (l, r) {
                        (Value::List(a), Value::List(b)) => {
                            let mut out = (*a).clone();
                            out.extend(b.iter().cloned());
                            u.stack.push(StackEntry::Val(Value::List(Rc::new(out))));
                        }
                        (l, _) => {
                            return Err(VmError::eval(format!(
                                "expected a list but found {}",
                                type_name(&l)
                            )));
                        }
                    }
                    u.ip += 1;
                }
                Op::MkAttrs { n, .. } => {
                    // Names are strict, values stay lazy: forcing a value here
                    // would make `{ a = throw "x"; } ? a` throw.
                    if let Some(s) = strict_names(&mut u, n) {
                        return self.yield_force(u, s);
                    }
                    let mut map = BTreeMap::new();
                    let mut pairs = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        let v = pop_slot(&mut u.stack)?;
                        let k = pop_value(&mut u.stack)?;
                        pairs.push((k, v));
                    }
                    pairs.reverse();
                    for (k, v) in pairs {
                        // cppnix skips a dynamic binding whose name evaluates
                        // to null, so `{ ${null} = true; }` is the empty set
                        // rather than an error.
                        if matches!(k, Value::Null) {
                            continue;
                        }
                        let name = match k {
                            Value::Str(s) => s.to_string(),
                            other => {
                                return Err(VmError::eval(format!(
                                    "expected a string but found {}: {other}",
                                    type_name(&other)
                                )));
                            }
                        };
                        let sym = self.intern(&name);
                        if map.insert(sym, v).is_some() {
                            return Err(VmError::eval(format!(
                                "attribute '{name}' already defined"
                            )));
                        }
                    }
                    u.stack.push(StackEntry::Val(Value::Attrs(Rc::new(map))));
                    u.ip += 1;
                }
                Op::Update => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    match (l, r) {
                        (Value::Attrs(a), Value::Attrs(b)) => {
                            let mut out = (*a).clone();
                            for (k, v) in b.iter() {
                                out.insert(*k, v.clone());
                            }
                            u.stack.push(StackEntry::Val(Value::Attrs(Rc::new(out))));
                        }
                        (l, r) => {
                            let bad = if matches!(l, Value::Attrs(_)) { r } else { l };
                            return Err(VmError::eval(format!(
                                "expected a set but found {}",
                                type_name(&bad)
                            )));
                        }
                    }
                    u.ip += 1;
                }
                Op::Select { sym } => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let g = self.msym(&u.module, sym)?;
                    let set = pop_value(&mut u.stack)?;
                    let slot = self.select_strict(&set, g)?;
                    u.ip += 1;
                    return self.yield_force(u, slot);
                }
                Op::SelectDyn => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let name = pop_value(&mut u.stack)?;
                    let set = pop_value(&mut u.stack)?;
                    let g = self.attr_sym(&name)?;
                    let slot = self.select_strict(&set, g)?;
                    u.ip += 1;
                    return self.yield_force(u, slot);
                }
                Op::SelectSoft { sym } => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let g = self.msym(&u.module, sym)?;
                    let top = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.ip += 1;
                    match top {
                        StackEntry::Miss => u.stack.push(StackEntry::Miss),
                        StackEntry::Val(set) => match select(&set, g) {
                            Some(slot) => return self.yield_force(u, slot),
                            None => u.stack.push(StackEntry::Miss),
                        },
                        StackEntry::Lazy(_) => return Err(unforced()),
                    }
                }
                Op::SelectSoftDyn => {
                    if let Some(s) = strict_at(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    if let Some(s) = strict_at(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let name = pop_value(&mut u.stack)?;
                    let g = self.attr_sym(&name)?;
                    let top = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.ip += 1;
                    match top {
                        StackEntry::Miss => u.stack.push(StackEntry::Miss),
                        StackEntry::Val(set) => match select(&set, g) {
                            Some(slot) => return self.yield_force(u, slot),
                            None => u.stack.push(StackEntry::Miss),
                        },
                        StackEntry::Lazy(_) => return Err(unforced()),
                    }
                }
                Op::OrDefault => {
                    let default = pop_slot(&mut u.stack)?;
                    let scrutinee = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.ip += 1;
                    match scrutinee {
                        StackEntry::Miss => return self.yield_force(u, default),
                        other => u.stack.push(other),
                    }
                }
                Op::HasAttr { sym } => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let g = self.msym(&u.module, sym)?;
                    let top = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.stack.push(StackEntry::Val(Value::Bool(has_attr(&top, g))));
                    u.ip += 1;
                }
                Op::HasAttrDyn => {
                    if let Some(s) = strict_at(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    if let Some(s) = strict_at(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let name = pop_value(&mut u.stack)?;
                    let g = self.attr_sym(&name)?;
                    let top = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.stack.push(StackEntry::Val(Value::Bool(has_attr(&top, g))));
                    u.ip += 1;
                }
                Op::ResolveWith { sym } => {
                    let g = self.msym(&u.module, sym)?;
                    let env = u.env.clone();
                    u.ip += 1;
                    return self.yield_task(u, Task::resolve_with(env, g));
                }
                Op::CallBuiltin { .. } => {
                    return Err(VmError::Unimplemented("CallBuiltin".into()));
                }
                Op::Assert => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    match pop_value(&mut u.stack)? {
                        Value::Bool(true) => {}
                        Value::Bool(false) => {
                            return Err(VmError::assertion("assertion failed"));
                        }
                        other => {
                            return Err(VmError::eval(format!(
                                "expected a Boolean but found {}",
                                type_name(&other)
                            )));
                        }
                    }
                    u.ip += 1;
                }
            }
        }
    }

    fn attr_sym(&mut self, name: &Value) -> Result<Sym> {
        match name {
            Value::Str(s) => Ok(self.intern(s)),
            // cppnix's wording, verbatim: "expected a string but found a set:
            // { }". The differ reads an error's class out of its text, and
            // this shape is what puts it in the `type` class.
            other => Err(VmError::eval(format!(
                "expected a string but found {}: {other}",
                type_name(other)
            ))),
        }
    }

    fn arith(&mut self, op: Op, l: Value, r: Value) -> Result<Value> {
        // String/path + coerces per cppnix.
        if matches!(op, Op::Add) {
            match (&l, &r) {
                (Value::Str(a), _) => {
                    let b = coerce_concat(&r)?;
                    return Ok(Value::Str(format!("{a}{b}").into()));
                }
                (Value::Path(a), _) => {
                    // A path stays canonical: cppnix normalizes the result,
                    // so `/foo/bar + "/../xyzzy"` is `/foo/xyzzy`. A string on
                    // the left does not, which is why the two orders differ.
                    let b = coerce_concat(&r)?;
                    return Ok(Value::Path(
                        crate::value2::normalize_path(&format!("{a}{b}")).into(),
                    ));
                }
                _ => {}
            }
        }
        match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => {
                let (a, b) = (*a, *b);
                let (out, verb) = match op {
                    Op::Add => (a.checked_add(b), "adding"),
                    Op::Sub => (a.checked_sub(b), "subtracting"),
                    Op::Mul => (a.checked_mul(b), "multiplying"),
                    Op::Div => {
                        if b == 0 {
                            return Err(VmError::eval("division by zero"));
                        }
                        (a.checked_div(b), "dividing")
                    }
                    _ => (None, "computing"),
                };
                let sign = match op {
                    Op::Add => "+",
                    Op::Sub => "-",
                    Op::Mul => "*",
                    _ => "/",
                };
                out.map(Value::Int).ok_or_else(|| {
                    VmError::eval(format!("integer overflow in {verb} {a} {sign} {b}"))
                })
            }
            _ => {
                let a = num_f64(&l)?;
                let b = num_f64(&r)?;
                let x = match op {
                    Op::Add => a + b,
                    Op::Sub => a - b,
                    Op::Mul => a * b,
                    Op::Div => a / b,
                    _ => return Err(VmError::eval("internal: bad arith op")),
                };
                Ok(Value::Float(x))
            }
        }
    }

    fn select_strict(&mut self, set: &Value, sym: Sym) -> Result<Slot> {
        match select(set, sym) {
            Some(s) => Ok(s),
            None => match set {
                Value::Attrs(_) => Err(VmError::eval(format!(
                    "attribute '{}' missing",
                    self.sym_name(sym)
                ))),
                other => Err(VmError::eval(format!(
                    "expected a set but found {}: {other}",
                    type_name(other)
                ))),
            },
        }
    }
}

fn select(set: &Value, sym: Sym) -> Option<Slot> {
    match set {
        Value::Attrs(map) => map.get(&sym).cloned(),
        _ => None,
    }
}

fn has_attr(e: &StackEntry, sym: Sym) -> bool {
    match e {
        StackEntry::Val(Value::Attrs(map)) => map.contains_key(&sym),
        _ => false,
    }
}

/// Force the topmost lazy entry among the top `k`, recording where its value
/// goes. Returns `None` when they are all strict and the op may run. Ops call
/// this before touching the stack, so re-running the op after the force is
/// what resumption costs -- no per-op phase bookkeeping.
fn strict_gap(u: &mut UnitFrame, k: usize) -> Option<Slot> {
    for off in 1..=k {
        if let Some(s) = strict_at(u, off) {
            return Some(s);
        }
    }
    None
}

/// As `strict_gap`, for exactly one position (1 is the top of the stack).
fn strict_at(u: &mut UnitFrame, off: usize) -> Option<Slot> {
    let i = u.stack.len().checked_sub(off)?;
    let StackEntry::Lazy(s) = u.stack.get(i)? else {
        return None;
    };
    let s = s.clone();
    u.dest = Dest::Stack(i);
    Some(s)
}

/// The attribute-name half of the `n` (name, value) pairs `MkAttrs` consumes,
/// topmost pair first (the order the op pops them in).
fn strict_names(u: &mut UnitFrame, n: u16) -> Option<Slot> {
    for j in 0..usize::from(n) {
        if let Some(s) = strict_at(u, 2 + 2 * j) {
            return Some(s);
        }
    }
    None
}

fn num_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        other => Err(VmError::eval(format!(
            "expected an integer or float but found {}",
            type_name(other)
        ))),
    }
}

/// cppnix refuses any string carrying an interior NUL, because a Nix string
/// reaches the store and the OS as a C string. Checked where strings are
/// built rather than where they are printed, so the failure names the input.
pub fn check_no_nul(s: &str) -> Result<()> {
    if s.contains('\0') {
        return Err(VmError::eval(format!(
            "input string '{}' cannot be represented as Nix string because it contains null bytes",
            s.replace('\0', "\u{2400}")
        )));
    }
    Ok(())
}

pub fn coerce_interpolated(v: &Value) -> Result<String> {
    match v {
        Value::Str(s) => Ok(s.to_string()),
        Value::Path(p) => Ok(p.to_string()),
        other => Err(VmError::eval(format!(
            "cannot coerce {} to a string",
            type_name(other)
        ))),
    }
}

pub fn coerce_concat(v: &Value) -> Result<String> {
    match v {
        Value::Str(s) => Ok(s.to_string()),
        Value::Path(p) => Ok(p.to_string()),
        Value::Int(n) => Ok(n.to_string()),
        Value::Float(x) => Ok(format_g6(*x)),
        other => Err(VmError::eval(format!(
            "cannot coerce {} to a string",
            type_name(other)
        ))),
    }
}

/// The memoized value of a slot the machine has already forced. Reaching the
/// error means an interpreter bug, not a Nix-level one: every value a builtin
/// or task is handed went through a `Force` frame first.
pub fn forced(s: &Slot) -> Result<Value> {
    s.peek()
        .ok_or_else(|| VmError::eval("internal: value read before it was forced"))
}

fn stack_underflow() -> VmError {
    VmError::eval("internal: stack underflow")
}

fn miss_escaped() -> VmError {
    VmError::eval("internal: miss escaped selection")
}

fn unforced() -> VmError {
    VmError::eval("internal: unforced stack entry")
}

fn pop_slot(stack: &mut Vec<StackEntry>) -> Result<Slot> {
    match stack.pop().ok_or_else(stack_underflow)? {
        StackEntry::Val(v) => Ok(Slot::value(v)),
        StackEntry::Lazy(s) => Ok(s),
        StackEntry::Miss => Err(miss_escaped()),
    }
}

fn pop_value(stack: &mut Vec<StackEntry>) -> Result<Value> {
    match stack.pop().ok_or_else(stack_underflow)? {
        StackEntry::Val(v) => Ok(v),
        StackEntry::Lazy(_) => Err(unforced()),
        StackEntry::Miss => Err(miss_escaped()),
    }
}

fn entry_value(u: &UnitFrame, off: usize) -> Result<Value> {
    let i = u.stack.len().checked_sub(off).ok_or_else(stack_underflow)?;
    match u.stack.get(i) {
        Some(StackEntry::Val(v)) => Ok(v.clone()),
        Some(StackEntry::Lazy(_)) => Err(unforced()),
        Some(StackEntry::Miss) => Err(miss_escaped()),
        None => Err(stack_underflow()),
    }
}

fn lookup_local(env: &Env, depth: u16, slot: u16) -> Result<Slot> {
    let mut node = env.clone();
    let mut d = depth;
    loop {
        match &*node {
            EnvNode::Frame { up, slots } => {
                if d == 0 {
                    return slots
                        .borrow()
                        .get(slot as usize)
                        .cloned()
                        .ok_or_else(|| VmError::eval("internal: bad local slot"));
                }
                d -= 1;
                node = up.clone();
            }
            EnvNode::With { up, .. } => {
                if d == 0 {
                    return Err(VmError::eval("internal: local depth hit with-scope"));
                }
                d -= 1;
                node = up.clone();
            }
            EnvNode::Root => return Err(VmError::eval("internal: local depth underflow")),
        }
    }
}

/// Rebase a thunk captured at compile-fill time onto the frame it belongs
/// to (let/rec self-reference).
fn repoint_thunk(s: &Slot, frame: &Env) -> Slot {
    let inner = s.0.borrow();
    match &*inner {
        SlotState::Thunk { module, unit, .. } => Slot::thunk(module.clone(), *unit, frame.clone()),
        _ => s.clone(),
    }
}

fn const_value(c: &Const) -> Value {
    match c {
        Const::Int(n) => Value::Int(*n),
        Const::Float(x) => Value::Float(*x),
        Const::Bool(b) => Value::Bool(*b),
        Const::Null => Value::Null,
        Const::Str(s) => Value::Str(s.clone().into()),
        Const::Path(p) => Value::Path(p.clone().into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::CodeUnit;

    /// The scheduler seam. No op produces a suspension yet -- the effects
    /// kernel is the first producer -- so the round trip is driven from where
    /// a handler will sit: suspend, refuse to poll, answer, and watch the
    /// frame that was mid-unit consume the answer as an ordinary operand.
    #[test]
    fn a_suspension_resumes_into_the_running_frame() -> std::result::Result<(), String> {
        // One unit that adds 40 to whatever is already on its stack.
        let module = Rc::new(Module {
            consts: vec![Const::Int(40)],
            symbols: Vec::new(),
            units: vec![CodeUnit {
                ops: vec![Op::Const(0), Op::Add, Op::Ret],
                param: None,
            }],
            entry: 0,
        });
        let mut vm = Vm::new();
        vm.start_module(&module);

        let Step::Perform {
            domain,
            request,
            resume,
        } = vm.suspend_perform("test".to_owned(), b"ping".to_vec())
        else {
            return Err("expected a Perform suspension".to_owned());
        };
        assert_eq!(domain, "test");
        assert_eq!(request, b"ping".to_vec());

        if vm.poll().is_ok() {
            return Err("poll must refuse while an answer is owed".to_owned());
        }
        vm.resume(resume, Value::Int(2))
            .map_err(|_| "resume rejected a live token".to_owned())?;
        // Spending the token twice is refused rather than answering a wait
        // that no longer exists.
        if vm.resume(resume, Value::Int(2)).is_ok() {
            return Err("a spent token must not resume again".to_owned());
        }

        let Ok(Step::Done(Value::Int(n))) = vm.poll() else {
            return Err("expected the unit to finish".to_owned());
        };
        assert_eq!(n, 42);
        Ok(())
    }
}
