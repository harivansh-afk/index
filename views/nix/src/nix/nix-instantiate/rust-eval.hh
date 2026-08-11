#pragma once
///@file The nix-instantiate seam into the Rust evaluator (rust/nix-eval-rs).
/// M1 scope: whole-expression evaluation of source text. The declaration is
/// unconditional; without -Drust-eval the definition throws, so the setting
/// still parses and reports a clear error on use.

#include "nix/expr/eval.hh"

namespace nix {

/// Evaluate source text with the Rust backend and print the result the way
/// processExpr would. Throws EvalError on evaluation failure and Error with
/// the marker "rust-eval unimplemented" on constructs the backend (or this
/// bridge) does not cover: attr paths, XML/JSON output.
void rustEvalPrint(
    EvalState & state,
    const std::string & source,
    const std::string & baseDir,
    const Strings & attrPaths,
    int outputKind,
    bool strict);

} // namespace nix
