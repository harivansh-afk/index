//! rnix CST -> `ir::Module`. Scope resolution happens here: every identifier
//! compiles to a (depth, slot) local access, a builtin reference, a constant
//! (`true`/`false`/`null` are ordinary bindings in cppnix's builtin scope),
//! or, when a `with` is in scope, a runtime `ResolveWith`. An identifier
//! that resolves to none of those is a compile-time "undefined variable",
//! matching cppnix's bindVars behavior.
//!
//! `rec` attrsets, `let`, and `inherit` all desugar to one shape: a frame of
//! mutually-visible thunk slots. The VM never sees recursion as a special
//! case.

use crate::builtins;
use crate::ir::{CodeUnit, Const, Module, Op, Param};
use crate::value2::normalize_path;
use rnix::ast::{self, AstToken, Expr, HasEntry};
use rowan::ast::AstNode;

#[derive(Debug)]
pub enum CompileError {
    Unimplemented(String),
    UndefinedVariable(String),
    Parse(String),
}

type Result<T> = std::result::Result<T, CompileError>;

/// One static scope frame: binding names in slot order.
enum ScopeFrame {
    Bindings(Vec<String>),
    With,
}

pub struct Compiler {
    module: Module,
    scopes: Vec<ScopeFrame>,
    /// Base directory for relative path literals.
    base_dir: String,
}

pub fn compile_source(src: &str, base_dir: &str) -> Result<Module> {
    let parse = rnix::Root::parse(src);
    if let Some(err) = parse.errors().first() {
        return Err(CompileError::Parse(err.to_string()));
    }
    let root = parse.tree();
    let expr = root
        .expr()
        .ok_or_else(|| CompileError::Parse("empty expression".into()))?;
    let mut c = Compiler {
        module: Module::default(),
        scopes: Vec::new(),
        base_dir: base_dir.to_owned(),
    };
    let mut ops = Vec::new();
    c.compile(&expr, &mut ops)?;
    ops.push(Op::Ret);
    let entry = c.push_unit(CodeUnit { ops, param: None });
    c.module.entry = entry;
    Ok(c.module)
}

impl Compiler {
    fn push_unit(&mut self, u: CodeUnit) -> u32 {
        self.module.units.push(u);
        (self.module.units.len() - 1) as u32
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(i) = self.module.symbols.iter().position(|x| x == s) {
            return i as u32;
        }
        self.module.symbols.push(s.to_owned());
        (self.module.symbols.len() - 1) as u32
    }

    fn konst(&mut self, c: Const) -> u32 {
        if let Some(i) = self.module.consts.iter().position(|x| *x == c) {
            return i as u32;
        }
        self.module.consts.push(c);
        (self.module.consts.len() - 1) as u32
    }

    /// Compile `expr`'s ops into `ops` (the value ends on the stack).
    fn compile(&mut self, expr: &Expr, ops: &mut Vec<Op>) -> Result<()> {
        match expr {
            Expr::Literal(lit) => self.compile_literal(lit, ops),
            Expr::Str(s) => self.compile_str(s, ops),
            Expr::Path(p) => self.compile_path(p, ops),
            Expr::Ident(id) => self.compile_ident(id, ops),
            Expr::Paren(p) => {
                let inner = p
                    .expr()
                    .ok_or_else(|| CompileError::Parse("empty parens".into()))?;
                self.compile(&inner, ops)
            }
            Expr::UnaryOp(op) => self.compile_unary(op, ops),
            Expr::BinOp(op) => self.compile_binop(op, ops),
            Expr::IfElse(ie) => self.compile_if(ie, ops),
            Expr::List(l) => self.compile_list(l, ops),
            Expr::LetIn(li) => self.compile_let(li, ops),
            Expr::Lambda(lam) => self.compile_lambda(lam, ops),
            Expr::Apply(ap) => self.compile_apply(ap, ops),
            Expr::AttrSet(a) => self.compile_attrset(a, ops),
            Expr::Select(sel) => self.compile_select(sel, ops),
            Expr::HasAttr(ha) => self.compile_hasattr(ha, ops),
            Expr::With(w) => self.compile_with(w, ops),
            Expr::Assert(a) => self.compile_assert(a, ops),
            Expr::LegacyLet(ll) => self.compile_legacy_let(ll, ops),
            other => Err(CompileError::Unimplemented(node_name(other).to_owned())),
        }
    }

    /// Compile `expr` as a fresh thunk unit capturing the current scope.
    /// Trivial expressions (constants) skip the thunk.
    fn compile_thunk(&mut self, expr: &Expr) -> Result<Op> {
        let mut ops = Vec::new();
        self.compile(expr, &mut ops)?;
        ops.push(Op::Ret);
        let unit = self.push_unit(CodeUnit { ops, param: None });
        Ok(Op::Thunk { unit })
    }

    /// cppnix's `Expr::maybeThunk`: a bare variable in a lazy position is
    /// passed as the binding's own cell rather than a fresh thunk over it, so
    /// two references to one binding ARE one value. That is the documented
    /// value-identity optimization, and it is what makes `[ f ] == [ f ]`
    /// true while `f == f` is false -- equality short-circuits on cell
    /// identity, which a per-reference thunk would destroy.
    ///
    /// Only for positions whose op runs in the environment it was compiled
    /// against. let/rec fill ops run one frame out from their own scope (the
    /// frame does not exist until PushEnv), so those keep `compile_thunk`,
    /// whose captured env PushEnv repoints.
    fn compile_lazy(&mut self, expr: &Expr) -> Result<Op> {
        if let Expr::Ident(id) = expr
            && let Some(tok) = id.ident_token()
        {
            let mut probe = Vec::new();
            if self.compile_var(tok.text(), &mut probe).is_ok()
                && let [Op::GetLocal { depth, slot }] = probe.as_slice()
            {
                return Ok(Op::GetLocalLazy {
                    depth: *depth,
                    slot: *slot,
                });
            }
        }
        self.compile_thunk(expr)
    }

    fn compile_literal(&mut self, lit: &ast::Literal, ops: &mut Vec<Op>) -> Result<()> {
        let c = match lit.kind() {
            ast::LiteralKind::Integer(i) => Const::Int(
                i.value()
                    .map_err(|e| CompileError::Parse(format!("bad integer literal: {e}")))?,
            ),
            ast::LiteralKind::Float(f) => Const::Float(
                f.value()
                    .map_err(|e| CompileError::Parse(format!("bad float literal: {e}")))?,
            ),
            ast::LiteralKind::Uri(u) => Const::Str(u.syntax().text().to_string()),
        };
        let idx = self.konst(c);
        ops.push(Op::Const(idx));
        Ok(())
    }

    fn compile_str(&mut self, s: &ast::Str, ops: &mut Vec<Op>) -> Result<()> {
        let parts = s.normalized_parts();
        let mut n: u16 = 0;
        for part in &parts {
            match part {
                ast::InterpolPart::Literal(text) => {
                    let idx = self.konst(Const::Str(text.clone()));
                    ops.push(Op::Const(idx));
                    n += 1;
                }
                ast::InterpolPart::Interpolation(ip) => {
                    let inner = ip
                        .expr()
                        .ok_or_else(|| CompileError::Parse("empty interpolation".into()))?;
                    self.compile(&inner, ops)?;
                    n += 1;
                }
            }
        }
        if n == 0 {
            let idx = self.konst(Const::Str(String::new()));
            ops.push(Op::Const(idx));
        } else if n > 1 || !matches!(parts.first(), Some(ast::InterpolPart::Literal(_))) {
            // Interpolation coerces to string even for a single part.
            ops.push(Op::ConcatStrings { n });
        }
        Ok(())
    }

    fn compile_path(&mut self, p: &ast::Path, ops: &mut Vec<Op>) -> Result<()> {
        let mut has_interpol = false;
        for part in p.parts() {
            if matches!(part, ast::InterpolPart::Interpolation(_)) {
                has_interpol = true;
            }
        }
        if has_interpol {
            return Err(CompileError::Unimplemented("path interpolation".into()));
        }
        let text = p.syntax().text().to_string();
        if text.starts_with('<') {
            return Err(CompileError::Unimplemented("search path lookup".into()));
        }
        let abs = if text.starts_with('/') {
            normalize_path(&text)
        } else if let Some(rest) = text.strip_prefix("~/") {
            return Err(CompileError::Unimplemented(format!("home path ~/{rest}")));
        } else {
            normalize_path(&format!("{}/{}", self.base_dir, text))
        };
        let idx = self.konst(Const::Path(abs));
        ops.push(Op::Const(idx));
        Ok(())
    }

    fn compile_ident(&mut self, id: &ast::Ident, ops: &mut Vec<Op>) -> Result<()> {
        let name = id
            .ident_token()
            .ok_or_else(|| CompileError::Parse("identifier without token".into()))?
            .text()
            .to_string();
        self.compile_var(&name, ops)
    }

    fn compile_var(&mut self, name: &str, ops: &mut Vec<Op>) -> Result<()> {
        // Static scopes, innermost first.
        let mut depth: u16 = 0;
        let mut crossed_with = false;
        for frame in self.scopes.iter().rev() {
            match frame {
                ScopeFrame::Bindings(names) => {
                    if let Some(slot) = names.iter().position(|n| n == name) {
                        if crossed_with {
                            // Static binding still wins over any inner with;
                            // depth counts only binding frames at runtime.
                        }
                        ops.push(Op::GetLocal {
                            depth,
                            slot: slot as u16,
                        });
                        return Ok(());
                    }
                    depth += 1;
                }
                ScopeFrame::With => {
                    crossed_with = true;
                    depth += 1;
                }
            }
        }
        // Builtin scope.
        match name {
            "true" => {
                let idx = self.konst(Const::Bool(true));
                ops.push(Op::Const(idx));
                return Ok(());
            }
            "false" => {
                let idx = self.konst(Const::Bool(false));
                ops.push(Op::Const(idx));
                return Ok(());
            }
            "null" => {
                let idx = self.konst(Const::Null);
                ops.push(Op::Const(idx));
                return Ok(());
            }
            _ => {}
        }
        if name == "builtins" {
            ops.push(Op::BuiltinsSet);
            return Ok(());
        }
        // Bare-global resolution mirrors cppnix registration spelling: a
        // primop registered as "__length" is global ONLY as __length (bare
        // `length` is an undefined variable), one registered as "map" is
        // global as map. Implemented names bind; known-but-unimplemented
        // ones compile to a runtime unimplemented report.
        if builtins::is_cpp_global(name) {
            let impl_name = name.strip_prefix("__").unwrap_or(name);
            if let Some(idx) = builtins::global_index(impl_name) {
                ops.push(Op::Builtin { idx });
            } else {
                let sym = self.intern(name);
                ops.push(Op::UnimplementedGlobal { sym });
            }
            return Ok(());
        }
        if crossed_with {
            let sym = self.intern(name);
            ops.push(Op::ResolveWith { sym });
            return Ok(());
        }
        Err(CompileError::UndefinedVariable(name.to_owned()))
    }

    fn compile_unary(&mut self, op: &ast::UnaryOp, ops: &mut Vec<Op>) -> Result<()> {
        let operand = op
            .expr()
            .ok_or_else(|| CompileError::Parse("unary op without operand".into()))?;
        self.compile(&operand, ops)?;
        match op.operator() {
            Some(ast::UnaryOpKind::Negate) => ops.push(Op::Negate),
            Some(ast::UnaryOpKind::Invert) => ops.push(Op::Not),
            None => return Err(CompileError::Parse("unknown unary operator".into())),
        }
        Ok(())
    }

    fn compile_binop(&mut self, op: &ast::BinOp, ops: &mut Vec<Op>) -> Result<()> {
        use ast::BinOpKind;
        let kind = op
            .operator()
            .ok_or_else(|| CompileError::Parse("unknown binary operator".into()))?;
        let (lhs, rhs) = match (op.lhs(), op.rhs()) {
            (Some(l), Some(r)) => (l, r),
            _ => return Err(CompileError::Parse("binary op missing operand".into())),
        };
        // Short-circuiting forms compile to jumps; the rest are strict.
        match kind {
            BinOpKind::And => {
                self.compile(&lhs, ops)?;
                let jump_at = ops.len();
                ops.push(Op::JumpIfFalse { target: 0 });
                self.compile(&rhs, ops)?;
                let end = ops.len();
                ops.push(Op::Jump { target: 1 });
                // false branch: push false
                let f = self.konst(Const::Bool(false));
                ops.push(Op::Const(f));
                self.patch_jump(ops, jump_at, end + 1)?;
                Ok(())
            }
            BinOpKind::Or => {
                self.compile(&lhs, ops)?;
                ops.push(Op::Not);
                let jump_at = ops.len();
                ops.push(Op::JumpIfFalse { target: 0 });
                self.compile(&rhs, ops)?;
                let end = ops.len();
                ops.push(Op::Jump { target: 1 });
                let t = self.konst(Const::Bool(true));
                ops.push(Op::Const(t));
                self.patch_jump(ops, jump_at, end + 1)?;
                Ok(())
            }
            BinOpKind::Implication => {
                self.compile(&lhs, ops)?;
                let jump_at = ops.len();
                ops.push(Op::JumpIfFalse { target: 0 });
                self.compile(&rhs, ops)?;
                let end = ops.len();
                ops.push(Op::Jump { target: 1 });
                let t = self.konst(Const::Bool(true));
                ops.push(Op::Const(t));
                self.patch_jump(ops, jump_at, end + 1)?;
                Ok(())
            }
            _ => {
                self.compile(&lhs, ops)?;
                self.compile(&rhs, ops)?;
                ops.push(match kind {
                    BinOpKind::Add => Op::Add,
                    BinOpKind::Sub => Op::Sub,
                    BinOpKind::Mul => Op::Mul,
                    BinOpKind::Div => Op::Div,
                    BinOpKind::Equal => Op::Eq,
                    BinOpKind::NotEqual => Op::Neq,
                    BinOpKind::Less => Op::Lt,
                    BinOpKind::LessOrEq => Op::Leq,
                    BinOpKind::More => Op::Gt,
                    BinOpKind::MoreOrEq => Op::Geq,
                    BinOpKind::Concat => Op::ConcatLists,
                    BinOpKind::Update => Op::Update,
                    other => {
                        return Err(CompileError::Unimplemented(format!("operator {other:?}")));
                    }
                });
                Ok(())
            }
        }
    }

    fn patch_jump(&self, ops: &mut [Op], at: usize, dest: usize) -> Result<()> {
        let delta = (dest - at - 1) as u32;
        match ops.get_mut(at) {
            Some(Op::JumpIfFalse { target }) | Some(Op::Jump { target }) => {
                *target = delta;
                Ok(())
            }
            _ => Err(CompileError::Parse("internal: bad jump patch".into())),
        }
    }

    fn compile_if(&mut self, ie: &ast::IfElse, ops: &mut Vec<Op>) -> Result<()> {
        let cond = ie
            .condition()
            .ok_or_else(|| CompileError::Parse("if without condition".into()))?;
        let then = ie
            .body()
            .ok_or_else(|| CompileError::Parse("if without then".into()))?;
        let els = ie
            .else_body()
            .ok_or_else(|| CompileError::Parse("if without else".into()))?;
        self.compile(&cond, ops)?;
        let jf_at = ops.len();
        ops.push(Op::JumpIfFalse { target: 0 });
        self.compile(&then, ops)?;
        let j_at = ops.len();
        ops.push(Op::Jump { target: 0 });
        let else_start = ops.len();
        self.compile(&els, ops)?;
        let end = ops.len();
        self.patch_jump(ops, jf_at, else_start)?;
        self.patch_jump(ops, j_at, end)?;
        Ok(())
    }

    fn compile_list(&mut self, l: &ast::List, ops: &mut Vec<Op>) -> Result<()> {
        let mut n: u16 = 0;
        for item in l.items() {
            let t = self.compile_lazy(&item)?;
            ops.push(t);
            n += 1;
        }
        ops.push(Op::MkList { n });
        Ok(())
    }



    /// A thunk unit that builds one assembled set. Runs with the environment
    /// it was compiled in (a thunk captures the chain unchanged), so depths
    /// inside it need no adjustment.
    fn set_build_unit(&mut self, b: &SetBuild) -> Result<u32> {
        let mut ops = Vec::new();
        self.emit_set_build(b, &mut ops)?;
        ops.push(Op::Ret);
        Ok(self.push_unit(CodeUnit { ops, param: None }))
    }

    /// Emit the ops that leave one assembled set on the stack.
    fn emit_set_build(&mut self, b: &SetBuild, ops: &mut Vec<Op>) -> Result<()> {
        if b.rec {
            return self.emit_rec_set_build(b, ops);
        }
        let mut n: u16 = 0;
        for inh in &b.inherits {
            n += self.emit_inherit_group(inh, ops)?;
        }
        for (name, t) in &b.kids {
            let k = self.konst(Const::Str(name.clone()));
            ops.push(Op::Const(k));
            let op = self.bind_value_op(t)?;
            ops.push(op);
            n += 1;
        }
        for (attr, value) in &b.dynamic {
            self.compile_attr_dynamic(attr, ops)?;
            let op = self.compile_lazy(value)?;
            ops.push(op);
            n += 1;
        }
        ops.push(Op::MkAttrs { n, rec: false });
        Ok(())
    }

    /// The rec shape: a frame of mutually-visible slots, then a set built out
    /// of it. Dynamic names are evaluated inside the frame but do not join
    /// the scope, matching cppnix's dynamicEnv.
    fn emit_rec_set_build(&mut self, b: &SetBuild, ops: &mut Vec<Op>) -> Result<()> {
        let mut names: Vec<String> = Vec::new();
        for inh in &b.inherits {
            for attr in inh.attrs() {
                names.push(static_attr_name(&attr)?);
            }
        }
        names.extend(b.kids.iter().map(|(n, _)| n.clone()));
        if names.iter().any(|n| n == "__overrides") {
            // cppnix injects __overrides' attributes into the rec scope
            // itself, so bindings resolve to the override rather than the
            // sibling. Reporting it beats evaluating the set without the
            // injection, which silently returns the un-overridden value.
            return Err(CompileError::Unimplemented("rec attrset __overrides".into()));
        }
        self.scopes.push(ScopeFrame::Bindings(names.clone()));
        let result = (|| -> Result<()> {
            let mut fill = Vec::new();
            for inh in &b.inherits {
                self.rec_inherit_fill_ops(inh, &mut fill)?;
            }
            for (_, t) in &b.kids {
                // Thunks, never GetLocalLazy: a fill op runs before PushEnv,
                // one frame out from the scope it was compiled against.
                fill.push(match t {
                    BindTree::Leaf(e) => self.compile_thunk(e)?,
                    BindTree::Node(sub) => {
                        let unit = self.set_build_unit(sub)?;
                        Op::Thunk { unit }
                    }
                });
            }
            let n = fill.len() as u16;
            ops.extend(fill);
            ops.push(Op::PushEnv { n });
            let mut m: u16 = 0;
            for (slot, name) in names.iter().enumerate() {
                let k = self.konst(Const::Str(name.clone()));
                ops.push(Op::Const(k));
                ops.push(Op::GetLocalLazy {
                    depth: 0,
                    slot: slot as u16,
                });
                m += 1;
            }
            for (attr, value) in &b.dynamic {
                self.compile_attr_dynamic(attr, ops)?;
                let op = self.compile_lazy(value)?;
                ops.push(op);
                m += 1;
            }
            ops.push(Op::MkAttrs { n: m, rec: false });
            ops.push(Op::PopEnv);
            Ok(())
        })();
        self.scopes.pop();
        result
    }

    /// One binding's value, in a position whose env matches its compilation.
    fn bind_value_op(&mut self, t: &BindTree) -> Result<Op> {
        match t {
            BindTree::Leaf(e) => self.compile_lazy(e),
            BindTree::Node(sub) => {
                let unit = self.set_build_unit(sub)?;
                Ok(Op::Thunk { unit })
            }
        }
    }

    /// `inherit x y;` / `inherit (e) x y;` as (name, value) pairs pushed onto
    /// `ops`; returns how many pairs. Values stay lazy.
    fn emit_inherit_group(&mut self, inh: &ast::Inherit, ops: &mut Vec<Op>) -> Result<u16> {
        let mut n: u16 = 0;
        let mut fill = Vec::new();
        self.inherit_fill_ops(inh, &mut fill)?;
        for (attr, op) in inh.attrs().zip(fill) {
            let name = static_attr_name(&attr)?;
            let k = self.konst(Const::Str(name));
            ops.push(Op::Const(k));
            ops.push(op);
            n += 1;
        }
        Ok(n)
    }

    /// `inherit x;` inside a rec scope resolves x in the ENCLOSING scope, so
    /// `rec { inherit x; }` takes the outer x rather than recursing on its
    /// own. Compiled with the new frame popped, then depths bumped by one:
    /// the thunk runs after PushEnv has repointed it into that frame.
    fn rec_inherit_fill_ops(&mut self, inh: &ast::Inherit, fill: &mut Vec<Op>) -> Result<()> {
        if inh.from().is_some() {
            // `inherit (e) x;`'s subject is an ordinary expression evaluated
            // in the rec scope, so it needs no adjustment.
            return self.inherit_fill_ops(inh, fill);
        }
        let frame = self
            .scopes
            .pop()
            .ok_or_else(|| CompileError::Parse("internal: scope underflow".into()))?;
        let mut outer = Vec::new();
        let r = self.inherit_fill_ops(inh, &mut outer);
        self.scopes.push(frame);
        r?;
        for op in outer {
            let Op::Thunk { unit } = op else {
                fill.push(op);
                continue;
            };
            if let Some(u) = self.module.units.get_mut(unit as usize) {
                for o in &mut u.ops {
                    if let Op::GetLocal { depth, slot } = *o {
                        *o = Op::GetLocal {
                            depth: depth + 1,
                            slot,
                        };
                    }
                }
            }
            fill.push(Op::Thunk { unit });
        }
        Ok(())
    }

    /// The lazy value op for each name in one inherit group.
    fn inherit_fill_ops(&mut self, inh: &ast::Inherit, fill: &mut Vec<Op>) -> Result<()> {
        let from = inh.from();
        for attr in inh.attrs() {
            let name = static_attr_name(&attr)?;
            let sym = self.intern(&name);
            let mut tops = Vec::new();
            match &from {
                Some(f) => {
                    let fe = f
                        .expr()
                        .ok_or_else(|| CompileError::Parse("inherit (…) missing".into()))?;
                    self.compile(&fe, &mut tops)?;
                    tops.push(Op::Select { sym });
                }
                None => self.compile_var(&name, &mut tops)?,
            }
            tops.push(Op::Ret);
            let unit = self.push_unit(CodeUnit {
                ops: tops,
                param: None,
            });
            fill.push(Op::Thunk { unit });
        }
        Ok(())
    }

    fn compile_let(&mut self, li: &ast::LetIn, ops: &mut Vec<Op>) -> Result<()> {
        let b = build_entries(li, true)?;
        let mut names: Vec<String> = Vec::new();
        for inh in &b.inherits {
            for attr in inh.attrs() {
                names.push(static_attr_name(&attr)?);
            }
        }
        names.extend(b.kids.iter().map(|(n, _)| n.clone()));
        if !b.dynamic.is_empty() {
            // `let ${e} = v; in ...` is a parse error in cppnix: a binding
            // whose name is unknown until run time cannot be in scope.
            return Err(CompileError::Parse(
                "dynamic attributes not allowed in let".into(),
            ));
        }
        self.scopes.push(ScopeFrame::Bindings(names));
        let result = (|| -> Result<()> {
            let mut fill = Vec::new();
            for inh in &b.inherits {
                self.rec_inherit_fill_ops(inh, &mut fill)?;
            }
            for (_, t) in &b.kids {
                fill.push(match t {
                    BindTree::Leaf(e) => self.compile_thunk(e)?,
                    BindTree::Node(sub) => {
                        let unit = self.set_build_unit(sub)?;
                        Op::Thunk { unit }
                    }
                });
            }
            let n = fill.len() as u16;
            ops.extend(fill);
            ops.push(Op::PushEnv { n });
            let body = li
                .body()
                .ok_or_else(|| CompileError::Parse("let without body".into()))?;
            self.compile(&body, ops)?;
            ops.push(Op::PopEnv);
            Ok(())
        })();
        self.scopes.pop();
        result
    }

    fn compile_lambda(&mut self, lam: &ast::Lambda, ops: &mut Vec<Op>) -> Result<()> {
        let param = lam
            .param()
            .ok_or_else(|| CompileError::Parse("lambda without parameter".into()))?;
        let body = lam
            .body()
            .ok_or_else(|| CompileError::Parse("lambda without body".into()))?;
        let (param_ir, names) = match &param {
            ast::Param::IdentParam(ip) => {
                let name = ip
                    .ident()
                    .and_then(|i| i.ident_token())
                    .ok_or_else(|| CompileError::Parse("lambda param without name".into()))?
                    .text()
                    .to_string();
                let sym = self.intern(&name);
                (Param::Ident(sym), vec![name])
            }
            ast::Param::Pattern(pat) => {
                let mut fields = Vec::new();
                let mut names = Vec::new();
                // Two passes: defaults see all fields (and the @-binding).
                for entry in pat.pat_entries() {
                    let name = entry
                        .ident()
                        .and_then(|i| i.ident_token())
                        .ok_or_else(|| CompileError::Parse("pattern field without name".into()))?
                        .text()
                        .to_string();
                    names.push(name);
                }
                let bind = match pat.pat_bind() {
                    Some(b) => {
                        let name = b
                            .ident()
                            .and_then(|i| i.ident_token())
                            .ok_or_else(|| CompileError::Parse("@ without name".into()))?
                            .text()
                            .to_string();
                        names.push(name.clone());
                        Some(self.intern(&name))
                    }
                    None => None,
                };
                self.scopes.push(ScopeFrame::Bindings(names.clone()));
                let defaults: Result<Vec<(u32, Option<u32>)>> = pat
                    .pat_entries()
                    .map(|entry| {
                        let name = entry
                            .ident()
                            .and_then(|i| i.ident_token())
                            .map(|t| t.text().to_string())
                            .unwrap_or_default();
                        let sym = self.intern(&name);
                        let default = match entry.default() {
                            Some(d) => {
                                let mut dops = Vec::new();
                                self.compile(&d, &mut dops)?;
                                dops.push(Op::Ret);
                                Some(self.push_unit(CodeUnit {
                                    ops: dops,
                                    param: None,
                                }))
                            }
                            None => None,
                        };
                        Ok((sym, default))
                    })
                    .collect();
                self.scopes.pop();
                fields.extend(defaults?);
                (
                    Param::Formals {
                        fields,
                        ellipsis: pat.ellipsis_token().is_some(),
                        bind,
                    },
                    {
                        let mut names = Vec::new();
                        for entry in pat.pat_entries() {
                            if let Some(t) = entry.ident().and_then(|i| i.ident_token()) {
                                names.push(t.text().to_string());
                            }
                        }
                        if let Some(b) = pat.pat_bind()
                            && let Some(t) = b.ident().and_then(|i| i.ident_token())
                        {
                            names.push(t.text().to_string());
                        }
                        names
                    },
                )
            }
        };
        self.scopes.push(ScopeFrame::Bindings(names));
        let result = (|| -> Result<u32> {
            let mut bops = Vec::new();
            self.compile(&body, &mut bops)?;
            bops.push(Op::Ret);
            Ok(self.push_unit(CodeUnit {
                ops: bops,
                param: Some(param_ir),
            }))
        })();
        self.scopes.pop();
        let unit = result?;
        ops.push(Op::Closure { unit });
        Ok(())
    }

    fn compile_apply(&mut self, ap: &ast::Apply, ops: &mut Vec<Op>) -> Result<()> {
        let f = ap
            .lambda()
            .ok_or_else(|| CompileError::Parse("apply without function".into()))?;
        let arg = ap
            .argument()
            .ok_or_else(|| CompileError::Parse("apply without argument".into()))?;
        if let (Expr::Literal(lit), Expr::Ident(id)) = (&f, &arg)
            && lit.syntax().text_range().end() == id.syntax().text_range().start()
            && id.syntax().text().to_string().starts_with('_')
        {
            // `1_000` is one literal in cppnix and INTEGER+IDENT in rnix, and
            // the two tokens touching is what tells them apart from `f 1 _x`.
            // Reported rather than mis-evaluated as an application (ENG-12140).
            return Err(CompileError::Unimplemented(
                "underscore digit separators in numeric literals".into(),
            ));
        }
        self.compile(&f, ops)?;
        let t = self.compile_lazy(&arg)?;
        ops.push(t);
        ops.push(Op::Apply);
        Ok(())
    }

    /// `let { body = e; ... }`: cppnix's pre-`let ... in` syntax, defined as
    /// the `body` attribute of the equivalent rec set. Deprecated upstream and
    /// still all over the corpus.
    fn compile_legacy_let(&mut self, ll: &ast::LegacyLet, ops: &mut Vec<Op>) -> Result<()> {
        let b = build_entries(ll, true)?;
        self.emit_set_build(&b, ops)?;
        let sym = self.intern("body");
        ops.push(Op::Select { sym });
        Ok(())
    }

    fn compile_attrset(&mut self, a: &ast::AttrSet, ops: &mut Vec<Op>) -> Result<()> {
        let b = absorb(a)?;
        self.emit_set_build(&b, ops)
    }



    fn compile_attr_dynamic(&mut self, attr: &ast::Attr, ops: &mut Vec<Op>) -> Result<()> {
        match attr {
            ast::Attr::Dynamic(d) => {
                let e = d
                    .expr()
                    .ok_or_else(|| CompileError::Parse("empty dynamic attr".into()))?;
                self.compile(&e, ops)
            }
            ast::Attr::Str(s) => self.compile_str(s, ops),
            ast::Attr::Ident(_) => Err(CompileError::Parse("internal: static attr".into())),
        }
    }

    fn compile_select(&mut self, sel: &ast::Select, ops: &mut Vec<Op>) -> Result<()> {
        let base = sel
            .expr()
            .ok_or_else(|| CompileError::Parse("select without base".into()))?;
        let path = sel
            .attrpath()
            .ok_or_else(|| CompileError::Parse("select without attrpath".into()))?;
        let default = sel.default_expr();
        self.compile(&base, ops)?;
        let attrs: Vec<ast::Attr> = path.attrs().collect();
        let guarded = default.is_some();
        for attr in &attrs {
            // With an `or` default, every step selects softly and a miss at
            // any depth reaches OrDefault as the marker; without one, a miss
            // throws at the step that failed, as in cppnix.
            match static_attr_name(attr) {
                Ok(name) => {
                    let sym = self.intern(&name);
                    ops.push(if guarded { Op::SelectSoft { sym } } else { Op::Select { sym } });
                }
                Err(_) => {
                    self.compile_attr_dynamic(attr, ops)?;
                    ops.push(if guarded { Op::SelectSoftDyn } else { Op::SelectDyn });
                }
            }
        }
        if let Some(d) = default {
            let t = self.compile_thunk(&d)?;
            ops.push(t);
            ops.push(Op::OrDefault);
        }
        Ok(())
    }

    fn compile_hasattr(&mut self, ha: &ast::HasAttr, ops: &mut Vec<Op>) -> Result<()> {
        let base = ha
            .expr()
            .ok_or_else(|| CompileError::Parse("? without base".into()))?;
        let path = ha
            .attrpath()
            .ok_or_else(|| CompileError::Parse("? without attrpath".into()))?;
        self.compile(&base, ops)?;
        let attrs: Vec<ast::Attr> = path.attrs().collect();
        let n = attrs.len();
        for (i, attr) in attrs.iter().enumerate() {
            let last = i + 1 == n;
            match static_attr_name(attr) {
                Ok(name) => {
                    let sym = self.intern(&name);
                    if last {
                        ops.push(Op::HasAttr { sym });
                    } else {
                        ops.push(Op::SelectSoft { sym });
                    }
                }
                Err(_) => {
                    self.compile_attr_dynamic(attr, ops)?;
                    if last {
                        ops.push(Op::HasAttrDyn);
                    } else {
                        ops.push(Op::SelectSoftDyn);
                    }
                }
            }
        }
        Ok(())
    }

    fn compile_with(&mut self, w: &ast::With, ops: &mut Vec<Op>) -> Result<()> {
        let ns = w
            .namespace()
            .ok_or_else(|| CompileError::Parse("with without namespace".into()))?;
        let body = w
            .body()
            .ok_or_else(|| CompileError::Parse("with without body".into()))?;
        let t = self.compile_thunk(&ns)?;
        ops.push(t);
        ops.push(Op::PushWith);
        self.scopes.push(ScopeFrame::With);
        let r = self.compile(&body, ops);
        self.scopes.pop();
        r?;
        ops.push(Op::PopEnv);
        Ok(())
    }

    fn compile_assert(&mut self, a: &ast::Assert, ops: &mut Vec<Op>) -> Result<()> {
        let cond = a
            .condition()
            .ok_or_else(|| CompileError::Parse("assert without condition".into()))?;
        let body = a
            .body()
            .ok_or_else(|| CompileError::Parse("assert without body".into()))?;
        self.compile(&cond, ops)?;
        ops.push(Op::Assert);
        self.compile(&body, ops)
    }
}

/// What one binding name is bound to. A `Node` is an attribute set under
/// construction: `a.b = 1; a.c = 2;` is one `a`, and so is
/// `a = { b = 1; }; a = { c = 2; };`, because cppnix's parser merges two
/// bindings of one name whenever both values are set literals.
enum BindTree {
    Leaf(Expr),
    Node(SetBuild),
}

/// An attribute set being assembled from one or more sources.
#[derive(Default)]
struct SetBuild {
    /// From the FIRST literal to occupy this name. cppnix keeps that one's
    /// `rec` and discards any on a set merged in later, so the earlier set's
    /// scope ends up covering the later one's attributes -- NixOS/nix#9020,
    /// which the corpus calls regrettable and pins anyway.
    rec: bool,
    kids: Vec<(String, BindTree)>,
    /// Names known only at run time; never part of a `rec` scope.
    dynamic: Vec<(ast::Attr, Expr)>,
    inherits: Vec<ast::Inherit>,
}

/// A set literal's own entries, as a `SetBuild`.
fn absorb(a: &ast::AttrSet) -> Result<SetBuild> {
    build_entries(a, a.rec_token().is_some())
}

/// `let`, `let { }` and `rec { }` all assemble the same shape.
fn build_entries(a: &impl HasEntry, rec: bool) -> Result<SetBuild> {
    let mut b = SetBuild {
        rec,
        ..SetBuild::default()
    };
    merge_literal(&mut b, a)?;
    Ok(b)
}

/// Fold one set literal's entries into `b`. The literal's own `rec` is NOT
/// consulted: only the first set to claim a name decides that.
fn merge_literal(b: &mut SetBuild, a: &impl HasEntry) -> Result<()> {
    b.inherits.extend(a.inherits());
    for kv in a.attrpath_values() {
        let path = kv
            .attrpath()
            .ok_or_else(|| CompileError::Parse("attr without name".into()))?;
        let value = kv
            .value()
            .ok_or_else(|| CompileError::Parse("attr without value".into()))?;
        match static_path(&path) {
            Ok(comps) => tree_insert(b, &comps, value)?,
            Err(CompileError::Unimplemented(_)) => {
                let attrs: Vec<ast::Attr> = path.attrs().collect();
                match attrs.as_slice() {
                    [only] => b.dynamic.push((only.clone(), value)),
                    // `${e}.x = v` needs an implicit set under a name that
                    // does not exist until run time, so it cannot merge with
                    // anything and cannot be placed in the static tree.
                    _ => {
                        return Err(CompileError::Unimplemented(
                            "dynamic attribute name inside a nested attrpath".into(),
                        ));
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Insert one static attrpath, following cppnix's addAttr: descend through
/// existing set literals, create implicit sets for missing levels, and merge
/// at the leaf when both sides are sets. Anything else is a duplicate.
fn tree_insert(b: &mut SetBuild, path: &[String], value: Expr) -> Result<()> {
    let Some((head, rest)) = path.split_first() else {
        return Err(CompileError::Parse("empty attrpath".into()));
    };
    let existing = b.kids.iter().position(|(n, _)| n == head);
    if rest.is_empty() {
        let Some(i) = existing else {
            b.kids.push((head.clone(), BindTree::Leaf(value)));
            return Ok(());
        };
        // Both sides sets: merge. Otherwise it is a redefinition.
        let Expr::AttrSet(incoming) = &value else {
            return Err(CompileError::Parse(format!(
                "attribute '{head}' already defined"
            )));
        };
        let slot = b
            .kids
            .get_mut(i)
            .ok_or_else(|| CompileError::Parse("internal: lost tree slot".into()))?;
        promote(slot, head)?;
        let (_, BindTree::Node(node)) = slot else {
            return Err(CompileError::Parse("internal: promotion failed".into()));
        };
        return merge_literal(node, incoming);
    }
    let idx = match existing {
        Some(i) => i,
        None => {
            b.kids
                .push((head.clone(), BindTree::Node(SetBuild::default())));
            b.kids.len() - 1
        }
    };
    let slot = b
        .kids
        .get_mut(idx)
        .ok_or_else(|| CompileError::Parse("internal: lost tree slot".into()))?;
    promote(slot, head)?;
    match slot {
        (_, BindTree::Node(node)) => tree_insert(node, rest, value),
        _ => Err(CompileError::Parse("internal: promotion failed".into())),
    }
}

/// Turn a `Leaf` holding a set literal into the `Node` that can absorb more
/// attributes. A leaf holding anything else is a duplicate definition.
fn promote(slot: &mut (String, BindTree), name: &str) -> Result<()> {
    if let (_, BindTree::Leaf(Expr::AttrSet(a))) = slot {
        let built = absorb(a)?;
        slot.1 = BindTree::Node(built);
        return Ok(());
    }
    if matches!(slot.1, BindTree::Node(_)) {
        return Ok(());
    }
    Err(CompileError::Parse(format!(
        "attribute '{name}' already defined"
    )))
}

/// Every static component of an attrpath, or Err if any component is dynamic.
fn static_path(path: &ast::Attrpath) -> Result<Vec<String>> {
    path.attrs().map(|a| static_attr_name(&a)).collect()
}


/// The attr name when it is static (ident or literal string); Err for
/// dynamic names.
fn static_attr_name(attr: &ast::Attr) -> Result<String> {
    match attr {
        ast::Attr::Ident(i) => Ok(i
            .ident_token()
            .ok_or_else(|| CompileError::Parse("attr ident without token".into()))?
            .text()
            .to_string()),
        ast::Attr::Str(s) => {
            let parts = s.normalized_parts();
            match parts.as_slice() {
                [] => Ok(String::new()),
                [ast::InterpolPart::Literal(text)] => Ok(text.clone()),
                _ => Err(CompileError::Unimplemented("dynamic attr name".into())),
            }
        }
        // `${"a"}` is a static name: cppnix folds a dynamic attribute whose
        // expression is a plain string literal at parse time, which is why it
        // is allowed in a `let` (where dynamic names otherwise are not).
        ast::Attr::Dynamic(d) => match d.expr() {
            Some(Expr::Str(s)) => match s.normalized_parts().as_slice() {
                [] => Ok(String::new()),
                [ast::InterpolPart::Literal(text)] => Ok(text.clone()),
                _ => Err(CompileError::Unimplemented("dynamic attr name".into())),
            },
            _ => Err(CompileError::Unimplemented("dynamic attr name".into())),
        },
    }
}

fn node_name(e: &Expr) -> &'static str {
    match e {
        Expr::Apply(_) => "function application",
        Expr::Assert(_) => "assert",
        Expr::AttrSet(_) => "attribute set",
        Expr::BinOp(_) => "binary operator",
        Expr::Error(_) => "parse error node",
        Expr::HasAttr(_) => "has-attr",
        Expr::Ident(_) => "identifier",
        Expr::IfElse(_) => "if",
        Expr::Lambda(_) => "lambda",
        Expr::LegacyLet(_) => "legacy let",
        Expr::LetIn(_) => "let",
        Expr::List(_) => "list",
        Expr::Literal(_) => "literal",
        Expr::Paren(_) => "parentheses",
        Expr::Path(_) => "path literal",
        Expr::Root(_) => "root",
        Expr::Select(_) => "attribute selection",
        Expr::Str(_) => "string",
        Expr::UnaryOp(_) => "unary operator",
        Expr::With(_) => "with",
    }
}
