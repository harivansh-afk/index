//! Builtin functions. The table index is the IR-level contract: a compiled
//! module referencing builtin N means entry N of this table, so entries are
//! append-only once a module format ships.
//!
//! A builtin is either `Pure` -- every argument arrives already forced and the
//! result needs no further evaluation -- or `Start`, which returns a
//! continuation the machine drives. No builtin re-enters the interpreter, so
//! none of them can put Nix-value-proportional depth on the host stack.

use crate::builtins2::{self as b2, Begin, argv};
use crate::builtins3 as b3;
use crate::print::coerce_scalar;
use crate::task::Task;
use crate::value2::{Slot, Value, type_name};
use crate::vm::{Vm, VmError};
use std::collections::BTreeMap;
use std::rc::Rc;

type Result<T> = std::result::Result<T, VmError>;

pub enum Kind {
    /// Arguments pre-forced, result immediate.
    Pure(fn(&mut Vm, &[Slot]) -> Result<Value>),
    /// Needs more evaluation; says how to continue.
    Start(fn(&mut Vm, &[Slot]) -> Result<Begin>),
}

pub struct Builtin {
    pub name: &'static str,
    pub arity: usize,
    pub kind: Kind,
    /// Argument indices the machine must NOT force before the body runs.
    /// Per-argument rather than all-or-nothing because cppnix is: `foldl'`
    /// forces its function and its list but not its initial accumulator,
    /// which the corpus checks with a `throw` there.
    pub lazy: &'static [usize],
}

macro_rules! pure_bi {
    ($name:literal, $arity:literal, $f:path) => {
        Builtin {
            name: $name,
            arity: $arity,
            kind: Kind::Pure($f),
            lazy: &[],
        }
    };
}

macro_rules! start_bi {
    ($name:literal, $arity:literal, $f:path) => {
        Builtin {
            name: $name,
            arity: $arity,
            kind: Kind::Start($f),
            lazy: &[],
        }
    };
}

/// A builtin naming the arguments the machine leaves unforced.
macro_rules! lazy_bi {
    ($name:literal, $arity:literal, $f:path, $lazy:expr) => {
        Builtin {
            name: $name,
            arity: $arity,
            kind: Kind::Start($f),
            lazy: $lazy,
        }
    };
}

/// In global scope (usable bare, like `map` or `throw`); everything is also
/// reachable as `builtins.<name>`.
pub static TABLE: &[Builtin] = &[
    pure_bi!("throw", 1, bi_throw),
    pure_bi!("abort", 1, bi_abort),
    start_bi!("toString", 1, bi_to_string),
    start_bi!("map", 2, b2::bi_map),
    pure_bi!("isNull", 1, bi_is_null),
    pure_bi!("baseNameOf", 1, bi_base_name_of),
    pure_bi!("dirOf", 1, bi_dir_of),
    pure_bi!("length", 1, b2::bi_length),
    start_bi!("head", 1, b2::bi_head),
    pure_bi!("tail", 1, b2::bi_tail),
    start_bi!("elemAt", 2, b2::bi_elem_at),
    start_bi!("elem", 2, b2::bi_elem),
    start_bi!("filter", 2, b2::bi_filter),
    start_bi!("any", 2, b2::bi_any),
    start_bi!("all", 2, b2::bi_all),
    start_bi!("concatLists", 1, b2::bi_concat_lists),
    start_bi!("concatMap", 2, b2::bi_concat_map),
    start_bi!("genList", 2, b2::bi_gen_list),
    lazy_bi!("foldl'", 3, b2::bi_foldl_strict, &[1]),
    start_bi!("sort", 2, b2::bi_sort),
    pure_bi!("attrNames", 1, b2::bi_attr_names),
    pure_bi!("attrValues", 1, b2::bi_attr_values),
    start_bi!("getAttr", 2, b2::bi_get_attr),
    pure_bi!("hasAttr", 2, b2::bi_has_attr),
    start_bi!("removeAttrs", 2, b2::bi_remove_attrs),
    pure_bi!("intersectAttrs", 2, b2::bi_intersect_attrs),
    start_bi!("catAttrs", 2, b2::bi_cat_attrs),
    start_bi!("listToAttrs", 1, b2::bi_list_to_attrs),
    start_bi!("mapAttrs", 2, b2::bi_map_attrs),
    start_bi!("groupBy", 2, b2::bi_group_by),
    start_bi!("partition", 2, b2::bi_partition),
    pure_bi!("stringLength", 1, b2::bi_string_length),
    pure_bi!("substring", 3, b2::bi_substring),
    start_bi!("concatStringsSep", 2, b2::bi_concat_strings_sep),
    start_bi!("replaceStrings", 3, b2::bi_replace_strings),
    pure_bi!("splitVersion", 1, b2::bi_split_version),
    pure_bi!("seq", 2, b2::bi_seq),
    lazy_bi!("deepSeq", 2, b2::bi_deep_seq, &[0, 1]),
    lazy_bi!("tryEval", 1, b2::bi_try_eval, &[0]),
    pure_bi!("functionArgs", 1, b2::bi_function_args),
    pure_bi!("typeOf", 1, b2::bi_type_of),
    pure_bi!("isInt", 1, b2::bi_is_int),
    pure_bi!("isFloat", 1, b2::bi_is_float),
    pure_bi!("isBool", 1, b2::bi_is_bool),
    pure_bi!("isString", 1, b2::bi_is_string),
    pure_bi!("isPath", 1, b2::bi_is_path),
    pure_bi!("isList", 1, b2::bi_is_list),
    pure_bi!("isAttrs", 1, b2::bi_is_attrs),
    pure_bi!("isFunction", 1, b2::bi_is_function),
    pure_bi!("add", 2, b2::bi_add),
    pure_bi!("sub", 2, b2::bi_sub),
    pure_bi!("mul", 2, b2::bi_mul),
    pure_bi!("div", 2, b2::bi_div),
    start_bi!("lessThan", 2, b2::bi_less_than),
    pure_bi!("bitAnd", 2, b2::bi_bit_and),
    pure_bi!("bitOr", 2, b2::bi_bit_or),
    pure_bi!("bitXor", 2, b2::bi_bit_xor),
    pure_bi!("floor", 1, b2::bi_floor),
    pure_bi!("ceil", 1, b2::bi_ceil),
    pure_bi!("compareVersions", 2, b2::bi_compare_versions),
    start_bi!("import", 1, b2::bi_import),
    start_bi!("readFile", 1, b2::bi_read_file),
    start_bi!("pathExists", 1, b2::bi_path_exists),
    start_bi!("readDir", 1, b2::bi_read_dir),
    start_bi!("readFileType", 1, b2::bi_read_file_type),
    start_bi!("genericClosure", 1, b3::bi_generic_closure),
    pure_bi!("getEnv", 1, b3::bi_get_env),
    start_bi!("zipAttrsWith", 2, b3::bi_zip_attrs_with),
    pure_bi!("hashString", 2, b3::bi_hash_string),
    pure_bi!("match", 2, b3::bi_match),
    pure_bi!("split", 2, b3::bi_split),
    pure_bi!("fromJSON", 1, b3::bi_from_json),
    pure_bi!("fromTOML", 1, b3::bi_from_toml),
    lazy_bi!("toJSON", 1, b3::bi_to_json, &[0]),
];

pub fn global_index(name: &str) -> Option<u16> {
    TABLE.iter().position(|b| b.name == name).map(|i| i as u16)
}

/// cppnix registers every primop under its registered spelling as a global
/// (plus a few non-primop globals); names we have no implementation for
/// compile to a slot that reports unimplemented on use, so coverage gaps
/// count as `unimplemented`, never as `undefined variable` mismatches.
pub fn is_cpp_global(name: &str) -> bool {
    crate::builtins_gen::CPP_PRIMOP_NAMES.contains(&name)
        || crate::builtins_gen::CPP_EXTRA_GLOBALS.contains(&name)
}

pub fn mk_value(idx: u16) -> Value {
    Value::Builtin(Rc::new(crate::value2::BuiltinData {
        idx,
        args: Vec::new(),
    }))
}

fn arg(args: &[Slot], i: usize) -> Result<&Slot> {
    args.get(i)
        .ok_or_else(|| VmError::eval("internal: missing builtin argument"))
}

fn bi_throw(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Err(VmError::thrown(want_message(&argv(args, 0)?)?))
}

fn bi_abort(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let msg = want_message(&argv(args, 0)?)?;
    // abort is deliberately not catchable: tryEval must not swallow it.
    Err(VmError::eval(format!(
        "evaluation aborted with the following error message: '{msg}'"
    )))
}

fn want_message(v: &Value) -> Result<String> {
    match v {
        Value::Str(s) => Ok(s.to_string()),
        other => Err(VmError::eval(format!(
            "expected a string but found {}",
            type_name(other)
        ))),
    }
}

/// Lists coerce element-wise, which is a walk of unbounded depth, so it goes
/// through the machine; everything else decides here.
fn bi_to_string(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let v = argv(args, 0)?;
    match v {
        Value::List(_) => Ok(Begin::Sub(Task::coerce(arg(args, 0)?.clone()))),
        other => Ok(Begin::Done(Value::Str(coerce_scalar(&other)?.into()))),
    }
}

fn bi_is_null(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Bool(matches!(argv(args, 0)?, Value::Null)))
}

fn bi_base_name_of(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let v = argv(args, 0)?;
    let s = match &v {
        Value::Str(s) => s.to_string(),
        Value::Path(p) => p.to_string(),
        other => {
            return Err(VmError::eval(format!(
                "expected a string but found {}",
                type_name(other)
            )));
        }
    };
    Ok(Value::Str(legacy_base_name_of(&s).into()))
}

/// cppnix's baseNameOf is `legacyBaseNameOf`, not the utility of the same
/// name: it strips at most ONE trailing slash, so `baseNameOf "a//"` is ""
/// rather than "a". Upstream calls the behavior regrettable and keeps it.
fn legacy_base_name_of(path: &str) -> &str {
    if path.is_empty() {
        return "";
    }
    let bytes = path.as_bytes();
    let mut last = path.len() - 1;
    if bytes.get(last) == Some(&b'/') && last > 0 {
        last -= 1;
    }
    let pos = match path.get(..=last).and_then(|head| head.rfind('/')) {
        Some(i) => i + 1,
        None => 0,
    };
    if pos > last {
        return "";
    }
    path.get(pos..=last).unwrap_or("")
}

fn bi_dir_of(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let v = argv(args, 0)?;
    match &v {
        Value::Path(p) => {
            let d = match p.rfind('/') {
                Some(0) => "/".to_owned(),
                Some(i) => p.get(..i).unwrap_or("/").to_owned(),
                None => ".".to_owned(),
            };
            Ok(Value::Path(d.into()))
        }
        Value::Str(s) => {
            let d = match s.rfind('/') {
                Some(0) => "/".to_owned(),
                Some(i) => s.get(..i).unwrap_or(".").to_owned(),
                None => ".".to_owned(),
            };
            Ok(Value::Str(d.into()))
        }
        other => Err(VmError::eval(format!(
            "expected a string but found {}",
            type_name(other)
        ))),
    }
}

/// The `builtins` attrset: every cppnix builtin name is present, bound to
/// the real implementation where one exists and to an unimplemented-on-use
/// slot otherwise. Absent names would surface as `attribute missing`, which
/// the differ counts as a semantic mismatch; present-but-unimplemented is
/// the honest state.
pub fn builtins_set(vm: &mut Vm) -> Value {
    let mut map = BTreeMap::new();
    for name in crate::builtins_gen::CPP_PRIMOP_NAMES {
        let bare = name.strip_prefix("__").unwrap_or(name);
        let sym = vm.intern(bare);
        let slot = match global_index(bare) {
            Some(i) => Slot::value(mk_value(i)),
            None => Slot::unimplemented(&format!("builtins.{bare}")),
        };
        map.insert(sym, slot);
    }
    for name in crate::builtins_gen::CPP_BUILTINS_EXTRA {
        let sym = vm.intern(name);
        let slot = match global_index(name) {
            Some(i) => Slot::value(mk_value(i)),
            None => Slot::unimplemented(&format!("builtins.{name}")),
        };
        map.insert(sym, slot);
    }
    let t = vm.intern("true");
    map.insert(t, Slot::value(Value::Bool(true)));
    let f = vm.intern("false");
    map.insert(f, Slot::value(Value::Bool(false)));
    let n = vm.intern("null");
    map.insert(n, Slot::value(Value::Null));
    let b = vm.intern("builtins");
    map.insert(b, Slot::value(builtins_set_marker()));
    Value::Attrs(Rc::new(map))
}

/// builtins.builtins is self-referential in cppnix; a second level is
/// enough for the corpus and avoids a cyclic Rc.
fn builtins_set_marker() -> Value {
    Value::Attrs(Rc::new(BTreeMap::new()))
}
