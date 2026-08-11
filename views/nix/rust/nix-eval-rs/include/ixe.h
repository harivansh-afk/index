#pragma once
/* C ABI of nix-eval-rs (rust/nix-eval-rs/src/capi.rs). Hand-written for the
 * M1 slice; switches to cbindgen output when the surface grows past two
 * functions. Keep the status values in step with capi.rs. */

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 0 ok; 1 eval error; 2 unimplemented construct; 3 parse error; 4 bad call;
 * 5 builtins.throw (ThrownError); 6 failed assert (AssertionError). The last
 * two are separate because the exception class cannot be read back out of
 * the message, and cppnix reports each under its own trace note. */
int ixe_eval_expr(
    const unsigned char * src,
    size_t src_len,
    const unsigned char * base_dir, /* directory for relative paths; NULL = cwd */
    size_t base_dir_len,
    char ** out);
void ixe_string_free(char * s);

#ifdef __cplusplus
}
#endif
