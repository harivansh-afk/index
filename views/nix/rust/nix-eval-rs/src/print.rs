//! Strict value printing in nix-instantiate's output format, and the
//! `toString` coercion, as resumable tasks. Byte compatibility with cppnix
//! here is part of the corpus contract, so every rule cites the behavior it
//! mirrors.
//!
//! Both walkers are a token worklist rather than a recursive descent: a
//! printed value can be as deep as the expression that built it, and the
//! printer is exactly the code most likely to meet the deepest one.

use crate::task::Yield;
use crate::value2::{Slot, Value, format_g6, type_name};
use crate::vm::{Result, Vm, VmError};

/// Either a literal already decided on, or a slot whose forced value gets
/// rendered when the machine hands it back.
enum Item {
    Slot(Slot),
    Lit(String),
}

pub struct Print {
    work: Vec<Item>,
    out: String,
}

impl Print {
    pub fn new(v: Value) -> Self {
        Print {
            work: vec![Item::Slot(Slot::value(v))],
            out: String::new(),
        }
    }

    pub fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        if let Some(v) = incoming {
            self.render(vm, &v);
        }
        while let Some(item) = self.work.pop() {
            match item {
                Item::Lit(s) => self.out.push_str(&s),
                Item::Slot(s) => return Ok(Yield::Force(s)),
            }
        }
        Ok(Yield::Done(Value::Str(std::mem::take(&mut self.out).into())))
    }

    fn render(&mut self, vm: &Vm, v: &Value) {
        match v {
            Value::Int(n) => self.out.push_str(&n.to_string()),
            Value::Float(x) => self.out.push_str(&format_g6(*x)),
            Value::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            Value::Null => self.out.push_str("null"),
            Value::Str(s) => print_string(s, &mut self.out),
            Value::Path(p) => self.out.push_str(p),
            Value::List(items) => {
                if items.is_empty() {
                    self.out.push_str("[ ]");
                    return;
                }
                self.out.push('[');
                let mut queued = Vec::with_capacity(items.len() * 2 + 1);
                for s in items.iter() {
                    queued.push(Item::Lit(" ".to_owned()));
                    queued.push(Item::Slot(s.clone()));
                }
                queued.push(Item::Lit(" ]".to_owned()));
                self.queue(queued);
            }
            Value::Attrs(map) => {
                if map.is_empty() {
                    self.out.push_str("{ }");
                    return;
                }
                // cppnix prints attrs sorted by name string, not symbol id.
                let mut entries: Vec<(String, Slot)> = map
                    .iter()
                    .map(|(k, s)| (vm.sym_name(*k).to_owned(), s.clone()))
                    .collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                self.out.push('{');
                let mut queued = Vec::with_capacity(entries.len() * 3 + 1);
                for (name, s) in entries {
                    let mut lead = String::from(" ");
                    print_attr_name(&name, &mut lead);
                    lead.push_str(" = ");
                    queued.push(Item::Lit(lead));
                    queued.push(Item::Slot(s));
                    queued.push(Item::Lit(";".to_owned()));
                }
                queued.push(Item::Lit(" }".to_owned()));
                self.queue(queued);
            }
            Value::Closure(_) => self.out.push_str("<LAMBDA>"),
            Value::Builtin(b) => {
                // cppnix: <PRIMOP> for primops, <PRIMOP-APP> once partially
                // applied.
                if b.args.is_empty() {
                    self.out.push_str("<PRIMOP>");
                } else {
                    self.out.push_str("<PRIMOP-APP>");
                }
            }
        }
    }

    fn queue(&mut self, items: Vec<Item>) {
        for it in items.into_iter().rev() {
            self.work.push(it);
        }
    }
}

/// `toString`: more permissive than interpolation (bools, null, numbers and
/// lists all coerce), and a list joins its elements' coercions with a space.
pub struct Coerce {
    work: Vec<Item>,
    out: String,
}

impl Coerce {
    pub fn new(slot: Slot) -> Self {
        Coerce {
            work: vec![Item::Slot(slot)],
            out: String::new(),
        }
    }

    pub fn step(&mut self, incoming: Option<Value>) -> Result<Yield> {
        if let Some(v) = incoming {
            self.render(&v)?;
        }
        while let Some(item) = self.work.pop() {
            match item {
                Item::Lit(s) => self.out.push_str(&s),
                Item::Slot(s) => return Ok(Yield::Force(s)),
            }
        }
        Ok(Yield::Done(Value::Str(std::mem::take(&mut self.out).into())))
    }

    fn render(&mut self, v: &Value) -> Result<()> {
        match v {
            Value::List(items) => {
                let mut queued = Vec::with_capacity(items.len() * 2);
                for (i, s) in items.iter().enumerate() {
                    if i > 0 {
                        queued.push(Item::Lit(" ".to_owned()));
                    }
                    queued.push(Item::Slot(s.clone()));
                }
                for it in queued.into_iter().rev() {
                    self.work.push(it);
                }
            }
            other => self.out.push_str(&coerce_scalar(other)?),
        }
        Ok(())
    }
}

pub fn coerce_scalar(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Str(s) => s.to_string(),
        Value::Path(p) => p.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => format_g6(*x),
        Value::Bool(true) => "1".to_owned(),
        Value::Bool(false) => String::new(),
        Value::Null => String::new(),
        other => {
            return Err(VmError::eval(format!(
                "cannot coerce {} to a string",
                type_name(other)
            )));
        }
    })
}

/// Quoted-string escaping per cppnix printLiteralString: backslash, quote,
/// newline as \n, CR as \r, tab as \t, and `${` escaped as \${.
fn print_string(s: &str, out: &mut String) {
    out.push('"');
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' => {
                if chars.peek() == Some(&'{') {
                    out.push_str("\\$");
                } else {
                    out.push('$');
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Attr names print bare when they are valid identifiers, quoted otherwise.
fn print_attr_name(name: &str, out: &mut String) {
    let ident = !name.is_empty()
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '\'' || c == '-')
        && !matches!(
            name,
            "if" | "then" | "else" | "assert" | "with" | "let" | "in" | "rec" | "inherit" | "or"
        );
    if ident {
        out.push_str(name);
    } else {
        print_string(name, out);
    }
}
