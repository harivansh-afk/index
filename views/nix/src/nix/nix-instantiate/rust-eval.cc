#include "rust-eval.hh"

#include "nix/util/error.hh"
#include "nix/expr/eval-error.hh"

#include <iostream>

// Spelled `defined(...) &&` rather than a bare `#if`: -Werror=undef makes
// an undefined macro a build error, and the whole point of the #else path
// is to compile when -Drust-eval is off and the macro does not exist.
#if defined(HAVE_RUST_EVAL) && HAVE_RUST_EVAL
#  include "ixe.h"
#endif

namespace nix {

#if defined(HAVE_RUST_EVAL) && HAVE_RUST_EVAL

/// Owns the string the C ABI returns so every exit path frees it.
struct IxeString
{
    char * s = nullptr;

    ~IxeString()
    {
        ixe_string_free(s);
    }

    std::string str() const
    {
        return s ? std::string(s) : std::string();
    }
};

void rustEvalPrint(
    EvalState & state,
    const std::string & source,
    const std::string & baseDir,
    const Strings & attrPaths,
    int outputKind,
    bool strict)
{
    // The lang corpus drives eval through --eval --strict with plain output;
    // name everything else instead of approximating it.
    if (!(attrPaths.size() == 1 && attrPaths.front().empty()))
        throw Error("rust-eval unimplemented: attribute path selection");
    if (outputKind != 0 /* okPlain */)
        throw Error("rust-eval unimplemented: --xml/--json/raw output");
    if (!strict)
        throw Error("rust-eval unimplemented: lazy top-level printing (run with --strict)");

    IxeString out;
    int rc = ixe_eval_expr(
        reinterpret_cast<const unsigned char *>(source.data()),
        source.size(),
        reinterpret_cast<const unsigned char *>(baseDir.data()),
        baseDir.size(),
        &out.s);
    switch (rc) {
    case 0:
        std::cout << out.str() << "\n";
        return;
    case 1:
        state.error<EvalError>("%s", out.str()).debugThrow();
    case 2:
        throw Error("rust-eval unimplemented: %s", out.str());
    case 3:
        state.error<EvalError>("rust-eval parse error: %s", out.str()).debugThrow();
    case 4:
        break;
    case 5: {
        // The same class and trace note cppnix's throw primop produces, so a
        // thrown error reads (and classifies) as a throw rather than as an
        // anonymous evaluation failure. Built directly rather than through
        // EvalErrorBuilder: only the templates libexpr instantiates are
        // linkable here, and the variadic addTrace is not one of them.
        ThrownError e(state, "%s", out.str());
        e.addTrace(nullptr, HintFmt("while calling the '%s' builtin", "throw"));
        throw e;
    }
    case 6:
        throw AssertionError(state, "%s", out.str());
    default:
        throw Error("rust-eval: invalid call into nix-eval-rs (status %d)", rc);
    }
}

#else

void rustEvalPrint(EvalState &, const std::string &, const std::string &, const Strings &, int, bool)
{
    throw Error("this nix was built without the rust evaluator (meson -Drust-eval=true)");
}

#endif

} // namespace nix
