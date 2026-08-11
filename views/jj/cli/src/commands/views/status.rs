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
use jj_views::Cache;
use tracing::instrument;

use super::Freshness;
use super::Position;
use super::Survey;
use super::anchor;
use super::commits;
use super::elided_note;
use super::get_views_config;
use super::open_store;
use super::record;
use super::require_tracking_refs;
use super::select_views;
use super::survey;
use super::validate_endpoints;
use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::user_error_with_message;
use crate::complete;
use crate::ui::Ui;

/// Say where each configured view stands against its published repository
///
/// Nothing in this repository moves: no commit is lifted, no bookmark is
/// written, and a diverged view is reported rather than refused, so one run
/// answers for every view instead of stopping at the first problem. It does
/// fetch, since the question is about a repository somewhere else, and that
/// updates the same tracking ref `jj views fetch` keeps and nothing besides.
/// `--no-fetch` answers from the last fetch and touches no network.
///
/// The number worth knowing about is the elided one. A commit that arrived and
/// that the view then drops -- an upstream merge that changed nothing under the
/// prefix, most often -- stays inside `git rev-list <view tip>..<upstream>`
/// forever, so raw ancestry reports a view as behind when nothing is missing
/// and no amount of fetching will change it. "3 commits behind" and "up to
/// date, 3 commits elided" are different situations and this is what tells
/// them apart.
#[derive(clap::Args, Clone, Debug)]
pub struct ViewsStatusArgs {
    /// View to report on, by its key in the `views` config table (can be
    /// repeated)
    ///
    /// Defaults to every configured view.
    #[arg(value_name = "VIEW")]
    #[arg(add = ArgValueCandidates::new(complete::views))]
    views: Vec<String>,

    /// Compare one view with its read-only upstream endpoint
    #[arg(long, value_name = "VIEW", conflicts_with = "views")]
    #[arg(add = ArgValueCandidates::new(complete::views))]
    upstream: Option<String>,

    /// Bookmark the views are derived from
    #[arg(long, short = 'b', default_value = "main", value_name = "NAME")]
    #[arg(add = ArgValueCandidates::new(complete::local_bookmarks))]
    bookmark: String,

    /// Report against the last fetch instead of asking the published
    /// repositories where they are now
    #[arg(long)]
    no_fetch: bool,

    /// Emit one stable JSON object containing every selected view
    #[arg(long)]
    json: bool,
}

#[derive(serde::Serialize)]
struct StatusOutput {
    views: Vec<ViewStatus>,
}

#[derive(serde::Serialize)]
struct ViewStatus {
    name: String,
    path: String,
    remote: String,
    branch: String,
    anchor_source: Option<String>,
    anchor_view: Option<String>,
    anchor_tree_matches: Option<bool>,
    local_commit: Option<String>,
    published_commit: String,
    ahead: usize,
    behind: usize,
    elided: usize,
    state: ViewState,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ViewState {
    UpToDate,
    LocalAhead,
    FastForward,
    Diverged,
}

#[instrument(skip_all)]
pub async fn cmd_views_status(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &ViewsStatusArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui).await?;
    let configured = get_views_config(
        workspace_command.settings(),
        workspace_command.workspace_root(),
    )?;
    let selected_names = args
        .upstream
        .as_ref()
        .map(std::slice::from_ref)
        .unwrap_or(&args.views);
    let selected = select_views(&configured, selected_names)?;
    let (_, local) = anchor(&workspace_command, &args.bookmark)?;
    let (git, mut repo) = open_store(&workspace_command)?;
    let mut cache = Cache::new();
    validate_endpoints(&git, &configured)?;
    if args.no_fetch {
        require_tracking_refs(&repo, &selected, args.upstream.is_some())?;
    }

    let freshness = if args.no_fetch {
        Freshness::AsOfLastFetch
    } else {
        Freshness::Fetch
    };
    let mut surveys = Vec::with_capacity(selected.len());
    for view in &selected {
        let survey = survey(
            &git,
            &mut repo,
            view,
            &local,
            args.upstream.is_some(),
            freshness,
            &mut cache,
        )?;
        record::write(
            workspace_command.repo_path(),
            &args.bookmark,
            &local,
            &survey,
        )?;
        surveys.push(survey);
    }
    if args.json {
        let output = StatusOutput {
            views: surveys.iter().map(ViewStatus::from).collect(),
        };
        let json = serde_json::to_string(&output)
            .map_err(|err| user_error_with_message("Could not encode view status", err))?;
        writeln!(ui.stdout(), "{json}")?;
    } else {
        ui.request_pager();
        let mut out = ui.stdout();
        for survey in &surveys {
            writeln!(out, "{}: {}", survey.view.name, headline(survey))?;
            for line in detail(survey) {
                writeln!(out, "  {line}")?;
            }
        }
    }
    Ok(())
}

impl From<&Survey<'_>> for ViewStatus {
    fn from(survey: &Survey<'_>) -> Self {
        let (anchor_source, anchor_view, anchor_tree_matches) = match survey.view.anchor {
            Some(anchor) => (
                Some(anchor.source.to_string()),
                Some(anchor.view.to_string()),
                Some(true),
            ),
            None => (None, None, None),
        };
        Self {
            name: survey.view.name.clone(),
            path: survey.view.path.clone(),
            remote: survey.remote.to_owned(),
            branch: survey.branch.to_owned(),
            anchor_source,
            anchor_view,
            anchor_tree_matches,
            local_commit: survey.derived.map(|id| id.to_string()),
            published_commit: survey.upstream.to_string(),
            ahead: survey.ahead,
            behind: survey.incoming.len(),
            elided: survey.elided,
            state: match survey.position {
                Position::Current => ViewState::UpToDate,
                Position::LocalAhead => ViewState::LocalAhead,
                Position::FastForward => ViewState::FastForward,
                Position::Diverged => ViewState::Diverged,
            },
        }
    }
}

/// The one-line answer to "where is this view".
fn headline(survey: &Survey) -> String {
    let remote = survey.remote;
    match survey.position {
        Position::Current => "up to date.".to_owned(),
        Position::LocalAhead => format!(
            "{} ahead of {remote}; run `jj views push`.",
            commits(survey.ahead)
        ),
        Position::FastForward => format!(
            "{} behind {remote}; run `jj views fetch`.",
            commits(survey.incoming.len())
        ),
        Position::Diverged => format!(
            "diverged from {remote}; run `jj views fetch` to bring the published commits in \
             beside this history, then `jj new`."
        ),
    }
}

/// The lines under the headline, each one a number the headline cannot carry.
fn detail(survey: &Survey) -> Vec<String> {
    let mut lines = Vec::new();
    match survey.derived {
        Some(tip) => lines.push(format!("here: {tip} (from {})", survey.view.path)),
        // Nothing under the prefix in this bookmark's ancestry. Worth its own
        // line: every count below is then trivially zero for a reason that has
        // nothing to do with the published repository.
        None => lines.push(format!(
            "here: nothing under {} yet, so a fetch would import the whole history.",
            survey.view.path
        )),
    }
    lines.push(format!(
        "{}: {} ({})",
        survey.remote, survey.upstream, survey.branch
    ));
    if survey.ahead > 0 && survey.position != Position::LocalAhead {
        lines.push(format!("{} here not published yet.", commits(survey.ahead)));
    }
    if survey.elided > 0 {
        lines.push(elided_note(survey.elided));
    }
    lines
}
