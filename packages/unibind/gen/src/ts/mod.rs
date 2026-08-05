//! Emit the TypeScript host files for one interface.
//!
//! Three files land at the npm package root: `index.d.ts` types every export
//! (`TSDoc` from the IR's doc comments), `schemas.ts` carries a Zod schema
//! per record and per enumeration for consumers that want the same shapes
//! checked at runtime,
//! and the `CommonJS` `index.js` wraps the native addon into the surface
//! users import: decoded `Error` subclasses, async functions forwarding a
//! trailing `AbortSignal`, streams as `AsyncIterable`s, and object classes
//! with the resource close surface (`await using` works). The wrapper pairs
//! with the glue the `ts`-feature macro backend (`unibind-backend-ts`)
//! compiled into the addon: everything dynamic crosses inside `__unibind__:`
//! napi rejection reasons, and `index.js` is where those reasons become real
//! exception types.

mod dts;
mod js;
mod types;
mod zod;

use unibind_core::docs;
use unibind_core::ir::Interface;

use crate::host::{EmitError, HostEmitter, HostFile};

/// The TypeScript emitter; writes `index.d.ts`, `schemas.ts`, and
/// `index.js` at the output root.
pub struct TsEmitter {
    /// Basename of the native addon: the generated `index.js` loads
    /// `./native/<addon>.node`, so the packaging step must place the
    /// compiled cdylib there.
    pub addon: String,
}

impl HostEmitter for TsEmitter {
    fn target(&self) -> &'static str {
        "ts"
    }

    fn emit(&self, interface: &Interface) -> Result<Vec<HostFile>, EmitError> {
        // Doc comments are written against the Rust surface, so their
        // intra-doc links are resolved into TSDoc `{@link ...}` references
        // here, once, before any of the three files renders one.
        let interface = &docs::resolve(interface, docs::Language::Ts)
            .map_err(|error| EmitError { message: error.to_string() })?;
        let mut files = vec![HostFile {
            path: "index.d.ts".to_owned(),
            contents: dts::render(interface)?,
        }];
        // Records and enumerations are the only things with a Zod schema, so
        // an interface without either would land a file whose sole content is
        // an unused `zod` import -- and a `zod` peer dependency the package
        // does not need. No flag: the schemas come from the same IR as the
        // types, so making them optional would only let the two drift.
        if !interface.records.is_empty() || !interface.enums.is_empty() {
            files.push(HostFile {
                path: "schemas.ts".to_owned(),
                contents: zod::render(interface)?,
            });
        }
        files.push(HostFile {
            path: "index.js".to_owned(),
            contents: js::render(interface, &self.addon),
        });
        Ok(files)
    }
}
