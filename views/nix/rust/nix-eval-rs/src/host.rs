//! The evaluator's window onto the filesystem.
//!
//! The VM performs no IO: a builtin that needs a path suspends with
//! `Step::NeedPath` and the scheduler answers, so the machine's only state
//! stays the frame chain and a read is a plain return from `poll`. `Host` is
//! what the scheduler calls. Keeping it behind a trait is what lets the
//! effects kernel record a readset later without the evaluator changing, and
//! what lets tests answer paths without touching a disk.

use std::path::Path;

/// What a path turned out to be. Mirrors cppnix's `readFileType`, whose
/// spellings ("regular", "directory", "symlink", "unknown") are corpus-visible
/// through `builtins.readDir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Unknown,
}

impl FileType {
    pub fn as_str(self) -> &'static str {
        match self {
            FileType::Regular => "regular",
            FileType::Directory => "directory",
            FileType::Symlink => "symlink",
            FileType::Unknown => "unknown",
        }
    }
}

/// Every filesystem question the evaluator can ask. Errors are the message
/// text the evaluator reports, so implementations phrase them the way cppnix
/// does rather than leaking an OS string.
pub trait Host {
    fn read_file(&self, path: &str) -> Result<String, String>;
    fn read_dir(&self, path: &str) -> Result<Vec<(String, FileType)>, String>;
    fn path_exists(&self, path: &str) -> bool;
    fn file_type(&self, path: &str) -> Result<FileType, String>;

    /// The file an `import` of `path` actually reads: a directory imports its
    /// `default.nix`, as cppnix does, so the importing file's own directory
    /// (which relative paths inside it resolve against) is the resolved
    /// file's parent, not the argument's.
    fn resolve_import(&self, path: &str) -> Result<String, String> {
        match self.file_type(path) {
            Ok(FileType::Directory) => Ok(format!("{}/default.nix", path.trim_end_matches('/'))),
            Ok(_) => Ok(path.to_owned()),
            Err(e) => Err(e),
        }
    }
}

/// Reads the real filesystem. The corpus runs `nix-instantiate` against files
/// on disk, so this is what the bridge gets; the effects kernel replaces it
/// with a recording host when readsets arrive.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

impl Host for RealFs {
    fn read_file(&self, path: &str) -> Result<String, String> {
        // cppnix reports a missing path before it reports anything about the
        // contents, and with this wording; the corpus compares the class.
        if !self.path_exists(path) {
            return Err(format!("path '{path}' does not exist"));
        }
        std::fs::read_to_string(path).map_err(|e| format!("cannot read '{path}': {e}"))
    }

    fn read_dir(&self, path: &str) -> Result<Vec<(String, FileType)>, String> {
        // cppnix's two spellings, verbatim: a missing path is reported as
        // missing before anything about directories, and a non-directory is
        // reported with double quotes and no errno tail.
        if !self.path_exists(path) {
            return Err(format!("path '{path}' does not exist"));
        }
        // cppnix names the path it actually opened, so a symlink to a
        // non-directory is reported as its target rather than as the link.
        let shown = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_owned());
        let entries = std::fs::read_dir(path)
            .map_err(|e| format!("cannot read directory \"{shown}\": {}", errno_text(&e)))?;
        let mut out = Vec::new();
        for e in entries {
            let e =
                e.map_err(|e| format!("cannot read directory \"{shown}\": {}", errno_text(&e)))?;
            let name = e.file_name().to_string_lossy().into_owned();
            // Not followed: cppnix reports a symlink as a symlink here, and
            // only resolves it when something reads through it.
            let t = match e.file_type() {
                Ok(t) if t.is_symlink() => FileType::Symlink,
                Ok(t) if t.is_dir() => FileType::Directory,
                Ok(t) if t.is_file() => FileType::Regular,
                _ => FileType::Unknown,
            };
            out.push((name, t));
        }
        // cppnix returns an attrset, which is name-sorted; sorting here keeps
        // the host's answer independent of readdir order.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn path_exists(&self, path: &str) -> bool {
        Path::new(path).symlink_metadata().is_ok() && Path::new(path).metadata().is_ok()
    }

    fn file_type(&self, path: &str) -> Result<FileType, String> {
        let md = std::fs::symlink_metadata(path)
            .map_err(|_| format!("path '{path}' does not exist"))?;
        Ok(if md.file_type().is_symlink() {
            FileType::Symlink
        } else if md.is_dir() {
            FileType::Directory
        } else if md.is_file() {
            FileType::Regular
        } else {
            FileType::Unknown
        })
    }
}

/// The bare strerror text. Rust appends " (os error N)" to its Display, which
/// cppnix does not print, and the corpus compares the message.
fn errno_text(e: &std::io::Error) -> String {
    let full = e.to_string();
    match full.find(" (os error ") {
        Some(i) => full.get(..i).unwrap_or(&full).to_owned(),
        None => full,
    }
}
