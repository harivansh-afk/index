//! Claude with the loom key loaded: what the remote exec path and the
//! Elixir side run inside a fork.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let err = match run() {
        Ok(never) => match never {},
        Err(err) => err,
    };
    eprintln!("loom-claude: {err:#}");
    ExitCode::FAILURE
}

fn run() -> anyhow::Result<std::convert::Infallible> {
    let claude = loom_launch::required_env("LOOM_CLAUDE_BIN")?;
    let key = loom_launch::anthropic_api_key()?;
    let mut cmd = Command::new(claude);
    cmd.args(std::env::args_os().skip(1))
        .env("ANTHROPIC_API_KEY", key);
    Err(loom_launch::exec(cmd))
}
