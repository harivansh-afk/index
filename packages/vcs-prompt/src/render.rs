//! Turn a [`crate::jj`] or [`crate::git`] head into the one line starship
//! prints.
//!
//! The binary colors its own output instead of leaning on the custom module's
//! `style`, because one segment carries several colors (branch, counts,
//! in-progress state) the way starship's own `git_*` modules do.

use std::fmt::Write as _;

use anstyle::{AnsiColor, Reset, Style};

use crate::git::{self, HeadName};
use crate::jj;
use crate::views;

/// nf-dev-git_branch, the symbol this prompt used for `git_branch`.
const GIT_SYMBOL: &str = "\u{e0a0} ";
/// nf-md-source_commit_start, the closest thing to a jj mark in Nerd Fonts.
const JJ_SYMBOL: &str = "\u{f15c6} ";

const NAME: Style = AnsiColor::Magenta.on_default().bold();
const COUNTS: Style = AnsiColor::Red.on_default().bold();
const MUTED: Style = Style::new().dimmed();

/// A segment under construction: text plus whether escapes are wanted.
struct Segment {
    text: String,
    color: bool,
}

impl Segment {
    const fn new(color: bool) -> Self {
        Self {
            text: String::new(),
            color,
        }
    }

    fn push(&mut self, style: Style, text: &str) {
        if self.color {
            let _ = write!(self.text, "{}{text}{}", style.render(), Reset.render());
        } else {
            self.text.push_str(text);
        }
    }

    fn push_plain(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn into_text(self) -> String {
        self.text
    }
}

/// `on 󱗆 lsurukvy ix-patched+1 *`: the working-copy change id, the bookmark it
/// descends from, and the state flags. There is no separate dirty count: in jj
/// the edits are already in @, so a non-empty working copy is the dirty
/// signal.
pub fn jj(head: &jj::Head, view: Option<&views::View>, color: bool) -> String {
    let mut segment = Segment::new(color);
    segment.push_plain("on ");
    segment.push(NAME, JJ_SYMBOL);
    segment.push(NAME, &head.change_prefix);
    segment.push(MUTED, &head.change_rest);

    if let Some(bookmark) = &head.bookmark {
        segment.push_plain(" ");
        segment.push(NAME, &bookmark.names);
        if bookmark.distance > 0 {
            segment.push(MUTED, &format!("+{}", bookmark.distance));
        }
    }

    let mut flags = String::new();
    if head.flags.conflict {
        flags.push('=');
    }
    if head.flags.divergent {
        flags.push_str("??");
    }
    if !head.flags.empty {
        flags.push('*');
    }
    if !flags.is_empty() {
        segment.push_plain(" ");
        segment.push(COUNTS, &flags);
    }

    // The view the directory is inside: context the way the submodule
    // breadcrumb is, so the name is muted, with the last survey's counts
    // against the published repository beside it.
    if let Some(view) = view {
        segment.push_plain(" ");
        segment.push(MUTED, &view.name);
        if let Some(counts) = view.counts {
            let arrows = ahead_behind(counts.ahead, counts.behind);
            if !arrows.is_empty() {
                segment.push(COUNTS, &arrows);
            }
        }
    }

    segment.into_text()
}

/// `on  main !2?1⇡2`, matching the symbols the disabled `git_branch` and
/// `git_status` modules were configured with.
pub fn git(head: &git::Head, color: bool) -> String {
    let mut segment = Segment::new(color);
    segment.push_plain("on ");
    segment.push(NAME, GIT_SYMBOL);
    match &head.name {
        HeadName::Branch(branch) => segment.push(NAME, branch),
        HeadName::Detached(commit) => segment.push(NAME, &format!("({commit})")),
    }

    let counts = counts(&head.counts, head.tracking);
    if !counts.is_empty() {
        segment.push_plain(" ");
        segment.push(COUNTS, &counts);
    }

    segment.into_text()
}

/// Status counts in starship's `$all_status$ahead_behind` order and symbols,
/// so the segment reads the same as before the modules moved in here.
fn counts(counts: &git::Counts, tracking: Option<git::Tracking>) -> String {
    let mut rendered = String::new();
    for (symbol, count) in [
        ("=", counts.conflicted),
        ("✘", counts.deleted),
        ("»", counts.renamed),
        ("!", counts.modified),
        ("+", counts.staged),
        ("?", counts.untracked),
    ] {
        if count > 0 {
            let _ = write!(rendered, "{symbol}{count}");
        }
    }

    if let Some(git::Tracking { ahead, behind }) = tracking {
        rendered.push_str(&ahead_behind(ahead, behind));
    }

    rendered
}

/// The ahead/behind arrows, shared by the git tracking counts and the jj
/// view counts so the two read identically.
fn ahead_behind(ahead: usize, behind: usize) -> String {
    match (ahead, behind) {
        (0, 0) => String::new(),
        (ahead, 0) => format!("⇡{ahead}"),
        (0, behind) => format!("⇣{behind}"),
        (ahead, behind) => format!("⇕⇡{ahead}⇣{behind}"),
    }
}

#[cfg(test)]
mod tests {
    use crate::git::{Counts as GitCounts, Head as GitHead, HeadName, Tracking};
    use crate::jj::{Bookmark, Flags, Head as JjHead};
    use crate::views::{Counts, View};

    #[test]
    fn a_jj_working_copy_reads_change_bookmark_distance_then_flags() {
        let head = JjHead {
            change_prefix: "l".to_owned(),
            change_rest: "surukvy".to_owned(),
            flags: Flags {
                empty: false,
                conflict: false,
                divergent: false,
            },
            bookmark: Some(Bookmark {
                names: "ix-patched".to_owned(),
                distance: 2,
            }),
        };

        assert_eq!(super::jj(&head, None, false), "on \u{f15c6} lsurukvy ix-patched+2 *");
    }

    #[test]
    fn an_empty_jj_working_copy_on_its_bookmark_shows_neither_flag_nor_distance() {
        let head = JjHead {
            change_prefix: "q".to_owned(),
            change_rest: "pzxrtln".to_owned(),
            flags: Flags {
                empty: true,
                conflict: false,
                divergent: false,
            },
            bookmark: Some(Bookmark {
                names: "main".to_owned(),
                distance: 0,
            }),
        };

        assert_eq!(super::jj(&head, None, false), "on \u{f15c6} qpzxrtln main");
    }

    #[test]
    fn a_view_directory_names_the_view_and_the_last_surveys_counts() {
        let head = JjHead {
            change_prefix: "p".to_owned(),
            change_rest: "sqrptoq".to_owned(),
            flags: Flags {
                empty: true,
                conflict: false,
                divergent: false,
            },
            bookmark: Some(Bookmark {
                names: "main".to_owned(),
                distance: 1,
            }),
        };
        let view = View {
            name: "ix".to_owned(),
            counts: Some(Counts {
                behind: 25,
                ahead: 0,
            }),
        };

        assert_eq!(
            super::jj(&head, Some(&view), false),
            "on \u{f15c6} psqrptoq main+1 ix⇣25"
        );
    }

    #[test]
    fn a_view_with_nothing_to_report_is_just_its_name() {
        let head = JjHead {
            change_prefix: "q".to_owned(),
            change_rest: "pzxrtln".to_owned(),
            flags: Flags {
                empty: true,
                conflict: false,
                divergent: false,
            },
            bookmark: None,
        };
        for counts in [None, Some(Counts {
            behind: 0,
            ahead: 0,
        })] {
            let view = View {
                name: "ix".to_owned(),
                counts,
            };
            assert_eq!(super::jj(&head, Some(&view), false), "on \u{f15c6} qpzxrtln ix");
        }
    }

    #[test]
    fn git_counts_follow_starship_order_and_symbols() {
        let head = GitHead {
            name: HeadName::Branch("main".to_owned()),
            tracking: Some(Tracking {
                ahead: 2,
                behind: 1,
            }),
            counts: GitCounts {
                modified: 3,
                untracked: 1,
                ..GitCounts::default()
            },
        };

        assert_eq!(super::git(&head, false), "on \u{e0a0} main !3?1⇕⇡2⇣1");
    }

    #[test]
    fn a_detached_head_shows_the_commit_in_parentheses() {
        let head = GitHead {
            name: HeadName::Detached("c1b4a88".to_owned()),
            tracking: None,
            counts: GitCounts::default(),
        };

        assert_eq!(super::git(&head, false), "on \u{e0a0} (c1b4a88)");
    }
}
