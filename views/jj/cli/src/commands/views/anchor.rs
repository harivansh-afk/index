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

use std::fmt;
use std::io::Write as _;

use clap_complete::ArgValueCandidates;
use jj_views::Cache;
use tracing::instrument;

use super::ANCHOR_REF_NAMESPACE;
use super::ANCHOR_REVISION_REF_NAMESPACE;
use super::ANCHOR_SOURCE_REF_NAMESPACE;
use super::ShallowFetch;
use super::anchor;
use super::commits;
use super::get_views_config;
use super::lift_error;
use super::open_store;
use super::select_views;
use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::config_error;
use crate::command_error::user_error;
use crate::command_error::user_error_with_message;
use crate::complete;
use crate::ui::Ui;

/// Fetch and validate one manifest anchor without its older history
#[derive(clap::Args, Clone, Debug)]
pub struct ViewsAnchorArgs {
    /// View whose manifest anchor should be installed (can be repeated)
    ///
    /// Defaults to every configured view.
    #[arg(value_name = "VIEW")]
    #[arg(add = ArgValueCandidates::new(complete::views))]
    views: Vec<String>,

    /// Bookmark whose history must contain the source anchor
    #[arg(long, short = 'b', default_value = "main", value_name = "NAME")]
    #[arg(add = ArgValueCandidates::new(complete::local_bookmarks))]
    bookmark: String,

    /// Emit one stable JSON object containing every selected anchor
    #[arg(long)]
    json: bool,
}

#[derive(serde::Serialize)]
struct AnchorOutput {
    views: Vec<AnchorStatus>,
}

#[derive(serde::Serialize)]
struct AnchorStatus {
    name: String,
    source: String,
    view: String,
    fetched_commits: usize,
    tree_matches: bool,
    endpoint: AnchorEndpointKind,
    attempts: Vec<AnchorEndpointAttempt>,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum AnchorEndpointKind {
    Local,
    Upstream,
    Published,
}

impl fmt::Display for AnchorEndpointKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => formatter.write_str("local object store"),
            Self::Upstream => formatter.write_str("read-only upstream"),
            Self::Published => formatter.write_str("published branch"),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
struct AnchorEndpointAttempt {
    endpoint: AnchorEndpointKind,
    remote: String,
    selector: String,
    error: String,
}

#[derive(Debug)]
struct AnchorFetchError {
    anchor: gix::ObjectId,
    attempts: Vec<AnchorEndpointAttempt>,
}

impl fmt::Display for AnchorFetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "anchor {} was not available", self.anchor)?;
        for attempt in &self.attempts {
            write!(
                formatter,
                "; {} {} at {} failed: {}",
                attempt.endpoint, attempt.selector, attempt.remote, attempt.error
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for AnchorFetchError {}

#[derive(Debug, thiserror::Error)]
enum PublishedHistoryError {
    #[error("host commit {host_commit} produced no published view commit")]
    ElidedHostCommit { host_commit: gix::ObjectId },
    #[error("published tip is {actual_tip}; derived host tip is {expected_tip}")]
    Mismatch {
        expected_tip: gix::ObjectId,
        actual_tip: gix::ObjectId,
    },
}

struct AnchorEndpoint<'a> {
    kind: AnchorEndpointKind,
    remote: &'a str,
    selector: String,
    depth: usize,
}

#[instrument(skip_all)]
pub async fn cmd_views_anchor(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &ViewsAnchorArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let configured = get_views_config(
        workspace_command.settings(),
        workspace_command.workspace_root(),
    )?;
    let selected = select_views(&configured, &args.views)?;
    let (_, revision) = anchor(&workspace_command, &args.bookmark)?;
    let (git, repo) = open_store(&workspace_command)?;
    let reachable = git.reachable_commits(&revision).map_err(|err| {
        user_error(format!(
            "Could not read commits reachable from {revision}: {err}"
        ))
    })?;
    let mut statuses = Vec::with_capacity(selected.len());
    let mut cache = Cache::new();
    for view in selected {
        let manifest_anchor = view.anchor.ok_or_else(|| {
            config_error(format!(
                "The {} view has no anchor in .jj-views.toml",
                view.name
            ))
        })?;
        if !reachable.contains(&manifest_anchor.source) {
            return Err(user_error(format!(
                "The {} view anchor source {} is not an ancestor of {revision}",
                view.name, manifest_anchor.source
            )));
        }
        let filter = view.filter()?;
        if view.root_anchor {
            cache
                .create_root_anchor_after_ancestry_check(&repo, &filter, manifest_anchor)
                .map_err(|err| lift_error(view, err))?;
            write_anchor_refs(&repo, &view.name, manifest_anchor, revision)?;
            statuses.push(AnchorStatus {
                name: view.name.clone(),
                source: manifest_anchor.source.to_string(),
                view: manifest_anchor.view.to_string(),
                fetched_commits: 0,
                tree_matches: true,
                endpoint: AnchorEndpointKind::Local,
                attempts: Vec::new(),
            });
            continue;
        }
        let anchor_is_present = match repo.find_object(manifest_anchor.view) {
            Ok(_) => true,
            Err(gix::object::find::existing::Error::NotFound { .. }) => false,
            Err(err) => {
                return Err(user_error_with_message(
                    format!("Could not read the {} view anchor", view.name),
                    err,
                ));
            }
        };
        let (fetched_commits, endpoint, attempts) = if anchor_is_present {
            cache
                .seed_anchor_after_ancestry_check(&repo, &filter, manifest_anchor)
                .map_err(|err| lift_error(view, err))?;
            write_anchor_refs(&repo, &view.name, manifest_anchor, revision)?;
            (0, AnchorEndpointKind::Local, Vec::new())
        } else {
            let host_commit_count = git
                .history_count_after(&revision, &manifest_anchor.source)
                .map_err(|err| {
                    user_error(format!(
                        "Could not count host commits for the {} view: {err}",
                        view.name
                    ))
                })?;
            let mut endpoints = Vec::with_capacity(2);
            endpoints.push(AnchorEndpoint {
                kind: AnchorEndpointKind::Published,
                remote: &view.remote,
                selector: format!("refs/heads/{}", view.branch),
                depth: host_commit_count.checked_add(1).ok_or_else(|| {
                    user_error(format!("The {} view's fetch depth overflowed", view.name))
                })?,
            });
            if let Some(upstream) = &view.upstream {
                endpoints.push(AnchorEndpoint {
                    kind: AnchorEndpointKind::Upstream,
                    remote: &upstream.remote,
                    selector: manifest_anchor.view.to_string(),
                    depth: 1,
                });
            }

            let mut attempts = Vec::new();
            let mut fetched = None;
            for endpoint in endpoints {
                match git.fetch_shallow(
                    endpoint.remote,
                    &endpoint.selector,
                    endpoint.depth,
                    manifest_anchor.view,
                ) {
                    Ok(prepared) => {
                        fetched = Some((endpoint.kind, prepared));
                        break;
                    }
                    Err(error) => attempts.push(AnchorEndpointAttempt {
                        endpoint: endpoint.kind,
                        remote: endpoint.remote.to_owned(),
                        selector: endpoint.selector,
                        error,
                    }),
                }
            }
            let Some((endpoint, prepared)) = fetched else {
                return Err(user_error_with_message(
                    format!(
                        "Could not fetch the {} view anchor {}",
                        view.name, manifest_anchor.view
                    ),
                    AnchorFetchError {
                        anchor: manifest_anchor.view,
                        attempts,
                    },
                ));
            };
            cache
                .validate_fetched_anchor_after_ancestry_check(
                    &repo,
                    &filter,
                    manifest_anchor,
                    &prepared.anchor_commit,
                )
                .map_err(|err| lift_error(view, err))?;
            if matches!(endpoint, AnchorEndpointKind::Published) {
                validate_published_history(view, &repo, &filter, &revision, &prepared, &mut cache)?;
            }
            git.install_shallow_anchor(&prepared).map_err(|err| {
                user_error(format!(
                    "Could not install the {} view's shallow anchor: {err}",
                    view.name
                ))
            })?;
            write_anchor_refs(&repo, &view.name, manifest_anchor, revision)?;
            (1, endpoint, attempts)
        };
        statuses.push(AnchorStatus {
            name: view.name.clone(),
            source: manifest_anchor.source.to_string(),
            view: manifest_anchor.view.to_string(),
            fetched_commits,
            tree_matches: true,
            endpoint,
            attempts,
        });
    }
    if args.json {
        let json = serde_json::to_string(&AnchorOutput { views: statuses })
            .map_err(|err| user_error_with_message("Could not encode view anchors", err))?;
        writeln!(ui.stdout(), "{json}")?;
    } else {
        let mut out = ui.status();
        for status in statuses {
            for attempt in &status.attempts {
                writeln!(
                    out,
                    "{}: {} {} at {} failed: {}",
                    status.name, attempt.endpoint, attempt.selector, attempt.remote, attempt.error
                )?;
            }
            writeln!(
                out,
                "{}: anchor {} -> {} is valid from {}; fetched {} and its tree matches.",
                status.name,
                status.source,
                status.view,
                status.endpoint,
                commits(status.fetched_commits)
            )?;
        }
    }
    Ok(())
}

fn validate_published_history(
    view: &super::ViewConfig,
    repo: &gix::Repository,
    filter: &jj_views::Filter,
    revision: &gix::ObjectId,
    prepared: &ShallowFetch,
    cache: &mut Cache,
) -> Result<(), CommandError> {
    let expected_tip = jj_views::derive_tip(repo, revision, filter, cache)
        .map_err(|err| lift_error(view, err))?
        .ok_or_else(|| {
            user_error_with_message(
                format!(
                    "Could not verify the {} published anchor history",
                    view.name
                ),
                PublishedHistoryError::ElidedHostCommit {
                    host_commit: *revision,
                },
            )
        })?;
    // A Git commit id hashes its parent ids recursively, so equal tips prove
    // the full filtered parent graph without requiring that graph to be linear.
    if expected_tip != prepared.tip {
        return Err(user_error_with_message(
            format!(
                "Could not verify the {} published anchor history",
                view.name
            ),
            PublishedHistoryError::Mismatch {
                expected_tip,
                actual_tip: prepared.tip,
            },
        ));
    }
    Ok(())
}

fn write_anchor_refs(
    repo: &gix::Repository,
    name: &str,
    anchor: jj_views::DeriveAnchor,
    revision: gix::ObjectId,
) -> Result<(), CommandError> {
    for (namespace, commit) in [
        (ANCHOR_REF_NAMESPACE, anchor.view),
        (ANCHOR_SOURCE_REF_NAMESPACE, anchor.source),
        (ANCHOR_REVISION_REF_NAMESPACE, revision),
    ] {
        repo.reference(
            format!("{namespace}{name}"),
            commit,
            gix::refs::transaction::PreviousValue::Any,
            "jj views anchor",
        )
        .map_err(|err| {
            user_error_with_message(format!("Could not record the {name} anchor"), err)
        })?;
    }
    Ok(())
}
