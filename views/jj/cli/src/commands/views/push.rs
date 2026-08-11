// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write as _;

use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_views::Cache;
use tracing::instrument;

use super::Freshness;
use super::Git;
use super::Position;
use super::ViewConfig;
use super::get_views_config;
use super::select_views;
use super::survey;
use super::validate_views;
use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::cli_util::WorkspaceCommandHelper;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::command_error::user_error_with_message;
use crate::complete;
use crate::ui::Ui;

/// Namespace the derived tip of each view is remembered under.
///
/// The ref is written before the push, not after, for two reasons: the objects
/// a derive produced are unreachable until something names them, so a failed
/// push would otherwise leave them for `git gc`; and a retry after a network
/// failure then costs nothing, because the derive is already cached in the
/// store rather than only in this process.
const VIEW_REF_NAMESPACE: &str = "refs/jj/views/";

/// Derive each configured view and push it to the repository it belongs to
///
/// A view is a path prefix of this repository that is also published as a
/// repository of its own. Deriving one produces a history whose hashes are
/// exactly the published repository's, so what this command sends is an
/// ordinary branch that repository can fast-forward.
///
/// By default the branch is a new one named after the revision's change ID, and
/// the command prints a URL to open a pull request from it. Writing a view's
/// own default branch takes `--branch` naming it *and*
/// `--allow-default-branch`.
///
/// This does not push the repository you are in. That push is `jj git push`,
/// which has bookmarks, tracking and force-with-lease semantics of its own, and
/// a second implementation of them here would be a second set of bugs. Run
/// both.
///
/// The command reads each view's published default branch before pushing. When
/// it exists, the branch is fetched for an exact comparison. An integrated
/// derived tip with the same root tree has no reviewable content, so its local
/// topology is recorded but no remote branch is created.
#[derive(clap::Args, Clone, Debug)]
pub struct ViewsPushArgs {
    /// View to push, by its key in the `views` config table (can be repeated)
    ///
    /// Defaults to every configured view.
    #[arg(value_name = "VIEW")]
    #[arg(add = ArgValueCandidates::new(complete::views))]
    views: Vec<String>,

    /// Revision whose view to push
    #[arg(long, short, default_value = "@", value_name = "REVSET")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    revision: RevisionArg,

    /// Branch to push under, instead of the generated name
    ///
    /// Use the `templates.git_push_bookmark` setting to customize the generated
    /// name. The default is `"push-" ++ change_id.short()`.
    #[arg(long, short, value_name = "NAME")]
    branch: Option<String>,

    /// Permit `--branch` to name a view's own default branch
    ///
    /// Pushing straight to the branch a published repository builds and
    /// releases from skips every review its own repository would have applied.
    /// It is occasionally what you want, and never what you want by accident.
    #[arg(long, requires = "branch")]
    allow_default_branch: bool,

    /// Allow pushing a view whose tip commit has an empty description
    ///
    /// The description a view publishes is the one the monorepo commit carries,
    /// so an undescribed change here is an undescribed commit in a repository
    /// other people read.
    #[arg(long)]
    allow_empty_description: bool,

    /// Derive every view and report what would be pushed, without pushing
    #[arg(long)]
    dry_run: bool,
}

/// What happened to one view.
enum Outcome {
    /// The branch was written on the remote.
    Pushed { url: Option<String> },
    /// The remote branch was already this commit.
    Current { url: Option<String> },
    /// The derived tip adds no content to the integrated published branch.
    NoChanges { published: gix::ObjectId },
    /// `git push` refused or failed. Carries what it said.
    Failed(String),
    /// An earlier view failed, so this one was never sent.
    NotAttempted,
}

#[instrument(skip_all)]
pub async fn cmd_views_push(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &ViewsPushArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let settings = workspace_command.settings();
    let configured = get_views_config(settings, workspace_command.workspace_root())?;
    let selected = select_views(&configured, &args.views)?;

    let commit = workspace_command
        .resolve_single_rev(ui, &args.revision)
        .await?;
    let branch = match &args.branch {
        Some(name) => name.clone(),
        None => generated_branch_name(ui, &workspace_command, &commit)?,
    };

    let git_backend = jj_lib::git::get_git_backend(workspace_command.repo().store())?;
    let git = Git {
        executable: jj_lib::git::GitSettings::from_settings(settings)?.executable_path,
        git_dir: git_backend.git_repo_path().to_owned(),
    };
    git.check_ref_format(&branch)?;

    // Everything that can be decided locally is decided before anything is
    // sent. Deriving is also the expensive half, so a repository that is not
    // going to be publishable fails without having written to any remote.
    for view in &selected {
        if branch == view.branch && !args.allow_default_branch {
            return Err(user_error(format!(
                "Refusing to push to {branch}, the default branch of the {} view at {}",
                view.name, view.remote
            ))
            .hinted(
                "Pass --allow-default-branch as well if that is really what you want, or drop \
                 --branch to push a new branch and open a pull request.",
            ));
        }
    }

    let mut repo = git_backend.git_repo();
    let source = gix::ObjectId::try_from(commit.id().as_bytes())
        .map_err(|err| user_error_with_message("Commit is not a Git object", err))?;
    let mut cache = Cache::new();
    validate_views(&git, &repo, &configured, &source, &mut cache)?;
    let mut derived = Vec::new();
    for view in &selected {
        let published_exists = git
            .remote_branch(&view.remote, &view.branch)
            .map_err(|err| user_error(format!("Could not read the {} view: {err}", view.name)))?
            .is_some();
        let (tip, published) = if published_exists {
            let surveyed = survey(
                &git,
                &mut repo,
                view,
                &source,
                false,
                Freshness::Fetch,
                &mut cache,
            )?;
            let tip = surveyed.derived.ok_or_else(|| no_history(view, args))?;
            let published = (surveyed.position == Position::Current).then_some(surveyed.upstream);
            (tip, published)
        } else {
            let filter = view.filter()?;
            let tip = jj_views::derive(&repo, &source, &filter, &mut cache)
                .map_err(|err| {
                    user_error_with_message(format!("Could not derive {}", view.name), err)
                })?
                .ok_or_else(|| no_history(view, args))?;
            (tip, None)
        };
        if !args.allow_empty_description && has_empty_description(&repo, tip)? {
            return Err(user_error(format!(
                "Won't push the {} view: its tip {} has no description",
                view.name,
                tip.to_hex_with_len(12)
            ))
            .hinted(
                "Describe the revision the view derives from, or pass --allow-empty-description.",
            ));
        }
        repo.reference(
            format!("{VIEW_REF_NAMESPACE}{}", view.name),
            tip,
            gix::refs::transaction::PreviousValue::Any,
            "jj views push",
        )
        .map_err(|err| {
            user_error_with_message(format!("Could not record the {} view", view.name), err)
        })?;
        derived.push((*view, tip, published));
    }

    if args.dry_run {
        // The `refs/jj/views/` refs above were written even here. They are
        // local and hold nothing but what a derive already computed, and
        // keeping them is what makes the real push that follows a dry run cheap.
        let mut out = ui.status();
        for (view, tip, published) in &derived {
            match published {
                Some(published) => writeln!(
                    out,
                    "{}: no content beyond {} at {published}; nothing to push.",
                    view.name, view.branch
                )?,
                None => writeln!(
                    out,
                    "{}: would push {tip} to {} as {branch}",
                    view.name, view.remote
                )?,
            }
        }
        writeln!(out, "Dry-run requested, not pushing.")?;
        return Ok(());
    }

    // A fixed order, and a stop at the first failure, so what landed is always
    // a prefix of that order rather than an arbitrary subset. See the report
    // below for what a partial push leaves behind.
    let mut outcomes = Vec::new();
    let mut failed = false;
    for (view, tip, published) in &derived {
        if failed {
            outcomes.push((*view, *tip, Outcome::NotAttempted));
            continue;
        }
        let outcome = match published {
            Some(published) => Outcome::NoChanges {
                published: *published,
            },
            None => push_one(&git, view, *tip, &branch),
        };
        failed = matches!(outcome, Outcome::Failed(_));
        outcomes.push((*view, *tip, outcome));
    }

    report(ui, &outcomes, &branch)?;
    if failed {
        Err(user_error("Not every view was pushed").hinted(
            "The views listed above as pushed are on their remotes; re-run to send the rest.",
        ))
    } else {
        if settings.get_bool("hints.views-push-host-repo")? {
            writeln!(
                ui.hint_default(),
                "Only the views were pushed. Nothing in this repository moved: its own bookmarks, \
                 tags and remotes are exactly as they were, and `jj git push` is what sends those."
            )?;
        }
        Ok(())
    }
}

fn no_history(view: &ViewConfig, args: &ViewsPushArgs) -> CommandError {
    user_error(format!(
        "Nothing under {} anywhere in the ancestry of {}, so the {} view has no history to push",
        view.path, args.revision, view.name
    ))
}

/// Renders the same template `jj git push --change` uses.
///
/// Sharing it is the point: a branch this command creates and a branch that one
/// creates for the same revision have the same name, so a user who knows one
/// convention knows both.
fn generated_branch_name(
    ui: &Ui,
    workspace_command: &WorkspaceCommandHelper,
    commit: &Commit,
) -> Result<String, CommandError> {
    let text = workspace_command
        .settings()
        .get_string("templates.git_push_bookmark")?;
    let template = workspace_command.parse_commit_template(ui, &text)?;
    let output = template.format_plain_text(commit);
    let name = String::from_utf8(output).map_err(|err| {
        user_error_with_message("Invalid character in branch name", err.utf8_error())
    })?;
    if name.is_empty() {
        return Err(user_error("Empty branch name generated"));
    }
    Ok(name)
}

/// Sends one view, without forcing except over what we just observed.
fn push_one(git: &Git, view: &ViewConfig, tip: gix::ObjectId, branch: &str) -> Outcome {
    // Computed for both outcomes, not just the one that moved the branch. A
    // re-push is the common case for a change under review, and dropping the
    // link there sent people to the forge to find their own branch by hand.
    let url = pull_request_url(&view.remote, &view.branch, branch);
    let observed = match git.remote_branch(&view.remote, branch) {
        Ok(observed) => observed,
        Err(err) => return Outcome::Failed(err),
    };
    if observed == Some(tip) {
        return Outcome::Current { url };
    }
    match git.push(&view.remote, tip, branch, observed) {
        Ok(()) => Outcome::Pushed { url },
        Err(err) => Outcome::Failed(err),
    }
}

/// Whether a derived commit says nothing about itself.
///
/// Only the tip is checked, not the whole derived history. Everything under it
/// is either already published, where refusing now is a refusal nobody can act
/// on, or was written before this repository adopted the check. The tip is the
/// commit this push adds and the one a reviewer opens.
fn has_empty_description(repo: &gix::Repository, tip: gix::ObjectId) -> Result<bool, CommandError> {
    let object = repo.find_object(tip).map_err(|err| {
        user_error_with_message(format!("Could not read the derived commit {tip}"), err)
    })?;
    let commit = gix::objs::CommitRef::from_bytes(&object.data, repo.object_hash())
        .map_err(|err| user_error_with_message(format!("Could not read the commit {tip}"), err))?;
    Ok(commit.message.trim_ascii().is_empty())
}

fn report(
    ui: &Ui,
    outcomes: &[(&ViewConfig, gix::ObjectId, Outcome)],
    branch: &str,
) -> Result<(), CommandError> {
    for (view, tip, outcome) in outcomes {
        match outcome {
            Outcome::Pushed { url } => {
                writeln!(
                    ui.status(),
                    "{}: pushed {tip} to {} as {branch}",
                    view.name,
                    view.remote
                )?;
                report_pull_request_url(ui, &view.name, url.as_deref())?;
            }
            Outcome::Current { url } => {
                writeln!(
                    ui.status(),
                    "{}: {} already has {branch} at {tip}",
                    view.name,
                    view.remote
                )?;
                report_pull_request_url(ui, &view.name, url.as_deref())?;
            }
            Outcome::NoChanges { published } => {
                writeln!(
                    ui.status(),
                    "{}: no content beyond {} at {published}; nothing pushed.",
                    view.name,
                    view.branch
                )?;
            }
            Outcome::Failed(message) => {
                writeln!(
                    ui.warning_default(),
                    "{}: could not push to {}",
                    view.name,
                    view.remote
                )?;
                for line in message.lines() {
                    writeln!(ui.status(), "  {line}")?;
                }
            }
            Outcome::NotAttempted => {
                writeln!(ui.status(), "{}: not attempted.", view.name)?;
            }
        }
    }
    Ok(())
}

fn report_pull_request_url(ui: &Ui, name: &str, url: Option<&str>) -> Result<(), CommandError> {
    if let Some(url) = url {
        writeln!(ui.status(), "{name}: open a pull request at {url}")?;
    }
    Ok(())
}

/// The URL that opens a pull request from `head` into `base`, when the remote
/// is one whose shape we know.
///
/// Only GitHub, and deliberately: a guessed URL for a forge this does not
/// recognize is worse than none, because it looks like it was checked.
fn pull_request_url(remote: &str, base: &str, head: &str) -> Option<String> {
    // `--allow-default-branch` pushes straight to the base. There is no pull
    // request to open from a branch to itself, and GitHub renders the compare
    // page for one as an empty diff.
    if base == head {
        return None;
    }
    let repo = github_repo(remote)?;
    Some(format!(
        "https://github.com/{repo}/compare/{base}...{head}?expand=1"
    ))
}

/// `owner/name` for the three spellings of a GitHub remote.
fn github_repo(remote: &str) -> Option<&str> {
    let path = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("https://github.com/"))
        .or_else(|| remote.strip_prefix("http://github.com/"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');
    // Exactly two components. Anything else is a URL shape this does not know,
    // and a compare link built from it would 404.
    (path.split('/').count() == 2 && !path.split('/').any(str::is_empty)).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_three_github_remote_spellings() {
        assert_eq!(
            github_repo("git@github.com:owner/repo.git"),
            Some("owner/repo")
        );
        assert_eq!(
            github_repo("ssh://git@github.com/owner/repo"),
            Some("owner/repo")
        );
        assert_eq!(
            github_repo("https://github.com/owner/repo.git"),
            Some("owner/repo")
        );
    }

    #[test]
    fn declines_to_guess_at_anything_else() {
        assert_eq!(github_repo("git@gitlab.com:owner/repo.git"), None);
        assert_eq!(github_repo("https://github.com/owner"), None);
        assert_eq!(github_repo("https://github.com/owner/repo/tree/main"), None);
        assert_eq!(github_repo("/srv/git/repo.git"), None);
    }

    #[test]
    fn offers_no_pull_request_from_the_base_to_itself() {
        assert_eq!(
            pull_request_url("git@github.com:indexable-inc/ix.git", "main", "main"),
            None
        );
    }

    #[test]
    fn builds_a_compare_url_against_the_views_own_default_branch() {
        assert_eq!(
            pull_request_url(
                "git@github.com:indexable-inc/ix.git",
                "main",
                "push-qpvuntsm"
            ),
            Some(
                "https://github.com/indexable-inc/ix/compare/main...push-qpvuntsm?expand=1"
                    .to_owned()
            )
        );
    }
}
