//! The `loom` entry inside the control VM: load the key, then run claude
//! under an identity watcher.
//!
//! A snapshot restore resumes this process in the child VM. The watcher
//! compares the VM's global addresses to a baseline once a second and
//! terminates claude when they differ, so the cloned parent session exits
//! in the fork; the original session keeps running because its addresses
//! never change.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::process::{Child, Command, ExitCode, ExitStatus};
use std::time::Duration;

use anyhow::Context as _;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("loom-launch: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let claude = loom_launch::required_env("LOOM_CLAUDE_BIN")?;
    let prompt = loom_launch::required_env("LOOM_PROMPT_FILE")?;
    let key = loom_launch::anthropic_api_key()?;
    let baseline = global_addrs()?;
    let child = Command::new(claude)
        .arg(format!("--append-system-prompt-file={prompt}"))
        .args(std::env::args_os().skip(1))
        .env("ANTHROPIC_API_KEY", key)
        .env("IS_SANDBOX", "1")
        .spawn()
        .context("spawn claude")?;
    watch_identity(child, &baseline)
}

/// Reap claude, or terminate it the moment the VM's addresses stop matching
/// `baseline`. A probe failure also terminates: an unreadable address table
/// cannot vouch for the identity that decides whether this session may run.
fn watch_identity(mut child: Child, baseline: &BTreeSet<IpAddr>) -> anyhow::Result<ExitCode> {
    loop {
        if let Some(status) = child.try_wait().context("wait for claude")? {
            return Ok(exit_code(status));
        }
        std::thread::sleep(Duration::from_secs(1));
        let matches_baseline = global_addrs().is_ok_and(|current| current == *baseline);
        if !matches_baseline {
            terminate(&child);
            let status = child.wait().context("wait for terminated claude")?;
            return Ok(exit_code(status));
        }
    }
}

fn terminate(child: &Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // ESRCH here means claude exited between try_wait and now; wait()
        // right after still reaps it.
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
}

/// The set `ip -o addr show scope global` would print: every configured
/// address except loopback and link-local.
fn global_addrs() -> anyhow::Result<BTreeSet<IpAddr>> {
    let addrs = nix::ifaddrs::getifaddrs().context("getifaddrs")?;
    let set = addrs
        .filter_map(|ifaddr| ifaddr.address.as_ref().and_then(ip_of))
        .filter(global_scope)
        .collect();
    Ok(set)
}

fn ip_of(storage: &nix::sys::socket::SockaddrStorage) -> Option<IpAddr> {
    if let Some(v4) = storage.as_sockaddr_in() {
        Some(IpAddr::V4(v4.ip()))
    } else {
        storage.as_sockaddr_in6().map(|v6| IpAddr::V6(v6.ip()))
    }
}

fn global_scope(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local(),
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unicast_link_local(),
    }
}

fn exit_code(status: ExitStatus) -> ExitCode {
    if status.success() {
        ExitCode::SUCCESS
    } else {
        let code = status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1);
        ExitCode::from(code)
    }
}
