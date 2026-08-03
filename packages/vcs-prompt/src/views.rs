//! Which configured jj view owns the prompt directory, and where it stands.
//!
//! `jj views prompt` is the seam: it maps the directory to the repository's
//! `views` config table and reads the survey record the last `jj views fetch`
//! or `jj views status` left. The counts are therefore as fresh as the last
//! survey and cost a config read plus a file read, where computing them fresh
//! is a derive over the view's whole history -- seconds, which no prompt can
//! pay.

use std::path::Path;
use std::process::Command;

/// The view the prompt directory sits inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub name: String,
    /// Counts from the last survey; `None` for a view never surveyed.
    pub counts: Option<Counts>,
}

/// How the view stood against its published repository at the last survey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Published commits that had not arrived here.
    pub behind: usize,
    /// View commits here the published repository did not have.
    pub ahead: usize,
}

/// The view owning `cwd` in the workspace at `root`, or `None` outside every
/// view.
///
/// Every failure is also `None`: stock jj exits nonzero on the unknown
/// subcommand, and a segment that goes missing beats one that renders an
/// error. The working-copy state still renders either way, so a missing view
/// segment stays visible as exactly that.
pub fn at(root: &Path, cwd: &Path) -> Option<View> {
    let output = Command::new("jj")
        .args(["views", "prompt", "--repository"])
        .arg(root)
        .args(["--ignore-working-copy", "--color=never", "--quiet"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&String::from_utf8(output.stdout).ok()?)
}

/// One `name<TAB>behind<TAB>ahead` line, or bare `name` for a view never
/// surveyed, or nothing at all outside every view.
fn parse(stdout: &str) -> Option<View> {
    let line = stdout.lines().next()?;
    let mut columns = line.split('\t');
    let name = columns.next().filter(|name| !name.is_empty())?.to_owned();
    let counts = match (columns.next(), columns.next()) {
        (Some(behind), Some(ahead)) => Some(Counts {
            behind: behind.parse().ok()?,
            ahead: ahead.parse().ok()?,
        }),
        _ => None,
    };
    Some(View { name, counts })
}

#[cfg(test)]
mod tests {
    use super::{Counts, View, parse};

    #[test]
    fn a_surveyed_view_carries_its_counts() {
        assert_eq!(
            parse("ix\t25\t1\n"),
            Some(View {
                name: "ix".to_owned(),
                counts: Some(Counts {
                    behind: 25,
                    ahead: 1,
                }),
            })
        );
    }

    #[test]
    fn a_view_never_surveyed_is_a_bare_name() {
        assert_eq!(
            parse("ix\n"),
            Some(View {
                name: "ix".to_owned(),
                counts: None,
            })
        );
    }

    #[test]
    fn outside_every_view_there_is_nothing_to_parse() {
        assert_eq!(parse(""), None);
    }
}
