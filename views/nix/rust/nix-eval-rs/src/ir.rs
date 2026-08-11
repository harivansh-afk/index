//! The bytecode IR: serializable, index-based, no pointers. A `Module` is
//! the unit of content addressing (one per source file); a `CodeUnit` is one
//! lambda body (or the top-level expression). Frontends other than Nix are
//! expected to emit this shape, so nothing in here references the rnix CST.
//!
//! Encoding choices carried from the design review (ENG-12068): fixed-width
//! ops rather than varints (varint decode showed up in snix profiles), a
//! const pool of pure literals only (runtime values in the pool are what
//! make snix Chunks unserializable), and `with`-scope ops as a feature a
//! frontend can simply not use.

/// One instruction. Operands index the const pool, the symbol table, the
/// unit list, or the locals of the current frame, per op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Push const pool entry.
    Const(u32),
    /// Push the value of the local at (depth, slot), forcing it.
    GetLocal { depth: u16, slot: u16 },
    /// Push the local's slot without forcing (rec-attrset construction).
    GetLocalLazy { depth: u16, slot: u16 },
    /// Push builtin `idx` as a value (unapplied).
    Builtin { idx: u16 },
    /// Push the `builtins` attrset.
    BuiltinsSet,
    /// A cppnix global this evaluator has no implementation for: errors as
    /// unimplemented when executed (not at compile time, matching laziness).
    UnimplementedGlobal { sym: u32 },
    /// Push a thunk over unit `unit`, capturing the current environment.
    Thunk { unit: u32 },
    /// Push a closure over unit `unit` (a lambda), capturing the environment.
    Closure { unit: u32 },
    /// Call: pop argument, pop callee, push result.
    Apply,
    /// Pop `n` values into a fresh environment frame (for let/lambda bodies),
    /// lazily: values stay thunks until forced.
    PushEnv { n: u16 },
    PopEnv,
    /// Pop scrutinee; if false jump forward by `target` ops.
    JumpIfFalse { target: u32 },
    /// Unconditional forward jump.
    Jump { target: u32 },
    /// Arithmetic / comparison / logic on the top two stack values.
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Neq,
    Lt,
    Leq,
    Gt,
    Geq,
    Not,
    Negate,
    /// String/path concatenation of the top `n` values (interpolation).
    ConcatStrings { n: u16 },
    /// List of the top `n` values (in push order).
    MkList { n: u16 },
    /// List concatenation (++) of the top two lists.
    ConcatLists,
    /// Attrset from the top 2*n stack entries: n (name, value) pairs where
    /// names were pushed as strings (dynamic names compile to the same op).
    MkAttrs { n: u16, rec: bool },
    /// Attrset update (//).
    Update,
    /// Pop attrset, push value of attr `sym` (forcing the select), or throw.
    Select { sym: u32 },
    /// Select that pushes a miss marker instead of throwing when the base is
    /// not a set or lacks the attr. Feeds `or` defaults and `?` paths.
    SelectSoft { sym: u32 },
    SelectSoftDyn,
    /// Pop default thunk, then value-or-miss: pushes the default on miss,
    /// the value otherwise.
    OrDefault,
    /// Pop set-or-miss, push bool: has attribute (miss reads as false).
    HasAttr { sym: u32 },
    /// Dynamic select: pop name string, then set.
    SelectDyn,
    HasAttrDyn,
    /// Enter a `with` scope: pop the (lazy) subject onto the env chain.
    PushWith,
    /// Resolve an identifier that static scoping could not: search the
    /// with-stack at runtime.
    ResolveWith { sym: u32 },
    /// Call builtin by table index with the single argument on the stack.
    CallBuiltin { idx: u16 },
    /// Assert: pop condition; throw if false.
    Assert,
    Ret,
}

/// Pure literals only.
#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// String without context (context arises only at runtime).
    Str(String),
    /// Path literal, already made absolute by the compiler against the
    /// module's base directory.
    Path(String),
}

/// One compiled body: the top-level expression or one lambda.
#[derive(Debug, Clone, Default)]
pub struct CodeUnit {
    pub ops: Vec<Op>,
    /// Formal parameter shape for lambda units.
    pub param: Option<Param>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    /// `x: body`; the argument binds one local slot.
    Ident(u32),
    /// `{ a, b ? d, ... } @ bind: body`: field symbols with optional default
    /// units, ellipsis flag, optional @-binding symbol.
    Formals {
        fields: Vec<(u32, Option<u32>)>,
        ellipsis: bool,
        bind: Option<u32>,
    },
}

/// A compiled source file. Symbols are module-local; the VM re-interns at
/// load. Everything is indexed, so serialization is a straight walk.
#[derive(Debug, Clone, Default)]
pub struct Module {
    pub consts: Vec<Const>,
    pub symbols: Vec<String>,
    pub units: Vec<CodeUnit>,
    /// Index of the entry unit (the file's top-level expression).
    pub entry: u32,
}
