//! Runtime values for the VM. `Value` is deliberately cheap to clone: every
//! aggregate is behind `Rc`. The two-word packed representation from the
//! architecture plan replaces this once semantics are complete; behavior
//! first, representation second, measured by the corpus differ throughout.

use crate::ir::Module;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

/// Interned symbol id within one VM instance.
pub type Sym = u32;

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// String with its (rarely present) context.
    Str(Rc<str>),
    Path(Rc<str>),
    List(Rc<Vec<Slot>>),
    /// Sorted by symbol id at construction; iteration order for printing is
    /// name-alphabetical, resolved through the interner at print time.
    Attrs(Rc<BTreeMap<Sym, Slot>>),
    Closure(Rc<ClosureData>),
    /// A builtin, possibly partially applied (arity > args.len()).
    Builtin(Rc<BuiltinData>),
}

#[derive(Debug)]
pub struct ClosureData {
    pub module: Rc<Module>,
    pub unit: u32,
    pub env: Env,
}

#[derive(Debug)]
pub struct BuiltinData {
    pub idx: u16,
    pub args: Vec<Slot>,
}

/// A lazily-evaluated cell: thunk until forced, then value forever.
#[derive(Debug, Clone)]
pub struct Slot(pub Rc<RefCell<SlotState>>);

#[derive(Debug)]
pub enum SlotState {
    Value(Value),
    Thunk {
        module: Rc<Module>,
        unit: u32,
        env: Env,
    },
    /// Under evaluation: hitting this is infinite recursion.
    Blackhole,
    /// A forced thunk whose evaluation threw. Re-forcing rethrows the same
    /// error (cppnix memoizes failures the same way).
    Failed(Rc<crate::vm::Catchable>),
    /// `f a b` not yet performed. cppnix's mkApp: map, genList and mapAttrs
    /// all build their results out of these, so `mapAttrs throw attrs` is a
    /// set of unexploded thunks rather than an immediate throw.
    PendingApply { f: Value, args: Vec<Slot> },
    /// A builtin (or other feature) the evaluator does not implement yet;
    /// forcing reports it as unimplemented, which the harnesses count
    /// separately from mismatches.
    Unimplemented(Rc<str>),
}

impl Slot {
    pub fn value(v: Value) -> Self {
        Slot(Rc::new(RefCell::new(SlotState::Value(v))))
    }

    pub fn thunk(module: Rc<Module>, unit: u32, env: Env) -> Self {
        Slot(Rc::new(RefCell::new(SlotState::Thunk { module, unit, env })))
    }

    pub fn pending(f: Value, args: Vec<Slot>) -> Self {
        Slot(Rc::new(RefCell::new(SlotState::PendingApply { f, args })))
    }

    pub fn unimplemented(what: &str) -> Self {
        Slot(Rc::new(RefCell::new(SlotState::Unimplemented(what.into()))))
    }

    /// The memoized value, or `None` when this slot has not been forced.
    /// The machine forces every value a builtin is allowed to look at, so a
    /// builtin reading `None` is an interpreter bug, never a Nix-level one.
    pub fn peek(&self) -> Option<Value> {
        match &*self.0.borrow() {
            SlotState::Value(v) => Some(v.clone()),
            _ => None,
        }
    }

    /// Identity of the cell, for cycle detection in deep traversals.
    pub fn id(&self) -> usize {
        Rc::as_ptr(&self.0) as usize
    }
}

/// One Nix expression routinely builds a value nested tens of thousands of
/// levels deep, and the derived drop glue recurses once per level, so a
/// teardown would blow the host stack right after an evaluation that never
/// touched it. Dismantle iteratively instead: take each cell's state out
/// behind a leaf and push its children onto a worklist.
impl Drop for Slot {
    fn drop(&mut self) {
        if Rc::strong_count(&self.0) != 1 {
            return;
        }
        let Ok(mut here) = self.0.try_borrow_mut() else {
            return;
        };
        let mut work = vec![Junk::State(std::mem::replace(
            &mut *here,
            SlotState::Blackhole,
        ))];
        drop(here);
        while let Some(j) = work.pop() {
            match j {
                Junk::State(SlotState::Value(v)) => dismantle_value(v, &mut work),
                Junk::State(SlotState::Thunk { env, .. }) => work.push(Junk::Env(env)),
            Junk::State(SlotState::PendingApply { f, args }) => {
                for s in &args {
                    drain_slot(s, &mut work);
                }
                dismantle_value(f, &mut work);
            }
                Junk::State(_) => {}
                Junk::Env(e) => dismantle_env(e, &mut work),
            }
        }
    }
}

enum Junk {
    State(SlotState),
    Env(Env),
}

/// Empty a slot we are the last owner of, queueing whatever it held. The
/// slot itself is left holding a leaf, so its own `Drop` is then trivial.
fn drain_slot(s: &Slot, work: &mut Vec<Junk>) {
    if Rc::strong_count(&s.0) != 1 {
        return;
    }
    if let Ok(mut st) = s.0.try_borrow_mut() {
        work.push(Junk::State(std::mem::replace(&mut *st, SlotState::Blackhole)));
    }
}

fn dismantle_value(v: Value, work: &mut Vec<Junk>) {
    match v {
        Value::List(rc) => {
            if let Ok(items) = Rc::try_unwrap(rc) {
                for s in &items {
                    drain_slot(s, work);
                }
            }
        }
        Value::Attrs(rc) => {
            if let Ok(map) = Rc::try_unwrap(rc) {
                for s in map.values() {
                    drain_slot(s, work);
                }
            }
        }
        Value::Closure(rc) => {
            if let Ok(c) = Rc::try_unwrap(rc) {
                work.push(Junk::Env(c.env));
            }
        }
        Value::Builtin(rc) => {
            if let Ok(b) = Rc::try_unwrap(rc) {
                for s in &b.args {
                    drain_slot(s, work);
                }
            }
        }
        Value::Int(_)
        | Value::Float(_)
        | Value::Bool(_)
        | Value::Null
        | Value::Str(_)
        | Value::Path(_) => {}
    }
}

fn dismantle_env(e: Env, work: &mut Vec<Junk>) {
    let mut cur = e;
    loop {
        match Rc::try_unwrap(cur) {
            Ok(EnvNode::Frame { up, slots }) => {
                for s in slots.borrow().iter() {
                    drain_slot(s, work);
                }
                cur = up;
            }
            Ok(EnvNode::With { up, subject }) => {
                drain_slot(&subject, work);
                cur = up;
            }
            Ok(EnvNode::Root) | Err(_) => return,
        }
    }
}

/// Environment: a chain of frames, innermost last. Frames are shared, not
/// copied, when captured by thunks and closures.
pub type Env = Rc<EnvNode>;

#[derive(Debug)]
pub enum EnvNode {
    Root,
    Frame {
        up: Env,
        slots: RefCell<Vec<Slot>>,
    },
    /// A `with` scope. Kept in the same chain so PopEnv/PopWith stay
    /// balanced under laziness (thunks capture whatever chain existed).
    With {
        up: Env,
        /// The with subject, lazily forced on first dynamic resolve.
        subject: Slot,
    },
}

pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Bool(_) => "a Boolean",
        Value::Null => "null",
        Value::Str(_) => "a string",
        Value::Path(_) => "a path",
        Value::List(_) => "a list",
        Value::Attrs(_) => "a set",
        Value::Closure(_) | Value::Builtin(_) => "a function",
    }
}

/// Lexical path normalization ("." and ".." segments); never touches the
/// filesystem, same as cppnix's path handling. Applied both to path literals
/// at compile time and to the result of `path + string`, which is why
/// `/foo/bar + "/../xyzzy/." + "/foo.txt"` is `/foo/xyzzy/foo.txt`.
pub fn normalize_path(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// printf %.6g, the cppnix float rendering (drv hashes depend on it).
pub fn format_g6(x: f64) -> String {
    if x == 0.0 {
        return "0".to_owned();
    }
    let exp = x.abs().log10().floor() as i32;
    if (-5..6).contains(&exp) {
        let prec = usize::try_from(5i64 - i64::from(exp)).unwrap_or(0);
        let mut out = format!("{x:.prec$}");
        if out.contains('.') {
            while out.ends_with('0') {
                out.pop();
            }
            if out.ends_with('.') {
                out.pop();
            }
        }
        out
    } else {
        let s = format!("{x:.5e}");
        let Some((mantissa, e)) = s.split_once('e') else {
            return s;
        };
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        let e: i32 = e.parse().unwrap_or(0);
        format!("{mantissa}e{e:+03}")
    }
}

impl fmt::Display for Value {
    /// Debug-ish display for errors; the real corpus printer lives in
    /// `print` (it needs the interner for attr names and forces lazily).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(x) => write!(f, "{}", format_g6(*x)),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Null => write!(f, "null"),
            Value::Str(s) => write!(f, "\"{s}\""),
            Value::Path(p) => write!(f, "{p}"),
            Value::List(_) => write!(f, "[ ... ]"),
            Value::Attrs(_) => write!(f, "{{ ... }}"),
            Value::Closure(_) | Value::Builtin(_) => write!(f, "<LAMBDA>"),
        }
    }
}
