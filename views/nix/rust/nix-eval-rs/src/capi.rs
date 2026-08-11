//! The C ABI cppnix calls. Deliberately tiny for M1: evaluate a source
//! string, get back a status and a malloc'd string the caller frees. The
//! full handle-table API from the architecture plan replaces this when
//! values outgrow strings; the status contract below is the part meant to
//! survive.

use crate::eval::EvalError;
use crate::vm::ErrKind;
use std::ffi::{c_char, CString};
use std::slice;

/// Status contract, kept in step with ixe.h and the C++ bridge:
/// 0 value produced; 1 evaluation error (cppnix should throw EvalError);
/// 2 unimplemented construct (cppnix should throw with the marker the
/// harnesses grep, "rust-eval unimplemented"); 3 parse error; 4 bad call;
/// 5 `builtins.throw` (ThrownError); 6 failed assert (AssertionError).
///
/// 5 and 6 exist because the exception class is not recoverable from the
/// message: cppnix reports a throw as ThrownError under "while calling the
/// 'throw' builtin", and collapsing every failure into EvalError loses the
/// distinction a reader (and the corpus differ) reads the class from.
const IXE_OK: i32 = 0;
const IXE_ERR_EVAL: i32 = 1;
const IXE_ERR_UNIMPLEMENTED: i32 = 2;
const IXE_ERR_PARSE: i32 = 3;
const IXE_ERR_BADCALL: i32 = 4;
const IXE_ERR_THROWN: i32 = 5;
const IXE_ERR_ASSERT: i32 = 6;

fn out_string(s: String, out: *mut *mut c_char) -> i32 {
    // A NUL inside the payload is not a caller error, so it must not be
    // reported as one: the evaluator rejects NUL-bearing Nix strings, and
    // anything that still reaches here is rendered the way cppnix renders
    // one rather than collapsing into "invalid call".
    let Ok(c) = CString::new(s.replace('\0', "\u{2400}")) else {
        return IXE_ERR_BADCALL;
    };
    // SAFETY: out is non-null, checked by the sole caller before dispatch.
    unsafe { *out = c.into_raw() };
    IXE_OK
}

/// Evaluate UTF-8 Nix source. On any return, *out (when non-null) is a
/// malloc'd C string: the rendered value for status 0, the message
/// otherwise. Free with ixe_string_free.
///
/// # Safety
/// `src` must point to `src_len` readable bytes; `out` must be a valid
/// non-null pointer to write one pointer through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_eval_expr(
    src: *const u8,
    src_len: usize,
    base_dir: *const u8,
    base_dir_len: usize,
    out: *mut *mut c_char,
) -> i32 {
    if src.is_null() || out.is_null() {
        return IXE_ERR_BADCALL;
    }
    let base = if base_dir.is_null() || base_dir_len == 0 {
        ".".to_owned()
    } else {
        // SAFETY: caller contract; base_dir points to base_dir_len bytes.
        let b = unsafe { slice::from_raw_parts(base_dir, base_dir_len) };
        match std::str::from_utf8(b) {
            Ok(s) => s.to_owned(),
            Err(_) => return IXE_ERR_BADCALL,
        }
    };
    // SAFETY: caller contract above.
    let bytes = unsafe { slice::from_raw_parts(src, src_len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        // cppnix accepts arbitrary bytes inside string literals; this
        // pipeline is &str end to end, so non-UTF-8 source is a coverage
        // gap to report, not a caller error.
        let rc = out_string("non-UTF-8 source".to_owned(), out);
        return if rc == IXE_OK { IXE_ERR_UNIMPLEMENTED } else { rc };
    };
    let (status, msg) = match crate::eval::eval_str_at(text, &base) {
        Ok(v) => (IXE_OK, v.to_string()),
        Err(EvalError::Unimplemented(what)) => (IXE_ERR_UNIMPLEMENTED, what),
        Err(EvalError::Eval(kind, msg)) => (
            match kind {
                ErrKind::Eval => IXE_ERR_EVAL,
                ErrKind::Thrown => IXE_ERR_THROWN,
                ErrKind::Assertion => IXE_ERR_ASSERT,
            },
            msg,
        ),
        Err(EvalError::Parse(msg)) => (IXE_ERR_PARSE, msg),
    };
    let rc = out_string(msg, out);
    if rc == IXE_OK { status } else { rc }
}

/// Free a string returned through `out` by ixe_eval_expr.
///
/// # Safety
/// `s` must be a pointer previously returned via ixe_eval_expr's out
/// parameter, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: ownership round-trip of a CString::into_raw pointer.
        drop(unsafe { CString::from_raw(s) });
    }
}
