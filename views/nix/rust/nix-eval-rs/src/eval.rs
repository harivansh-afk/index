//! Top-level entry: source text -> compiled module -> VM -> printed value.
//! The M1 CST-walking evaluator is gone; the compiler + VM are the one
//! implementation (one concept, one implementation).

use crate::compile::{self, CompileError};
use crate::host::{Host, RealFs};
use crate::task::NeedPath;
use crate::value2::{Slot, Value};
use crate::vm::{ErrKind, Step, Vm, VmError};
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Debug)]
pub enum EvalError {
    /// A construct the rust evaluator does not implement yet; the payload
    /// names it. Counted as `unimplemented` by the harnesses, never
    /// `mismatch`.
    Unimplemented(String),
    /// Real evaluation failure with cppnix-equivalent behavior, tagged with
    /// the cppnix exception class the bridge should raise.
    Eval(ErrKind, String),
    Parse(String),
}

pub fn eval_str(src: &str) -> Result<String, EvalError> {
    eval_str_at(src, ".")
}

pub fn eval_str_at(src: &str, base_dir: &str) -> Result<String, EvalError> {
    let module = compile::compile_source(src, base_dir).map_err(|e| match e {
        CompileError::Unimplemented(w) => EvalError::Unimplemented(w),
        CompileError::UndefinedVariable(n) => {
            EvalError::Eval(ErrKind::Eval, format!("undefined variable '{n}'"))
        }
        CompileError::Parse(m) => EvalError::Parse(m),
    })?;
    let module = Rc::new(module);
    let mut vm = Vm::new();
    let host = RealFs;
    vm.start_module(&module);
    let value = drive(&mut vm, &host).map_err(map_vm_err)?;
    vm.start_print(value);
    match drive(&mut vm, &host).map_err(map_vm_err)? {
        Value::Str(s) => Ok(s.to_string()),
        _ => Err(EvalError::Eval(
            ErrKind::Eval,
            "internal: printer produced a non-string".into(),
        )),
    }
}

/// The scheduler side of the poll loop: the only place in the crate that
/// touches a filesystem. The VM asks, this answers, and the frame chain is
/// untouched across the gap -- which is the property that lets the effects
/// kernel later record what was read, or replay it, without the evaluator
/// knowing.
pub fn drive(vm: &mut Vm, host: &dyn Host) -> Result<Value, VmError> {
    loop {
        match vm.poll()? {
            Step::Done(v) => return Ok(v),
            Step::Perform { domain, .. } => {
                return Err(VmError::Unimplemented(format!("effect domain '{domain}'")));
            }
            Step::NeedPath { need, resume } => {
                let answer = answer_path(vm, host, &need)?;
                vm.resume(resume, answer)?;
            }
        }
    }
}

fn answer_path(vm: &mut Vm, host: &dyn Host, need: &NeedPath) -> Result<Value, VmError> {
    match need {
        NeedPath::Import(p) => {
            let resolved = host.resolve_import(p).map_err(VmError::eval)?;
            let text = host.read_file(&resolved).map_err(VmError::eval)?;
            // Both halves in one answer: the VM needs the resolved path to
            // give the imported file its own base directory for relative
            // paths, and asking twice would let the two disagree.
            let mut m = BTreeMap::new();
            let k = vm.intern("path");
            m.insert(k, Slot::value(Value::Str(resolved.into())));
            let k = vm.intern("text");
            m.insert(k, Slot::value(Value::Str(text.into())));
            Ok(Value::Attrs(Rc::new(m)))
        }
        NeedPath::Contents(p) => Ok(Value::Str(host.read_file(p).map_err(VmError::eval)?.into())),
        NeedPath::Exists(p) => Ok(Value::Bool(host.path_exists(p))),
        NeedPath::Kind(p) => Ok(Value::Str(
            host.file_type(p).map_err(VmError::eval)?.as_str().into(),
        )),
        NeedPath::Entries(p) => {
            let entries = host.read_dir(p).map_err(VmError::eval)?;
            let mut m = BTreeMap::new();
            for (name, t) in entries {
                let k = vm.intern(&name);
                m.insert(k, Slot::value(Value::Str(t.as_str().into())));
            }
            Ok(Value::Attrs(Rc::new(m)))
        }
    }
}

fn map_vm_err(e: VmError) -> EvalError {
    match e {
        VmError::Unimplemented(w) => EvalError::Unimplemented(w),
        VmError::Throw(c) => EvalError::Eval(c.kind, c.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Value renders as-is, errors as their debug form; assertions never
    /// panic by hand (the workspace denies `panic`, tests included).
    fn render(src: &str) -> String {
        match eval_str(src) {
            Ok(v) => v,
            Err(e) => format!("{e:?}"),
        }
    }

    #[test]
    fn arithmetic() {
        assert_eq!(render("1 + 2"), "3");
        assert_eq!(render("2 * 3 + 4"), "10");
        assert_eq!(render("(1 + 2) * -3"), "-9");
        assert_eq!(render("7 / 2"), "3");
        assert_eq!(render("7.0 / 2"), "3.5");
        assert_eq!(render("1.0 + 2"), "3");
    }

    #[test]
    fn bindings_and_functions() {
        assert_eq!(render("let a = 1; b = a + 1; in a + b"), "3");
        assert_eq!(render("let f = x: x * 2; in f 21"), "42");
        assert_eq!(render("(x: y: x + y) 1 2"), "3");
        assert_eq!(render("({ a, b ? 10 }: a + b) { a = 1; }"), "11");
        assert_eq!(render("({ a, ... } @ all: a + all.b) { a = 1; b = 2; }"), "3");
        assert_eq!(render("let f = n: if n == 0 then 1 else n * f (n - 1); in f 5"), "120");
    }

    #[test]
    fn data_structures() {
        assert_eq!(render("[ 1 2 3 ]"), "[ 1 2 3 ]");
        assert_eq!(render("{ b = 2; a = 1; }"), "{ a = 1; b = 2; }");
        assert_eq!(render("{ a = 1; b = 2; }.b"), "2");
        assert_eq!(render("{ a = { b = 42; }; }.a.b"), "42");
        assert_eq!(render("{ a = 1; }.b or 7"), "7");
        assert_eq!(render("{ a = 1; } ? a"), "true");
        assert_eq!(render("rec { a = 1; b = a + 1; }.b"), "2");
        assert_eq!(render("[ 1 ] ++ [ 2 ]"), "[ 1 2 ]");
        assert_eq!(render("{ a = 1; } // { a = 2; b = 3; }"), "{ a = 2; b = 3; }"
        );
    }

    #[test]
    fn scoping() {
        assert_eq!(render("with { a = 1; }; a"), "1");
        assert_eq!(render("let a = 2; in with { a = 1; }; a"), "2");
        assert_eq!(render("with { a = 1; }; with { a = 2; }; a"), "2");
    }

    #[test]
    fn strings() {
        assert_eq!(render("\"abc\""), "\"abc\"");
        assert_eq!(render("let x = \"b\"; in \"a${x}c\""), "\"abc\"");
        assert_eq!(render("\"a\" + \"b\""), "\"ab\"");
        assert_eq!(render("toString 42"), "\"42\"");
        assert_eq!(render("toString [ 1 2 ]"), "\"1 2\"");
    }

    #[test]
    fn logic_and_comparison() {
        assert_eq!(render("true && false"), "false");
        assert_eq!(render("false || true"), "true");
        assert_eq!(render("false -> false"), "true");
        assert_eq!(render("1 < 2"), "true");
        assert_eq!(render("\"a\" < \"b\""), "true");
        assert_eq!(render("[ 1 2 ] == [ 1 2 ]"), "true");
        assert_eq!(render("{ a = 1; } == { a = 1; }"), "true");
        assert_eq!(render("assert 1 == 1; 42"), "42");
    }

    #[test]
    fn laziness() {
        assert_eq!(render("let boom = throw \"x\"; in 1"), "1");
        assert_eq!(render("[ (throw \"x\") ] == [ ]"), "false");
        assert_eq!(render("{ a = throw \"x\"; } ? a"), "true");
    }

    #[test]
    fn global_spelling_mirrors_cpp_registration() {
        // Registered "__length": bare `length` is undefined, __length and
        // builtins.length work. Registered "map": bare map works.
        assert_eq!(render("__length [ 1 2 ]"), "2");
        assert_eq!(render("builtins.length [ 1 2 ]"), "2");
        assert_eq!(render("length [ 1 2 ]"), "Eval(Eval, \"undefined variable 'length'\")");
        assert_eq!(render("map (x: x + 1) [ 1 2 ]"), "[ 2 3 ]");
    }

    #[test]
    fn builtins_batch2() {
        assert_eq!(render("builtins.attrNames { b = 1; a = 2; }"), "[ \"a\" \"b\" ]");
        assert_eq!(render("builtins.foldl' (a: b: a + b) 0 [ 1 2 3 ]"), "6");
        assert_eq!(render("builtins.tryEval (throw \"x\")"), "{ success = false; value = false; }");
        assert_eq!(render("builtins.tryEval 42"), "{ success = true; value = 42; }");
        assert_eq!(render("builtins.substring 1 2 \"abcd\""), "\"bc\"");
        assert_eq!(render("builtins.replaceStrings [ \"o\" ] [ \"0\" ] \"foobar\""), "\"f00bar\"");
        assert_eq!(render("builtins.compareVersions \"1.0\" \"1.0.1\""), "-1");
        assert_eq!(render("builtins.compareVersions \"2.3pre1\" \"2.3\""), "-1");
        assert_eq!(render("builtins.sort builtins.lessThan [ 3 1 2 ]"), "[ 1 2 3 ]");
        assert_eq!(render("builtins.typeOf 1.5"), "\"float\"");
        assert_eq!(render("builtins.functionArgs ({ a, b ? 1 }: a)"), "{ a = false; b = true; }");
    }

    /// 100k levels of nesting, which is far past what any host stack holds.
    /// Building, deep-forcing, printing and comparing each walk every level,
    /// and each recursed once per level in the old interpreter. Expected
    /// results cross-checked against the cppnix arm at small n. Runs on a
    /// default 2 MiB test thread.
    #[test]
    fn deep_values_never_reach_the_host_stack() {
        const N: usize = 100_000;
        let build = "let f = n: if n == 0 then [ ] else [ (f (n - 1)) ]; in ";
        assert_eq!(
            render(&format!("{build} builtins.deepSeq (f {N}) \"ok\"")),
            "\"ok\""
        );
        // "[ ]" at the bottom, four more characters per level above it.
        let printed = render(&format!("{build} f {N}"));
        assert_eq!(printed.len(), 3 + 4 * N);
        assert!(printed.starts_with("[ [ [ "));
        assert_eq!(render(&format!("{build} (f {N}) == (f {N})")), "true");
        assert_eq!(render(&format!("{build} (f {N}) == (f {})", N - 1)), "false");
        assert_eq!(render(&format!("{build} (f {N}) < (f {})", N - 1)), "false");
    }

    /// The other unbounded shape: a call chain rather than a value nest. The
    /// fold is 100k applications in one builtin, and `f` is 100k pending
    /// additions each waiting on the next call.
    #[test]
    fn long_call_chains_never_reach_the_host_stack() {
        assert_eq!(
            render("builtins.foldl' (a: b: a + b) 0 (builtins.genList (i: i) 100000)"),
            "4999950000"
        );
        assert_eq!(
            render("let f = n: if n == 0 then 0 else 1 + f (n - 1); in f 100000"),
            "100000"
        );
    }

    /// cppnix's forceValueDeep carries a seen-set so a cyclic attrset bottoms
    /// out; without one this is an infinite descent, which is what made
    /// eval-okay-deepseq the corpus's only crash.
    #[test]
    fn deep_seq_terminates_on_a_self_referential_attrset() {
        assert_eq!(
            render("builtins.deepSeq (let as = { x = 123; y = as; }; in as) 456"),
            "456"
        );
    }

    /// deepSeq finishes the first argument before it looks at the second, so
    /// a throw buried in the deep walk beats a throw sitting in the result.
    #[test]
    fn deep_seq_reports_the_deep_failure_first() {
        assert_eq!(
            render("builtins.deepSeq [ (throw \"deep\") ] (throw \"result\")"),
            "Eval(Thrown, \"deep\")"
        );
    }

    /// The `to` side of replaceStrings stays lazy: cppnix only forces the
    /// replacements it actually uses.
    #[test]
    fn replace_strings_leaves_unused_replacements_unforced() {
        assert_eq!(
            render("builtins.replaceStrings [ \"oo\" \"XX\" ] [ \"u\" (throw \"unreachable\") ] \"foobar\""),
            "\"fubar\""
        );
    }

    /// cppnix's eqValues bookends: the same cell equals itself whatever it
    /// holds, and functions equal nothing. Only both together give `f == f`
    /// false and `[ f ] == [ f ]` true, and the second needs the compiler to
    /// pass a bare variable as its own slot rather than a fresh thunk.
    #[test]
    fn function_equality_follows_cell_identity() {
        assert_eq!(render("let f = x: x; in f == f"), "false");
        assert_eq!(render("let f = x: x; in [ f ] == [ f ]"), "true");
        assert_eq!(render("let f = x: x; in { a = f; } == { a = f; }"), "true");
        assert_eq!(render("(x: x) == (x: x)"), "false");
        // Distinct cells holding equal data are still equal.
        assert_eq!(render("let a = [ 1 ]; b = [ 1 ]; in a == b"), "true");
    }

    /// cppnix builds map/genList/mapAttrs results with mkApp, so the function
    /// runs only when an element is forced. eval-okay-intersectAttrs is the
    /// corpus case: it maps `throw` over a set and never looks at the values.
    #[test]
    fn mapped_functions_do_not_run_until_forced() {
        assert_eq!(render("builtins.attrNames (builtins.mapAttrs throw { a = 1; })"), "[ \"a\" ]");
        assert_eq!(render("builtins.length (builtins.map throw [ 1 2 ])"), "2");
        assert_eq!(render("builtins.length (builtins.genList throw 3)"), "3");
        assert_eq!(render("builtins.mapAttrs (n: v: n + v) { a = \"1\"; }"), "{ a = \"a1\"; }");
    }

    /// cppnix's parser merges attrpaths that share a prefix, and merges two
    /// bindings of one name when both values are set literals. The `rec` of
    /// the FIRST set covers whatever is merged in later (NixOS/nix#9020).
    #[test]
    fn attrpaths_and_set_literals_merge() {
        assert_eq!(render("{ a.b = 1; a.c = 2; }"), "{ a = { b = 1; c = 2; }; }");
        assert_eq!(render("{ a = { b = 1; }; a = { c = 2; }; }"), "{ a = { b = 1; c = 2; }; }");
        assert_eq!(render("{ a.b.c = 1; }.a.b.c"), "1");
        assert_eq!(render("(let a.b = 1; in a).b"), "1");
        assert_eq!(
            render("{ a = rec { b = c + 1; d = 2; }; a.c = d + 3; }.a.b"),
            "6"
        );
        // Two non-set values under one name is still a redefinition.
        assert_eq!(
            render("{ a = 1; a = 2; }"),
            "Parse(\"attribute 'a' already defined\")"
        );
    }

    /// `let { body = …; }`: cppnix's pre-`let ... in` syntax, defined as the
    /// `body` attribute of the equivalent rec set.
    #[test]
    fn legacy_let_is_the_body_of_a_rec_set() {
        assert_eq!(render("let { body = a; a = 1; }"), "1");
        assert_eq!(render("let { body = x + y; x = 1; y = x + 1; }"), "3");
    }

    /// `inherit x` in a rec scope takes the OUTER x; joining the frame it is
    /// being added to would make it recurse on itself.
    #[test]
    fn rec_inherit_resolves_outside_its_own_frame() {
        assert_eq!(render("let x = 1; in (rec { inherit x; y = x + 1; }).y"), "2");
        assert_eq!(render("let x = 1; in { inherit x; }.x"), "1");
    }

    /// The scheduler answers path questions; the VM never reads a file. A
    /// host that resolves everything from memory proves the seam holds, and
    /// is what an effects-kernel host will replace.
    #[test]
    fn the_vm_reads_files_only_through_the_host() {
        use crate::host::{FileType, Host};

        struct Fake;
        impl Host for Fake {
            fn read_file(&self, path: &str) -> std::result::Result<String, String> {
                match path {
                    "/m/lib.nix" => Ok("{ id = x: x; n = 7; }".to_owned()),
                    "/m/dir/default.nix" => Ok("import /m/lib.nix".to_owned()),
                    _ => Err(format!("path '{path}' does not exist")),
                }
            }
            fn read_dir(&self, _p: &str) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(vec![("a".to_owned(), FileType::Regular)])
            }
            fn path_exists(&self, path: &str) -> bool {
                self.read_file(path).is_ok()
            }
            fn file_type(&self, path: &str) -> std::result::Result<FileType, String> {
                match path {
                    "/m/dir" => Ok(FileType::Directory),
                    p if self.path_exists(p) => Ok(FileType::Regular),
                    p => Err(format!("path '{p}' does not exist")),
                }
            }
        }

        fn run(src: &str) -> String {
            let Ok(module) = compile::compile_source(src, "/m") else {
                return "compile failed".to_owned();
            };
            let module = Rc::new(module);
            let mut vm = Vm::new();
            vm.start_module(&module);
            let v = match drive(&mut vm, &Fake) {
                Ok(v) => v,
                Err(e) => return format!("{e:?}"),
            };
            vm.start_print(v);
            match drive(&mut vm, &Fake) {
                Ok(Value::Str(s)) => s.to_string(),
                other => format!("{other:?}"),
            }
        }

        assert_eq!(run("(import /m/lib.nix).n"), "7");
        assert_eq!(run("(import /m/lib.nix).id 1"), "1");
        // A directory imports its default.nix, and the imported file's own
        // relative paths resolve against the RESOLVED file's directory.
        assert_eq!(run("(import /m/dir).n"), "7");
        assert_eq!(run("builtins.pathExists /m/lib.nix"), "true");
        assert_eq!(run("builtins.pathExists /m/nope.nix"), "false");
        assert_eq!(run("builtins.readFileType /m/dir"), "\"directory\"");
        assert_eq!(run("builtins.attrNames (builtins.readDir /m)"), "[ \"a\" ]");
        // One compile per file however many times it is imported.
        assert_eq!(
            run("let a = import /m/lib.nix; b = import /m/lib.nix; in a.n + b.n"),
            "14"
        );
    }

    /// foldl' is strict in the accumulator it produces, not the one it is
    /// handed: the machine's argument forcing is per-argument for exactly
    /// this, since `foldl'` forces its function and its list but not its nul.
    #[test]
    fn foldl_strict_leaves_the_initial_accumulator_alone() {
        assert_eq!(
            render("builtins.foldl' (_: x: x) (throw \"never\") [ 1 42 ]"),
            "42"
        );
        // Nothing consumed it, so an empty list hands the thunk straight back
        // and the throw surfaces where the value is finally wanted.
        assert_eq!(
            render("builtins.foldl' (_: x: x) (throw \"never\") [ ]"),
            "Eval(Thrown, \"never\")"
        );
        assert_eq!(render("builtins.foldl' (a: b: a + b) 0 [ 1 2 3 ]"), "6");
    }

    /// `path + string` stays canonical, `string + path` does not. cppnix
    /// normalizes only the path-valued side, and eval-okay-string pins both.
    #[test]
    fn path_concatenation_normalises_only_on_the_path_side() {
        assert_eq!(render("toString (/foo/bar + \"/../xyzzy/.\" + \"/a.txt\")"), "\"/foo/xyzzy/a.txt\"");
        assert_eq!(render("\"/../foo\" + toString /x/y"), "\"/../foo/x/y\"");
        assert_eq!(render("toString (/a/b + \"/c\")"), "\"/a/b/c\"");
    }

    #[test]
    fn infinite_recursion_detected() {
        assert_eq!(
            render("let a = a; in a"),
            "Eval(Eval, \"infinite recursion encountered\")"
        );
    }
}
