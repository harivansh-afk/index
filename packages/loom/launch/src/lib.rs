//! Shared pieces of the loom launchers: the API-key search order, required
//! env lookup, and an exec that only returns on failure.

use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::Context as _;

/// Disk first, tmpfs second. `/run/secrets` is tmpfs and does not survive a
/// stop/start of a restored fork (measured live), so provisioning persists
/// the key to `/var/lib/loom` and a woken fork reads that copy.
const KEY_PATHS: [&str; 2] = [
    "/var/lib/loom/anthropic_api_key",
    "/run/secrets/anthropic_api_key",
];

/// The first non-empty key file, trailing newline stripped. Both files
/// missing or empty is an error rather than an empty key: claude would only
/// fail later with a less specific message.
pub fn anthropic_api_key() -> anyhow::Result<String> {
    let found = KEY_PATHS
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .find(|raw| !raw.trim().is_empty());
    match found {
        Some(raw) => Ok(raw.trim_end_matches('\n').to_owned()),
        None => anyhow::bail!("no anthropic api key at {}", KEY_PATHS.join(" or ")),
    }
}

/// An env var the wrapper is required to set; absence is a packaging bug.
pub fn required_env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set; the nix wrapper sets it"))
}

/// Exec `cmd`, so a success never returns; the returned error is the exec
/// failure with the program named.
pub fn exec(mut cmd: Command) -> anyhow::Error {
    let program = cmd.get_program().to_owned();
    let err = cmd.exec();
    anyhow::Error::new(err).context(format!("exec {}", program.to_string_lossy()))
}
