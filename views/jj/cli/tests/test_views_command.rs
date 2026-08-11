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

//! `jj views push`, against a published repository that is really separate.
//!
//! The published repository here is seeded with the standalone history and
//! never sees a monorepo object, so the branch this command sends can only be
//! an ancestor-extension of it if the derived hashes are the standalone ones.
//! Git checks that for us on the receiving end; a hash that moved would arrive
//! as an unrelated history.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;

use gix::ObjectId;
use jj_views::Cache;
use jj_views::DeriveAnchor;
use jj_views::Elide;
use jj_views::Filter;
use jj_views::Semantics;
use jj_views::fixture;

use crate::common::TestEnvironment;

const PREFIX: &str = "vendor/upstream";

/// A monorepo carrying an imported standalone history, and the repository that
/// history is published to.
struct World {
    test_env: TestEnvironment,
    /// The fixture history, as it exists in the standalone repository.
    upstream: fixture::Upstream,
}

impl World {
    /// Builds the monorepo, injects the fixture under [`PREFIX`], and seeds a
    /// separate published repository with the fixture history alone.
    fn new() -> Self {
        let test_env = TestEnvironment::default();
        test_env
            .run_jj_in(".", ["git", "init", "monorepo"])
            .success();

        let store = gix::open(Self::store_path_in(test_env.env_root())).expect("the jj git store");
        let upstream = fixture::write_upstream(&store).expect("the fixture history");

        // Inject the fixture under the prefix, which is what `jj-views import`
        // does: each commit lifted onto its parent's counterpart, roots onto an
        // empty base. The order comes from the ancestry rather than from
        // `Upstream::commits`, for the same reason the command takes it from
        // there: it is every reachable commit, parents first, and a lift onto a
        // parent that has not been lifted yet lands in the wrong place.
        let filter = filter();
        let mut cache = Cache::new();
        let base = empty_base(&store);
        let mut injected = std::collections::HashMap::new();
        for source in &jj_views::verify::ancestry(&store, &upstream.head).expect("the ancestry") {
            let raw = store
                .find_object(*source)
                .expect("the commit")
                .detach()
                .data;
            let first = gix::objs::CommitRef::from_bytes(&raw, store.object_hash())
                .expect("a commit")
                .parents()
                .next();
            let onto = first
                .and_then(|parent| injected.get(&parent).copied())
                .unwrap_or(base);
            let id = jj_views::unfilter(&store, source, &onto, &filter, &mut cache)
                .expect("the commit lifts");
            injected.insert(*source, id);
        }
        let tip = injected[&upstream.head];
        store
            .reference(
                "refs/heads/vendored",
                tip,
                gix::refs::transaction::PreviousValue::Any,
                "the test fixture",
            )
            .expect("a writable ref");

        // The published repository holds the standalone history and nothing
        // else. Pushing from the store is how the objects get there, but only
        // the ones the fixture tip reaches.
        let published = test_env.env_root().join("published.git");
        gix::init_bare(&published).expect("a bare repository");
        git(
            test_env.env_root(),
            &[
                "--git-dir",
                &Self::store_path_in(test_env.env_root()).to_string_lossy(),
                "push",
                published.to_str().expect("a utf8 path"),
                &format!("{}:refs/heads/main", upstream.head),
            ],
        );

        test_env.add_config(format!(
            "[views.upstream]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \"main\"\n",
            published.to_str().expect("a utf8 path"),
        ));

        // Teach jj about the injected lineage, then put a real jj commit on top
        // of it that changes something under the prefix.
        test_env.run_jj_in("monorepo", ["git", "import"]).success();
        let work_dir = test_env.work_dir("monorepo");
        work_dir.run_jj(["new", "vendored"]).success();
        work_dir.write_file(format!("{PREFIX}/README"), b"changed by the monorepo\n");
        work_dir
            .run_jj(["describe", "-m", "a change made in the monorepo"])
            .success();

        Self { test_env, upstream }
    }

    fn store_path_in(env_root: &Path) -> std::path::PathBuf {
        env_root
            .join("monorepo")
            .join(".jj")
            .join("repo")
            .join("store")
            .join("git")
    }

    fn store(&self) -> gix::Repository {
        gix::open(Self::store_path_in(self.test_env.env_root())).expect("the jj git store")
    }

    fn pack_count(&self) -> usize {
        std::fs::read_dir(Self::store_path_in(self.test_env.env_root()).join("objects/pack"))
            .expect("a pack directory")
            .map(|entry| entry.expect("a readable pack entry"))
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "idx")
            })
            .count()
    }

    fn seed_pack_indexes(&self, count: usize) {
        let store = Self::store_path_in(self.test_env.env_root());
        let pack_dir = store.join("objects/pack");
        for index in 0..count {
            let seed = self.test_env.env_root().join(format!("pack-seed-{index}"));
            std::fs::write(&seed, format!("pack seed {index}\n")).expect("a writable pack seed");
            let object = git(
                self.test_env.env_root(),
                &[
                    "--git-dir",
                    &store.to_string_lossy(),
                    "hash-object",
                    "-w",
                    seed.to_str().expect("a utf8 pack seed path"),
                ],
            );
            git(
                self.test_env.env_root(),
                &[
                    "--git-dir",
                    &store.to_string_lossy(),
                    "update-ref",
                    &format!("refs/jj/pack-seed/{index}"),
                    object.trim(),
                ],
            );
            let prefix = pack_dir.join(format!("seed-{index}"));
            let mut child = Command::new("git")
                .args([
                    "--git-dir",
                    &store.to_string_lossy(),
                    "pack-objects",
                    prefix.to_str().expect("a utf8 pack prefix"),
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .expect("git pack-objects runs");
            child
                .stdin
                .take()
                .expect("pack-objects has stdin")
                .write_all(object.as_bytes())
                .expect("the object id is written");
            let status = child.wait().expect("git pack-objects finishes");
            assert!(status.success(), "git pack-objects failed with {status}");
        }
    }

    /// Puts a bookmark on the injected lineage, which is what `jj views fetch`
    /// adds lifted commits to.
    fn bookmark_main(&self) {
        self.test_env
            .run_jj_in("monorepo", ["bookmark", "create", "main", "-r", "vendored"])
            .success();
    }

    /// Moves `main` to a merge which adds topology and no view content.
    fn bookmark_topology_only_merge(&self) -> ObjectId {
        let first = self.git_id("vendored");
        let store = Self::store_path_in(self.test_env.env_root());
        let tree = git(
            self.test_env.env_root(),
            &[
                "--git-dir",
                &store.to_string_lossy(),
                "rev-parse",
                &format!("{first}^{{tree}}"),
            ],
        );
        let parent = git(
            self.test_env.env_root(),
            &[
                "--git-dir",
                &store.to_string_lossy(),
                "rev-parse",
                &format!("{first}^"),
            ],
        );
        let side = git(
            self.test_env.env_root(),
            &[
                "--git-dir",
                &store.to_string_lossy(),
                "-c",
                "user.name=Views",
                "-c",
                "user.email=views@invalid",
                "commit-tree",
                tree.trim(),
                "-p",
                parent.trim(),
                "-m",
                "the same view content on a side branch",
            ],
        );
        let merge = git(
            self.test_env.env_root(),
            &[
                "--git-dir",
                &store.to_string_lossy(),
                "-c",
                "user.name=Views",
                "-c",
                "user.email=views@invalid",
                "commit-tree",
                tree.trim(),
                "-p",
                &first.to_string(),
                "-p",
                side.trim(),
                "-m",
                "a topology-only merge",
            ],
        );
        git(
            self.test_env.env_root(),
            &[
                "--git-dir",
                &store.to_string_lossy(),
                "update-ref",
                "refs/heads/topology-only",
                merge.trim(),
            ],
        );
        self.test_env
            .run_jj_in("monorepo", ["git", "import"])
            .success();
        self.test_env
            .work_dir("monorepo")
            .run_jj(["bookmark", "set", "main", "-r", "topology-only"])
            .success();
        self.git_id("main")
    }

    /// Respells the view's remote as a GitHub URL, and points git at the bare
    /// repository next door for the transfer.
    ///
    /// The pull request URL only exists for a remote whose shape the command
    /// recognizes, and every other test here uses a filesystem path, where
    /// there is no forge to name. A test of the URL against a path remote would
    /// find no URL in either run and pass whether or not the URL is there,
    /// so the remote has to really be spelled as GitHub. `insteadOf` is what
    /// keeps the push itself local.
    fn publish_as_github(&self, url: &str) {
        let published = self.test_env.env_root().join("published.git");
        let store = Self::store_path_in(self.test_env.env_root());
        git(
            self.test_env.env_root(),
            &[
                "--git-dir",
                &store.to_string_lossy(),
                "config",
                &format!("url.{}.insteadOf", published.to_str().expect("a utf8 path")),
                url,
            ],
        );
        self.test_env
            .add_config(format!("[views.upstream]\nremote = {url:?}\n"));
    }

    /// Adds one commit to the published repository, as somebody else would.
    fn publish_upstream_commit(&self, file: &str, message: &str) {
        let work = self.test_env.env_root().join("upstream-work");
        let published = self.test_env.env_root().join("published.git");
        if !work.exists() {
            // `--branch main` matters: the bare repository was created with a
            // HEAD that names a branch it does not have, so a plain clone
            // leaves an unborn HEAD and the first commit is not a descendant
            // of main.
            git(
                self.test_env.env_root(),
                &[
                    "clone",
                    "-q",
                    "--branch",
                    "main",
                    published.to_str().unwrap(),
                    "upstream-work",
                ],
            );
            git(&work, &["config", "user.email", "up@example.invalid"]);
            git(&work, &["config", "user.name", "Up"]);
        }
        std::fs::write(work.join(file), b"published elsewhere\n").expect("a writable file");
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", message]);
        git(&work, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    }

    /// Publishes a merge of a branch whose change had already landed on the
    /// branch it is merged into.
    ///
    /// The result is the shape ENG-11873 was found in: a merge whose tree
    /// equals BOTH its parents', which the view's elision rule drops because it
    /// changed nothing under the prefix while its two counterparts here differ
    /// outside it. A dropped commit is invisible to `derive`, so before the fix
    /// a fetch lifted it, could not see it afterwards, and lifted it again on
    /// every run after that.
    ///
    /// Two details are load-bearing and neither is obvious.
    ///
    /// The branch forks from `main~2` rather than from `main~1`. A branch
    /// forking from the commit this repository's view tip already derives to
    /// gets lifted onto the monorepo tip by the lifting rule that exists to
    /// make a round trip add nothing, so both counterparts would then share the
    /// monorepo's content outside the prefix and the merge would not be
    /// dropped.
    ///
    /// And the commits are built with `commit-tree` rather than by checking out
    /// and committing, because the tree has to be exactly `main`'s. A porcelain
    /// route reaches the same tree only if the fixture's tip adds files and
    /// deletes none, which is a property of the fixture and not something this
    /// should depend on.
    ///
    /// Requires an earlier [`Self::publish_upstream_commit`], so that `main~2`
    /// is a commit this repository has already integrated.
    fn publish_already_landed_merge(&self) {
        let work = self.test_env.env_root().join("upstream-work");
        assert!(
            work.exists(),
            "publish_upstream_commit has to have run, so `main~2` is integrated here"
        );
        let tree = git(&work, &["rev-parse", "main^{tree}"]).trim().to_owned();
        let fork = git(&work, &["rev-parse", "main~2"]).trim().to_owned();
        let landed = git(
            &work,
            &[
                "commit-tree",
                &tree,
                "-p",
                &fork,
                "-m",
                "the same change, arrived at on a branch",
            ],
        )
        .trim()
        .to_owned();
        let merge = git(
            &work,
            &[
                "commit-tree",
                &tree,
                "-p",
                "main",
                "-p",
                &landed,
                "-m",
                "merge a branch that had already landed",
            ],
        )
        .trim()
        .to_owned();
        git(
            &work,
            &["push", "-q", "origin", &format!("{merge}:refs/heads/main")],
        );
    }

    /// Puts a commit that touches nothing under the prefix on `main`.
    ///
    /// Not decoration. The elision rule drops a commit that changed nothing
    /// under the prefix and something outside it, so a monorepo whose history
    /// is only the injected lineage -- which has nothing outside the prefix at
    /// all -- cannot produce an elided lift.
    fn commit_outside_the_prefix(&self) {
        let work_dir = self.test_env.work_dir("monorepo");
        work_dir.run_jj(["new", "main"]).success();
        work_dir.write_file("OUTSIDE.md", b"nothing to do with the view\n");
        work_dir
            .run_jj(["describe", "-m", "a monorepo change outside the prefix"])
            .success();
        work_dir
            .run_jj(["bookmark", "set", "main", "-r", "@"])
            .success();
        // Off the bookmark again, so the working copy is where a person's is:
        // somewhere else while the views move.
        work_dir.run_jj(["new", "main"]).success();
    }

    /// What the published repository's `main` points at.
    fn published_tip(&self) -> ObjectId {
        self.published()
            .find_reference("refs/heads/main")
            .expect("the published branch")
            .peel_to_id()
            .expect("a resolvable branch")
            .detach()
    }

    fn published(&self) -> gix::Repository {
        gix::open(self.test_env.env_root().join("published.git")).expect("the published repository")
    }

    fn configure_packed_views(&self, count: usize) {
        let publisher = self.test_env.env_root().join("packed-view-publisher");
        let published = self.test_env.env_root().join("published.git");
        git(
            self.test_env.env_root(),
            &[
                "clone",
                "-q",
                "--branch",
                "main",
                published.to_str().expect("a utf8 path"),
                publisher
                    .file_name()
                    .expect("a publisher directory name")
                    .to_str()
                    .expect("a utf8 publisher directory name"),
            ],
        );
        git(
            &publisher,
            &["config", "user.email", "views@example.invalid"],
        );
        git(&publisher, &["config", "user.name", "Views"]);
        let anchor_source = self.git_id("vendored");
        let anchor_view = self.upstream.head;
        let mut manifest = String::new();
        for index in 0..count {
            git(
                &publisher,
                &["switch", "-q", "--detach", &anchor_view.to_string()],
            );
            std::fs::write(publisher.join("PACK"), format!("view {index}\n").as_bytes())
                .expect("a writable publisher");
            git(&publisher, &["add", "PACK"]);
            git(&publisher, &["commit", "-q", "-m", "one packed view"]);

            let remote = self
                .test_env
                .env_root()
                .join(format!("packed-view-{index}.git"));
            git(
                self.test_env.env_root(),
                &[
                    "init",
                    "-q",
                    "--bare",
                    remote.to_str().expect("a utf8 path"),
                ],
            );
            git(
                &publisher,
                &[
                    "push",
                    "-q",
                    remote.to_str().expect("a utf8 path"),
                    "HEAD:refs/heads/main",
                ],
            );
            write!(
                manifest,
                "[views.packed-{index:02}]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \
                 \"main\"\n[views.packed-{index:02}.anchor]\nsource = \"{anchor_source}\"\nview = \
                 \"{anchor_view}\"\n",
                remote.to_str().expect("a utf8 path"),
            )
            .expect("writing to a string cannot fail");
        }
        self.test_env
            .work_dir("monorepo")
            .write_file(".jj-views.toml", manifest.as_bytes());
        git(
            self.test_env.env_root(),
            &[
                "--git-dir",
                &Self::store_path_in(self.test_env.env_root()).to_string_lossy(),
                "config",
                "fetch.unpackLimit",
                "1",
            ],
        );
        git(
            self.test_env.env_root(),
            &[
                "--git-dir",
                &Self::store_path_in(self.test_env.env_root()).to_string_lossy(),
                "config",
                "gc.auto",
                "0",
            ],
        );
    }

    /// The git commit behind a jj revision.
    fn git_id(&self, rev: &str) -> ObjectId {
        let output = self
            .test_env
            .run_jj_in(
                "monorepo",
                ["log", "--no-graph", "-r", rev, "-T", "commit_id"],
            )
            .success();
        ObjectId::from_hex(output.stdout.raw().trim().as_bytes()).expect("a commit id")
    }
}

/// The paths `jj file list` prints, with separators as repo paths spell them.
///
/// The command renders NATIVE separators, so on Windows every path comes back
/// as `vendor\upstream\NAME` while `PREFIX` is a repo path and always uses
/// slashes. An assertion built from `PREFIX` therefore matches on unix and not
/// on Windows -- which is exactly what happened: three of these tests were red
/// on both Windows legs while the fetch itself was working, and the file being
/// looked for was in the listing every time. `normalize_backslash` is jj's own
/// answer to this and every one of these assertions goes through it.
fn file_list(world: &World, args: &[&str]) -> String {
    world
        .test_env
        .run_jj_in("monorepo", args)
        .success()
        .stdout
        .normalize_backslash()
        .normalized()
        .to_owned()
}

fn assert_anchor_object_closure(repo: &gix::Repository, anchor: ObjectId) {
    let raw = repo
        .find_object(anchor)
        .expect("the anchor commit")
        .detach()
        .data;
    let commit =
        gix::objs::CommitRef::from_bytes(&raw, repo.object_hash()).expect("a valid anchor commit");
    let mut trees = vec![commit.tree()];
    let mut seen = std::collections::HashSet::new();
    while let Some(tree) = trees.pop() {
        if !seen.insert(tree) {
            continue;
        }
        let raw = repo
            .find_object(tree)
            .expect("every anchor tree object")
            .detach()
            .data;
        let tree =
            gix::objs::TreeRef::from_bytes(&raw, repo.object_hash()).expect("a valid anchor tree");
        for entry in tree.entries {
            match entry.mode.kind() {
                gix::objs::tree::EntryKind::Tree => trees.push(entry.oid.to_owned()),
                gix::objs::tree::EntryKind::Commit => {}
                _ => {
                    repo.find_object(entry.oid)
                        .expect("every blob referenced by the anchor tree");
                }
            }
        }
    }
}

fn filter() -> Filter {
    Filter::prefix(PREFIX)
        .expect("a usable prefix")
        .semantics(Semantics::V2)
        .elide(Elide::Unchanged)
}

fn empty_base(repo: &gix::Repository) -> ObjectId {
    let tree = repo
        .write_object(gix::objs::Tree::default())
        .expect("an empty tree")
        .detach();
    let raw = format!(
        "tree {tree}\nauthor views <views@invalid> 0 +0000\ncommitter views <views@invalid> 0 \
         +0000\n\nempty base\n"
    );
    gix::objs::Write::write_buf(&repo.objects, gix::objs::Kind::Commit, raw.as_bytes())
        .expect("a writable object database")
}

/// Runs git and requires it to succeed, reporting both streams when it does
/// not.
fn git(current_dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(current_dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "`git {}` failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn test_views_push_publishes_the_standalone_hashes() {
    let world = World::new();
    let source = world.git_id("@");

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "push"])
        .success();
    let report = output.stderr.raw();
    let branch = report
        .split(" as ")
        .nth(1)
        .expect("the report names the branch it pushed")
        .lines()
        .next()
        .expect("a branch name")
        .to_owned();
    assert!(
        branch.starts_with("push-"),
        "unexpected branch name {branch:?}"
    );

    // What the published repository now has, read from the published
    // repository, not from what we believe we sent.
    let published = world.published();
    let pushed = published
        .find_reference(&format!("refs/heads/{branch}"))
        .expect("the branch the command reported")
        .peel_to_id()
        .expect("a commit")
        .detach();

    // The comparison itself is `jj-views verify`, run against the tip that came
    // back over the wire. Its passing state is an absence, so the count of
    // commits it actually compared is asserted too: a verify that checked
    // nothing would otherwise look identical to one that checked everything.
    let mut cache = Cache::new();
    let report = jj_views::verify::verify(&world.store(), &source, &pushed, &filter(), &mut cache)
        .expect("the view derives");
    let standalone = jj_views::verify::ancestry(&world.store(), &world.upstream.head)
        .expect("the standalone ancestry")
        .len();
    assert!(standalone > 1, "the fixture should have a real history");
    assert!(
        report.tip_matches(),
        "derived tip is not what was published: {report:?}"
    );
    assert!(report.identical(), "published history differs: {report:?}");
    assert_eq!(report.expected, standalone + 1);

    // And the same claim as the published repository sees it: the branch
    // extends the standalone history it already had, by exactly the one commit
    // the monorepo added. Only identical hashes can produce that.
    let published_dir = world.test_env.env_root().join("published.git");
    git(
        &published_dir,
        &[
            "merge-base",
            "--is-ancestor",
            &world.upstream.head.to_string(),
            &branch,
        ],
    );
    let added = git(
        &published_dir,
        &["rev-list", "--count", &format!("main..{branch}")],
    );
    assert_eq!(added.trim(), "1");
}

#[test]
fn test_views_push_refuses_a_default_branch_without_being_asked_twice() {
    let world = World::new();

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "push", "--branch", "main"]);
    insta::assert_snapshot!(output, @r"
    ------- stderr -------
    Error: Refusing to push to main, the default branch of the upstream view at $TEST_ENV/published.git
    Hint: Pass --allow-default-branch as well if that is really what you want, or drop --branch to push a new branch and open a pull request.
    [EOF]
    [exit status: 1]
    ");

    // Refused before anything was sent: the published repository still has only
    // the branch it was seeded with.
    let published = world.published();
    assert_eq!(
        published
            .find_reference("refs/heads/main")
            .expect("the seeded branch")
            .peel_to_id()
            .expect("a commit")
            .detach(),
        world.upstream.head
    );
}

#[test]
fn test_views_push_to_the_default_branch_when_asked_twice() {
    let world = World::new();
    let source = world.git_id("@");

    world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "views",
                "push",
                "--branch",
                "main",
                "--allow-default-branch",
            ],
        )
        .success();

    let mut cache = Cache::new();
    let published = world.published();
    let main = published
        .find_reference("refs/heads/main")
        .expect("the branch")
        .peel_to_id()
        .expect("a commit")
        .detach();
    let report = jj_views::verify::verify(&world.store(), &source, &main, &filter(), &mut cache)
        .expect("the view derives");
    assert!(report.identical(), "published history differs: {report:?}");
}

#[test]
fn test_views_push_names_an_unknown_view() {
    let world = World::new();

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "push", "downstream"]);
    insta::assert_snapshot!(output, @r"
    ------- stderr -------
    Error: No such view: downstream
    Hint: Configured views are: upstream
    [EOF]
    [exit status: 1]
    ");
}

#[test]
fn test_views_push_dry_run_sends_nothing() {
    let world = World::new();

    world
        .test_env
        .run_jj_in("monorepo", ["views", "push", "--dry-run"])
        .success();

    let published = world.published();
    let branches: Vec<String> = published
        .references()
        .expect("the reference store")
        .prefixed("refs/heads/")
        .expect("a prefix iterator")
        .filter_map(Result::ok)
        .map(|reference| reference.name().as_bstr().to_string())
        .collect();
    assert_eq!(branches, ["refs/heads/main"]);
}

/// ENG-11940: a push that finds the branch already there still has to say
/// where the pull request is.
///
/// Re-pushing a branch under review is the common case, not the rare one, and
/// a report that drops the link on exactly that run sends people back to the
/// forge to find their own branch by hand.
#[test]
fn test_views_push_repeats_the_pull_request_url_for_a_branch_already_there() {
    let world = World::new();
    world.publish_as_github("https://github.com/indexable-inc/upstream.git");
    let compare =
        "open a pull request at https://github.com/indexable-inc/upstream/compare/main...push-";

    let first = world
        .test_env
        .run_jj_in("monorepo", ["views", "push"])
        .success()
        .stderr
        .raw()
        .to_owned();
    assert!(
        first.contains(compare),
        "the first push did not name the pull request: {first}"
    );

    let second = world
        .test_env
        .run_jj_in("monorepo", ["views", "push"])
        .success()
        .stderr
        .raw()
        .to_owned();
    assert!(
        second.contains("already has push-"),
        "the second push did not report the branch as already there: {second}"
    );
    assert!(
        second.contains(compare),
        "the second push dropped the pull request URL: {second}"
    );
}

/// Pushing to the view's own default branch opens no pull request, so there is
/// no URL worth printing: GitHub renders `compare/main...main` as an empty
/// diff.
#[test]
fn test_views_push_to_the_default_branch_offers_no_pull_request() {
    let world = World::new();
    world.publish_as_github("https://github.com/indexable-inc/upstream.git");

    let output = world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "views",
                "push",
                "--branch",
                "main",
                "--allow-default-branch",
            ],
        )
        .success()
        .stderr
        .raw()
        .to_owned();
    assert!(
        output.contains("as main"),
        "the push did not happen: {output}"
    );
    assert!(
        !output.contains("open a pull request"),
        "a push to the base offered a pull request against itself: {output}"
    );
}

/// ENG-11940: a tip nobody described is not published quietly.
///
/// The description a view sends is the monorepo commit's, so a change nobody
/// described here lands in a repository other people read as a commit nobody
/// described there either. `jj git push` refuses the same thing behind the same
/// flag name.
#[test]
fn test_views_push_refuses_an_undescribed_tip() {
    let world = World::new();
    let work_dir = world.test_env.work_dir("monorepo");
    work_dir.run_jj(["new"]).success();
    work_dir.write_file(format!("{PREFIX}/UNDESCRIBED.md"), b"nobody said why\n");

    let output = world.test_env.run_jj_in("monorepo", ["views", "push"]);
    let refusal = output.stderr.raw().to_owned();
    assert!(
        refusal.contains("Won't push the upstream view"),
        "the undescribed tip was not refused: {refusal}"
    );
    assert!(
        refusal.contains("has no description"),
        "the refusal did not say why: {refusal}"
    );
    assert!(
        refusal.contains("--allow-empty-description"),
        "the refusal did not name the way past it: {refusal}"
    );

    // Refused before anything was sent.
    let published = world.published();
    let branches: Vec<String> = published
        .references()
        .expect("the reference store")
        .prefixed("refs/heads/")
        .expect("a prefix iterator")
        .filter_map(Result::ok)
        .map(|reference| reference.name().as_bstr().to_string())
        .collect();
    assert_eq!(branches, ["refs/heads/main"]);

    // And the same push goes through when asked twice.
    world
        .test_env
        .run_jj_in("monorepo", ["views", "push", "--allow-empty-description"])
        .success();
}

/// The hint after a push says what moved and what did not.
///
/// ENG-11940: "run `jj git push` for this repository itself" reads as though
/// the views push had already done something to this repository.
#[test]
fn test_views_push_says_what_it_left_alone() {
    let world = World::new();

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "push"])
        .success()
        .stderr
        .raw()
        .to_owned();
    assert!(
        output.contains(
            "Hint: Only the views were pushed. Nothing in this repository moved: its own \
             bookmarks, tags and remotes are exactly as they were, and `jj git push` is what \
             sends those."
        ),
        "the hint did not say what was left alone: {output}"
    );
}

#[test]
fn test_views_fetch_reports_an_unchanged_view() {
    let world = World::new();
    world.bookmark_main();

    let output = world.test_env.run_jj_in("monorepo", ["views", "fetch"]);
    insta::assert_snapshot!(output, @r"
    ------- stderr -------
    upstream: already up to date.
    [EOF]
    ");
}

#[test]
fn test_views_fetch_fast_forwards_without_touching_local_work() {
    let world = World::new();
    world.bookmark_main();
    world.publish_upstream_commit("PUBLISHED.md", "published upstream");

    // The commit the working copy is on before the fetch. `jj git fetch` does
    // not move the working copy, and neither does this.
    let before = world.git_id("@");

    let output = world.test_env.run_jj_in("monorepo", ["views", "fetch"]);
    let report = output.success().stderr.raw().to_owned();
    assert!(
        report.contains("advanced 1 commit "),
        "unexpected report: {report}"
    );

    assert_eq!(
        world.git_id("@"),
        before,
        "the fetch moved the working copy"
    );

    // The lifted commit really is the published one, under the prefix.
    let files = file_list(&world, &["file", "list", "-r", "main", PREFIX]);
    assert!(
        files.contains(&format!("{PREFIX}/PUBLISHED.md")),
        "the published file did not arrive under the prefix: {files}"
    );
}

/// ENG-12220: a first fetch cannot attach a standalone root in place of the
/// host history.
///
/// `unfilter` preserves the standalone root's identity by keeping it a root.
/// Moving a host bookmark straight to that commit therefore discards the
/// bookmark's ancestry even though its files survive in the new tree.
#[test]
fn test_views_fetch_refuses_a_prefix_that_was_never_imported() {
    let world = World::new();
    let work_dir = world.test_env.work_dir("monorepo");
    work_dir.run_jj(["new", "root()"]).success();
    work_dir.write_file("HOST.md", b"host history\n");
    work_dir
        .run_jj(["describe", "-m", "host history outside the view"])
        .success();
    work_dir
        .run_jj(["bookmark", "create", "main", "-r", "@"])
        .success();
    work_dir.run_jj(["new", "main"]).success();
    let before = world.git_id("main");

    let output = world.test_env.run_jj_in("monorepo", ["views", "fetch"]);
    let error = output.stderr.raw().to_owned();
    assert!(
        !output.status.success(),
        "the lossy first fetch succeeded: {error}"
    );
    assert!(
        error.contains("has never been imported") && error.contains("jj-views import"),
        "the refusal did not name the safe import path: {error}"
    );
    assert_eq!(world.git_id("main"), before, "the refusal moved main");
}

#[test]
fn test_views_manifest_configures_a_fresh_clone_and_uses_its_anchor() {
    let world = World::new();
    world.bookmark_main();
    std::fs::remove_file(world.test_env.last_config_file_path())
        .expect("the local views config exists");
    let published = world.test_env.env_root().join("published.git");
    let source = world.git_id("main");
    let manifest = format!(
        "[views.upstream]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \
         \"main\"\n[views.upstream.anchor]\nsource = \"{source}\"\nview = \"{}\"\n",
        published.to_str().expect("a utf8 path"),
        world.upstream.head,
    );
    world
        .test_env
        .work_dir("monorepo")
        .write_file(".jj-views.toml", manifest.as_bytes());

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch"])
        .success();
    assert!(
        output.stderr.raw().contains("already up to date"),
        "the manifest-backed view was not loaded: {}",
        output.stderr.raw()
    );
}

#[test]
fn test_views_anchor_falls_back_to_upstream_when_published_is_empty() {
    let world = World::new();
    world.bookmark_main();
    std::fs::remove_file(world.test_env.last_config_file_path())
        .expect("the local views config exists");
    let published = world.test_env.env_root().join("published.git");
    let host_remote = world.test_env.env_root().join("host.git");
    git(
        world.test_env.env_root(),
        &[
            "init",
            "-q",
            "--bare",
            host_remote.to_str().expect("a utf8 path"),
        ],
    );
    let source = world.git_id("main");
    let manifest = format!(
        "[views.upstream]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \"main\"\nupstream-remote = \
         {:?}\nupstream-branch = \"main\"\n[views.upstream.anchor]\nsource = \"{source}\"\nview = \
         \"{}\"\n",
        host_remote.to_str().expect("a utf8 path"),
        published.to_str().expect("a utf8 path"),
        world.upstream.head,
    );
    world
        .test_env
        .work_dir("monorepo")
        .write_file(".jj-views.toml", manifest.as_bytes());

    let root = world.upstream.commits[0].1;
    for (_, commit) in &world.upstream.commits {
        let hex = commit.to_string();
        let object = World::store_path_in(world.test_env.env_root())
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        if object.exists() {
            std::fs::remove_file(object).expect("a removable loose fixture commit");
        }
    }
    assert!(
        world.store().find_object(world.upstream.head).is_err(),
        "the fixture did not remove the anchor object"
    );

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "anchor", "upstream", "--json"])
        .success();
    let value: serde_json::Value =
        serde_json::from_str(output.stdout.raw()).expect("valid anchor JSON");
    assert_eq!(value["views"].as_array().map(Vec::len), Some(1));
    assert_eq!(value["views"][0]["fetched_commits"], 1);
    assert_eq!(value["views"][0]["tree_matches"], true);
    assert_eq!(value["views"][0]["endpoint"], "upstream");
    assert_eq!(
        value["views"][0]["attempts"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(value["views"][0]["attempts"][0]["endpoint"], "published");
    assert!(
        world.store().find_object(world.upstream.head).is_ok(),
        "the anchor commit was not installed"
    );
    assert!(
        world.store().find_object(root).is_err(),
        "the depth-1 fetch materialized the anchor's old ancestry"
    );
    let status = world
        .test_env
        .run_jj_in(
            "monorepo",
            ["views", "status", "--upstream", "upstream", "--json"],
        )
        .success();
    let status: serde_json::Value =
        serde_json::from_str(status.stdout.raw()).expect("valid status JSON");
    assert_eq!(status["views"][0]["state"], "up_to_date");
}

#[test]
fn test_views_anchor_falls_back_to_the_bounded_published_patch_series() {
    let world = World::new();
    world.bookmark_main();
    let anchor_source = world.git_id("main");
    let work_dir = world.test_env.work_dir("monorepo");
    work_dir
        .run_jj(["bookmark", "set", "main", "-r", "@"])
        .success();
    work_dir.run_jj(["new", "main"]).success();
    work_dir.write_file(format!("{PREFIX}/PATCH-2"), b"second patch\n");
    work_dir
        .run_jj(["describe", "-m", "second published patch"])
        .success();
    work_dir
        .run_jj(["bookmark", "set", "main", "-r", "@"])
        .success();
    world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "views",
                "push",
                "upstream",
                "--revision",
                "main",
                "--branch",
                "main",
                "--allow-default-branch",
            ],
        )
        .success();
    work_dir.run_jj(["new", "main"]).success();

    std::fs::remove_file(world.test_env.last_config_file_path())
        .expect("the local views config exists");
    let published = world.test_env.env_root().join("published.git");
    let rejecting = world.test_env.env_root().join("rejecting.git");
    git(
        world.test_env.env_root(),
        &[
            "init",
            "-q",
            "--bare",
            rejecting.to_str().expect("a utf8 path"),
        ],
    );
    let manifest = format!(
        "[views.upstream]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \"main\"\nupstream-remote = \
         {:?}\nupstream-branch = \"main\"\n[views.upstream.anchor]\nsource = \
         \"{anchor_source}\"\nview = \"{}\"\n",
        published.to_str().expect("a utf8 path"),
        rejecting.to_str().expect("a utf8 path"),
        world.upstream.head,
    );
    work_dir.write_file(".jj-views.toml", manifest.as_bytes());
    for (_, commit) in &world.upstream.commits {
        let hex = commit.to_string();
        let object = World::store_path_in(world.test_env.env_root())
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        if object.exists() {
            std::fs::remove_file(object).expect("a removable loose fixture commit");
        }
    }

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "anchor", "upstream", "--json"])
        .success();
    let value: serde_json::Value =
        serde_json::from_str(output.stdout.raw()).expect("valid anchor JSON");
    assert_eq!(value["views"][0]["endpoint"], "published");
    assert_eq!(
        value["views"][0]["attempts"].as_array().map(Vec::len),
        Some(0)
    );
    assert!(
        world.store().find_object(world.upstream.head).is_ok(),
        "the published branch did not install the anchor"
    );
    assert_anchor_object_closure(&world.store(), world.upstream.head);
}

#[test]
fn test_views_six_parent_integration_preserves_selected_view_history() {
    let world = World::new();
    world.bookmark_main();
    let work_dir = world.test_env.work_dir("monorepo");
    let anchor_source = world.git_id("main");
    let anchor = DeriveAnchor {
        source: anchor_source,
        view: world.upstream.head,
    };

    let mut parents = Vec::new();
    for index in 0..6 {
        work_dir.run_jj(["new", "main"]).success();
        if index == 0 {
            work_dir.write_file(format!("{PREFIX}/SELECTED.md"), b"selected view history\n");
        } else {
            work_dir.write_file(
                format!("OTHER-{index}.md"),
                format!("independent view {index}\n").as_bytes(),
            );
        }
        work_dir
            .run_jj(["describe", "-m", &format!("independent view {index}")])
            .success();
        parents.push(world.git_id("@"));
    }
    let mut merge_args = vec!["new".to_owned()];
    merge_args.extend(parents.iter().map(ToString::to_string));
    work_dir.run_jj(merge_args).success();
    work_dir
        .run_jj(["describe", "-m", "integrate six independent views"])
        .success();
    work_dir
        .run_jj(["bookmark", "set", "main", "-r", "@"])
        .success();
    let revision = world.git_id("main");

    let repo = world.store();
    let raw = repo
        .find_object(revision)
        .expect("the integration commit")
        .detach()
        .data;
    let parsed = gix::objs::CommitRef::from_bytes(&raw, repo.object_hash())
        .expect("a valid integration commit");
    assert_eq!(
        parsed.parents().count(),
        6,
        "the regression must cross the six-parent production shape"
    );

    let filter = filter();
    let mut cache = Cache::new();
    cache
        .seed_anchor(&repo, &revision, &filter, anchor)
        .expect("a valid source anchor");
    let selected_tip = jj_views::derive(&repo, &parents[0], &filter, &mut cache)
        .expect("the selected parent derives")
        .expect("the selected parent has view history");
    let reads_before = cache.commit_reads();
    let expected_tip = jj_views::derive_tip(&repo, &revision, &filter, &mut cache)
        .expect("the integration derives")
        .expect("the integration has view history");
    assert_eq!(
        expected_tip, selected_tip,
        "parents with no selected path changes must collapse"
    );
    let reads = cache.commit_reads() - reads_before;
    assert_eq!(
        reads, 7,
        "tip derivation must read the merge and each of its six parent tips once"
    );
    for parent in &parents[1..] {
        assert_eq!(
            cache.derived(parent, &filter),
            None,
            "tip derivation traversed an irrelevant parent history"
        );
    }

    let published = world.test_env.env_root().join("published.git");
    let refspec = format!("{expected_tip}:refs/heads/main");
    git(
        world.test_env.env_root(),
        &[
            "--git-dir",
            &World::store_path_in(world.test_env.env_root()).to_string_lossy(),
            "push",
            "-q",
            published.to_str().expect("a utf8 path"),
            &refspec,
        ],
    );
    let manifest = format!(
        "[views.upstream]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \
         \"main\"\n[views.upstream.anchor]\nsource = \"{}\"\nview = \"{}\"\n",
        published.to_str().expect("a utf8 path"),
        anchor.source,
        anchor.view,
    );
    work_dir.write_file(".jj-views.toml", manifest.as_bytes());
    git(
        world.test_env.env_root(),
        &[
            "--git-dir",
            &World::store_path_in(world.test_env.env_root()).to_string_lossy(),
            "prune",
            "--expire",
            "now",
        ],
    );
    assert!(
        world.store().find_object(anchor.view).is_err(),
        "the anchor survived pruning, so the command has nothing to fetch"
    );

    let anchored = world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "views",
                "anchor",
                "upstream",
                "--bookmark",
                "main",
                "--json",
            ],
        )
        .success();
    let anchor_json: serde_json::Value =
        serde_json::from_str(anchored.stdout.raw()).expect("valid anchor JSON");
    assert_eq!(anchor_json["views"][0]["endpoint"], "published");

    let status = world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "views",
                "status",
                "upstream",
                "--bookmark",
                "main",
                "--json",
            ],
        )
        .success();
    let status_json: serde_json::Value =
        serde_json::from_str(status.stdout.raw()).expect("valid status JSON");
    assert_eq!(status_json["views"][0]["state"], "up_to_date");
    assert_eq!(
        status_json["views"][0]["published_commit"],
        expected_tip.to_string()
    );

    let dry_run = world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "views",
                "push",
                "upstream",
                "--revision",
                "main",
                "--dry-run",
            ],
        )
        .success();
    assert!(
        dry_run.stderr.raw().contains(&expected_tip.to_string()),
        "push dry-run did not preserve the selected view tip: {}",
        dry_run.stderr.raw()
    );
}

#[test]
fn test_views_anchor_uses_a_local_object_without_network() {
    let world = World::new();
    world.bookmark_main();
    std::fs::remove_file(world.test_env.last_config_file_path())
        .expect("the local views config exists");
    let source = world.git_id("main");
    let manifest = format!(
        "[views.upstream]\npath = {PREFIX:?}\nremote = \"file:///does-not-exist/published\"\nbranch = \
         \"main\"\nupstream-remote = \"file:///does-not-exist/upstream\"\nupstream-branch = \
         \"main\"\n[views.upstream.anchor]\nsource = \"{source}\"\nview = \"{}\"\n",
        world.upstream.head,
    );
    world
        .test_env
        .work_dir("monorepo")
        .write_file(".jj-views.toml", manifest.as_bytes());

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "anchor", "upstream", "--json"])
        .success();
    let value: serde_json::Value =
        serde_json::from_str(output.stdout.raw()).expect("valid anchor JSON");
    assert_eq!(value["views"][0]["endpoint"], "local");
    assert_eq!(value["views"][0]["fetched_commits"], 0);
    assert_eq!(
        value["views"][0]["attempts"].as_array().map(Vec::len),
        Some(0)
    );
}

#[test]
fn test_views_anchor_error_names_each_bounded_endpoint_attempt() {
    let world = World::new();
    world.bookmark_main();
    std::fs::remove_file(world.test_env.last_config_file_path())
        .expect("the local views config exists");
    let source = world.git_id("main");
    let published = world.test_env.env_root().join("empty-published.git");
    let upstream = world.test_env.env_root().join("empty-upstream.git");
    for remote in [&published, &upstream] {
        git(
            world.test_env.env_root(),
            &[
                "init",
                "-q",
                "--bare",
                remote.to_str().expect("a utf8 path"),
            ],
        );
    }
    let manifest = format!(
        "[views.upstream]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \"main\"\nupstream-remote = \
         {:?}\nupstream-branch = \"main\"\n[views.upstream.anchor]\nsource = \"{source}\"\nview = \
         \"{}\"\n",
        published.to_str().expect("a utf8 path"),
        upstream.to_str().expect("a utf8 path"),
        world.upstream.head,
    );
    world
        .test_env
        .work_dir("monorepo")
        .write_file(".jj-views.toml", manifest.as_bytes());
    for (_, commit) in &world.upstream.commits {
        let hex = commit.to_string();
        let object = World::store_path_in(world.test_env.env_root())
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        if object.exists() {
            std::fs::remove_file(object).expect("a removable loose fixture commit");
        }
    }

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "anchor", "upstream"]);
    assert!(
        !output.status.success(),
        "empty endpoints supplied an anchor"
    );
    let error = output.stderr.raw();
    assert!(
        error.contains("read-only upstream")
            && error.contains(&world.upstream.head.to_string())
            && error.contains("published branch")
            && error.contains("refs/heads/main"),
        "endpoint attempts were not preserved in the error: {error}"
    );
}

#[test]
fn test_views_root_anchor_pushes_to_an_empty_published_repository() {
    let world = World::new();
    world.bookmark_main();
    std::fs::remove_file(world.test_env.last_config_file_path())
        .expect("the local views config exists");
    let published = world.test_env.env_root().join("published.git");
    let host_remote = world.test_env.env_root().join("host.git");
    git(
        world.test_env.env_root(),
        &[
            "init",
            "-q",
            "--bare",
            host_remote.to_str().expect("a utf8 path"),
        ],
    );
    let source = world.git_id("main");
    let root_anchor =
        jj_views::root_anchor_id(&world.store(), &source, &filter()).expect("a root anchor id");
    let manifest = format!(
        "[views.upstream]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \"main\"\nupstream-remote = \
         {:?}\nupstream-branch = \"main\"\n[views.upstream.anchor]\nsource = \"{source}\"\nview = \
         \"{root_anchor}\"\nroot = true\n",
        host_remote.to_str().expect("a utf8 path"),
        published.to_str().expect("a utf8 path"),
    );
    world
        .test_env
        .work_dir("monorepo")
        .write_file(".jj-views.toml", manifest.as_bytes());

    let root = world.upstream.commits[0].1;
    for (_, commit) in &world.upstream.commits {
        let hex = commit.to_string();
        let object = World::store_path_in(world.test_env.env_root())
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        if object.exists() {
            std::fs::remove_file(object).expect("a removable loose fixture commit");
        }
    }
    assert!(
        world.store().find_object(world.upstream.head).is_err(),
        "the fixture did not remove the anchor object"
    );

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "anchor", "upstream", "--json"])
        .success();
    let value: serde_json::Value =
        serde_json::from_str(output.stdout.raw()).expect("valid anchor JSON");
    assert_eq!(value["views"].as_array().map(Vec::len), Some(1));
    assert_eq!(value["views"][0]["fetched_commits"], 0);
    assert_eq!(value["views"][0]["tree_matches"], true);
    assert!(
        world.store().find_object(root_anchor).is_ok(),
        "the root anchor commit was not installed"
    );
    assert!(
        world.store().find_object(root).is_err(),
        "the depth-1 fetch materialized the anchor's old ancestry"
    );
    let work_dir = world.test_env.work_dir("monorepo");
    work_dir
        .run_jj(["bookmark", "set", "main", "-r", "@"])
        .success();
    work_dir.run_jj(["new", "main"]).success();
    work_dir.write_file(format!("{PREFIX}/ROOT-PATCH"), b"first patch\n");
    work_dir.run_jj(["describe", "-m", "first patch"]).success();
    work_dir
        .run_jj(["bookmark", "set", "main", "-r", "@"])
        .success();
    world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "views",
                "push",
                "upstream",
                "--branch",
                "main",
                "--allow-default-branch",
            ],
        )
        .success();
    let remote = gix::open(&host_remote).expect("the published repository");
    let tip = remote
        .find_reference("refs/heads/main")
        .expect("the first published branch")
        .peel_to_id()
        .expect("a published tip")
        .detach();
    assert_ne!(tip, root_anchor, "the first patch was not published");
    assert!(
        remote.find_object(root_anchor).is_ok(),
        "the remote is missing the root anchor"
    );
    git(
        &host_remote,
        &[
            "merge-base",
            "--is-ancestor",
            &root_anchor.to_string(),
            &tip.to_string(),
        ],
    );
    git(&host_remote, &["fsck", "--full", "--strict"]);
}

#[test]
fn test_views_manifest_rejects_an_incomplete_upstream_endpoint() {
    let world = World::new();
    world.bookmark_main();
    std::fs::remove_file(world.test_env.last_config_file_path())
        .expect("the local views config exists");
    let published = world.test_env.env_root().join("published.git");
    let manifest = format!(
        "[views.upstream]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \"main\"\n\
         upstream-remote = \"https://example.invalid/upstream.git\"\n",
        published.to_str().expect("a utf8 path"),
    );
    world
        .test_env
        .work_dir("monorepo")
        .write_file(".jj-views.toml", manifest.as_bytes());

    let output = world.test_env.run_jj_in("monorepo", ["views", "fetch"]);
    let error = output.stderr.raw();
    assert!(
        !output.status.success(),
        "the incomplete endpoint was accepted"
    );
    assert!(
        error.contains("upstream-remote") && error.contains("upstream-branch"),
        "the manifest error did not name the paired fields: {error}"
    );
}
#[cfg(unix)]
#[test]
fn test_views_patches_are_deterministic_and_work_without_the_view_materialized() {
    use std::os::unix::fs::PermissionsExt as _;

    use sha2::Digest as _;

    let world = World::new();
    let work_dir = world.test_env.work_dir("monorepo");
    let source = world.git_id("vendored");
    let published = world.test_env.env_root().join("published.git");
    let manifest = format!(
        "[views.upstream]\npath = {PREFIX:?}\nremote = {:?}\nbranch = \
         \"main\"\n[views.upstream.anchor]\nsource = \"{source}\"\nview = \"{}\"\n",
        published.to_str().expect("a utf8 path"),
        world.upstream.head,
    );
    work_dir.write_file(".jj-views.toml", manifest.as_bytes());

    let prefix = work_dir.root().join(PREFIX);
    std::fs::rename(prefix.join("README"), prefix.join("RENAMED"))
        .expect("the fixture file can be renamed");
    std::fs::write(prefix.join("RENAMED"), b"upstream\n")
        .expect("the rename keeps the anchor content");
    work_dir.write_file(format!("{PREFIX}/executable"), b"#!/bin/sh\nexit 0\n");
    let mut executable = std::fs::metadata(prefix.join("executable"))
        .expect("executable metadata")
        .permissions();
    executable.set_mode(0o755);
    std::fs::set_permissions(prefix.join("executable"), executable)
        .expect("executable permissions");
    std::os::unix::fs::symlink("RENAMED", prefix.join("export-link")).expect("a symlink");
    work_dir.write_file(
        format!("{PREFIX}/binary"),
        b"\0\x01\x02\xffbinary contents\0",
    );
    work_dir
        .run_jj(["describe", "-m", "change every Git file kind"])
        .success();
    work_dir
        .run_jj(["sparse", "set", "--clear", "--add", ".jj-views.toml"])
        .success();
    assert!(
        !prefix.exists(),
        "the view path must be absent or this does not test sparse export"
    );

    let output = world.test_env.env_root().join("patch-output");
    let archive = world.test_env.env_root().join("patches.tar.zst");
    std::fs::create_dir(&output).expect("an empty output directory");
    let output_arg = output.to_string_lossy().into_owned();
    let archive_arg = archive.to_string_lossy().into_owned();
    let args = [
        "views",
        "patches",
        "upstream",
        "-r",
        "@",
        "--output",
        output_arg.as_str(),
        "--archive",
        archive_arg.as_str(),
        "--json",
    ];

    let first = world
        .test_env
        .run_jj_in("monorepo", args)
        .success()
        .stdout
        .raw()
        .as_bytes()
        .to_vec();
    let manifest_bytes = std::fs::read(output.join("manifest.json")).expect("the manifest");
    assert_eq!(first, manifest_bytes, "stdout and manifest JSON diverged");
    let value: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("valid manifest JSON");
    assert_eq!(value["view"], "upstream");
    assert_eq!(value["host_revision"].as_str().map(str::len), Some(40));
    assert_eq!(value["anchor_source"], source.to_string());
    assert_eq!(value["anchor_view"], world.upstream.head.to_string());
    assert_eq!(value["patch_count"], 1);
    assert_eq!(value["commit_ids"].as_array().map(Vec::len), Some(1));
    assert_eq!(value["patches"].as_array().map(Vec::len), Some(1));
    assert_eq!(value["archive_path"], archive_arg);
    assert_eq!(value["archive_sha256"].as_str().map(str::len), Some(64));

    let patch_path = value["patches"][0]["path"].as_str().expect("a patch path");
    let commit_id = value["commit_ids"][0].as_str().expect("a commit id");
    assert_eq!(patch_path, format!("patches/0001-{commit_id}.patch"));
    let patch = std::fs::read(output.join(patch_path)).expect("the patch");
    assert_eq!(
        value["patches"][0]["sha256"],
        format!("{:x}", sha2::Sha256::digest(&patch))
    );
    let patch_text = String::from_utf8_lossy(&patch);
    for marker in [
        "rename from README",
        "new file mode 100755",
        "new file mode 120000",
        "GIT binary patch",
    ] {
        assert!(
            patch_text.contains(marker),
            "the patch did not preserve {marker}: {patch_text}"
        );
    }
    let archived = tar_entries(&archive);
    assert_eq!(archived, vec![(patch_path.to_owned(), patch.clone())]);

    let applied = world.test_env.env_root().join("applied");
    git(
        world.test_env.env_root(),
        &[
            "clone",
            "-q",
            "--branch",
            "main",
            published.to_str().expect("a utf8 path"),
            applied.to_str().expect("a utf8 path"),
        ],
    );
    git(&applied, &["config", "user.email", "apply@example.invalid"]);
    git(&applied, &["config", "user.name", "Apply"]);
    git(
        &applied,
        &[
            "am",
            "-q",
            output.join(patch_path).to_str().expect("a utf8 path"),
        ],
    );
    assert_eq!(
        git(&applied, &["rev-parse", "HEAD^{tree}"]).trim(),
        value["view_tree"].as_str().expect("a view tree"),
        "applying the artifact did not recreate the derived view tree"
    );

    let first_archive = std::fs::read(&archive).expect("the first archive");
    assert_eq!(
        value["archive_sha256"],
        format!("{:x}", sha2::Sha256::digest(&first_archive))
    );
    let first_patch = patch;
    let nonempty = world.test_env.run_jj_in("monorepo", args);
    assert!(!nonempty.status.success(), "a nonempty output was accepted");
    assert!(
        nonempty.stderr.raw().contains("is not empty"),
        "unexpected nonempty-output refusal: {}",
        nonempty.stderr.raw()
    );

    std::fs::remove_dir_all(&output).expect("remove the first output");
    std::fs::create_dir(&output).expect("a second empty output");
    let second = world
        .test_env
        .run_jj_in("monorepo", args)
        .success()
        .stdout
        .raw()
        .as_bytes()
        .to_vec();
    assert_eq!(second, first, "the JSON changed across identical exports");
    assert_eq!(
        std::fs::read(output.join("manifest.json")).expect("the second manifest"),
        manifest_bytes,
        "the manifest changed across identical exports"
    );
    assert_eq!(
        std::fs::read(output.join(patch_path)).expect("the second patch"),
        first_patch,
        "the patch changed across identical exports"
    );
    assert_eq!(
        std::fs::read(&archive).expect("the existing archive"),
        first_archive,
        "the existing matching archive changed"
    );

    std::fs::remove_dir_all(&output).expect("remove the second output");
    std::fs::create_dir(&output).expect("a third empty output");
    std::fs::write(&archive, b"wrong artifact").expect("a mismatched archive");
    let mismatched = world.test_env.run_jj_in("monorepo", args);
    assert!(
        !mismatched.status.success(),
        "a mismatched existing archive was accepted"
    );
    assert!(
        mismatched
            .stderr
            .raw()
            .contains("differs from the derived artifact"),
        "unexpected artifact refusal: {}",
        mismatched.stderr.raw()
    );
    assert_eq!(
        std::fs::read_dir(&output)
            .expect("the rejected output")
            .count(),
        0,
        "a rejected artifact left partial output"
    );
}

#[cfg(unix)]
fn tar_entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let archive = std::fs::File::open(path).expect("the compressed archive");
    let tar = zstd::stream::decode_all(archive).expect("a valid zstd stream");
    let mut entries = Vec::new();
    let mut offset = 0;
    loop {
        let header = &tar[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            assert!(
                tar[offset..].iter().all(|byte| *byte == 0),
                "nonzero data follows the tar terminator"
            );
            return entries;
        }
        let name_end = header[..100]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(100);
        let name = std::str::from_utf8(&header[..name_end])
            .expect("a utf8 archive path")
            .to_owned();
        let size = std::str::from_utf8(&header[124..136])
            .expect("an octal size")
            .trim_matches(['\0', ' ']);
        let size = usize::from_str_radix(size, 8).expect("a valid octal size");
        let data_start = offset + 512;
        entries.push((name, tar[data_start..data_start + size].to_vec()));
        offset = data_start + size.div_ceil(512) * 512;
    }
}

#[test]
fn test_views_upstream_requires_a_read_only_endpoint() {
    let world = World::new();
    world.bookmark_main();

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--upstream", "upstream"]);
    assert!(
        !output.status.success(),
        "an absent upstream endpoint was accepted"
    );
    assert!(
        output
            .stderr
            .raw()
            .contains("no read-only upstream endpoint"),
        "unexpected refusal: {}",
        output.stderr.raw()
    );
}

#[test]
fn test_views_upstream_fetches_read_only_and_push_still_uses_published_remote() {
    let world = World::new();
    world.bookmark_main();
    let read_only = world.test_env.env_root().join("read-only-upstream.git");
    git(
        world.test_env.env_root(),
        &[
            "clone",
            "-q",
            "--bare",
            world
                .test_env
                .env_root()
                .join("published.git")
                .to_str()
                .expect("a utf8 path"),
            read_only.to_str().expect("a utf8 path"),
        ],
    );
    world.test_env.add_config(format!(
        "[views.upstream]\nupstream-remote = {:?}\nupstream-branch = \"main\"\n",
        read_only.to_str().expect("a utf8 path")
    ));
    let upstream_work = world.test_env.env_root().join("read-only-work");
    git(
        world.test_env.env_root(),
        &[
            "clone",
            "-q",
            "--branch",
            "main",
            read_only.to_str().expect("a utf8 path"),
            upstream_work.to_str().expect("a utf8 path"),
        ],
    );
    git(
        &upstream_work,
        &["config", "user.email", "up@example.invalid"],
    );
    git(&upstream_work, &["config", "user.name", "Up"]);
    std::fs::write(upstream_work.join("UPSTREAM.md"), b"read only source\n")
        .expect("a writable upstream clone");
    git(&upstream_work, &["add", "UPSTREAM.md"]);
    git(
        &upstream_work,
        &["commit", "-q", "-m", "read-only upstream change"],
    );
    git(&upstream_work, &["push", "-q", "origin", "HEAD:main"]);

    let read_only_before = git(&read_only, &["show-ref"]);
    let pushed = world
        .test_env
        .run_jj_in("monorepo", ["views", "push"])
        .success();
    assert!(
        pushed.stderr.raw().contains("published"),
        "the push did not reach the configured published remote: {}",
        pushed.stderr.raw()
    );
    assert_eq!(
        git(&read_only, &["show-ref"]),
        read_only_before,
        "jj views push wrote to the read-only upstream"
    );

    let fetched = world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch", "--upstream", "upstream"])
        .success();
    assert!(
        fetched.stderr.raw().contains("advanced 1 commit"),
        "fetch did not lift from the read-only endpoint: {}",
        fetched.stderr.raw()
    );
    let files = file_list(&world, &["file", "list", "-r", "main", PREFIX]);
    assert!(
        files.contains(&format!("{PREFIX}/UPSTREAM.md")),
        "the read-only endpoint's file was not lifted: {files}"
    );
}

/// A diverged view is a state, not a failure: the published commits arrive
/// beside this repository's history, and only the bookmark stays put.
#[test]
fn test_views_fetch_brings_a_diverged_view_in_beside_the_bookmark() {
    let world = World::new();
    world.bookmark_main();
    diverge(&world);

    let before = world.git_id("main");
    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch"])
        .success();
    let report = output.stderr.raw().to_owned();
    assert!(report.contains("diverged"), "unexpected report: {report}");
    assert!(
        report.contains("main was not moved"),
        "the report did not say the bookmark stayed put: {report}"
    );
    assert!(
        report.contains("`jj new main "),
        "the report did not name the way to integrate it: {report}"
    );
    assert!(
        report.contains("Do not rebase it onto main"),
        "the report did not warn about the one thing that rewrites published commits: {report}"
    );
    assert_eq!(
        world.git_id("main"),
        before,
        "a diverged fetch moved the bookmark"
    );

    // The published history is a revision this repository can name, which is
    // the whole point of bringing it in rather than refusing.
    let arrived = lifted_revision(&report);
    let listed = file_list(&world, &["file", "list", "-r", &arrived, PREFIX]);
    assert!(
        listed.contains(&format!("{PREFIX}/THEIRS.md")),
        "the arrived revision does not hold the published work: {listed}"
    );
}

#[test]
fn test_views_fetch_dry_run_changes_nothing() {
    let world = World::new();
    world.bookmark_main();
    world.publish_upstream_commit("PUBLISHED.md", "published upstream");

    let before = world
        .test_env
        .run_jj_in(
            "monorepo",
            ["log", "--no-graph", "-r", "main", "-T", "commit_id"],
        )
        .success()
        .stdout
        .raw()
        .to_owned();

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch", "--dry-run"])
        .success();
    assert!(
        output.stderr.raw().contains("would lift 1 commit "),
        "unexpected report: {}",
        output.stderr.raw()
    );

    let after = world
        .test_env
        .run_jj_in(
            "monorepo",
            ["log", "--no-graph", "-r", "main", "-T", "commit_id"],
        )
        .success()
        .stdout
        .raw()
        .to_owned();
    assert_eq!(before, after, "a dry run moved the bookmark");
}

/// ENG-11873: the second fetch of a remote that has not moved must do nothing.
///
/// Before the fix it lifted the published tip again, and again on every run
/// after that, because the view elides the lifted commit and `derive` therefore
/// cannot see that it arrived. `jj log` filled up with copies of one upstream
/// merge, all carrying the same description.
#[test]
fn test_views_fetch_converges_when_the_published_tip_is_elided() {
    let world = World::new();
    world.bookmark_main();
    world.commit_outside_the_prefix();
    world.publish_upstream_commit("LANDED.md", "a change that landed on main");
    world.publish_already_landed_merge();

    let first = world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch"])
        .success();
    assert!(
        first.stderr.raw().contains("advanced"),
        "the first fetch lifted nothing: {}",
        first.stderr.raw()
    );
    let after_first = world.git_id("main");

    // The shape only guards anything while the view really does elide the
    // published tip. If a later change to the elision rule keeps it, this test
    // starts passing for a reason that has nothing to do with the bug.
    let mut cache = Cache::new();
    let derived = jj_views::derive(&world.store(), &after_first, &filter(), &mut cache)
        .expect("a derivation")
        .expect("a view tip");
    assert_ne!(
        derived,
        world.published_tip(),
        "the view no longer elides the published tip, so this test is not exercising ENG-11873 \
         any more and needs a new shape"
    );

    let second = world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch"])
        .success();
    let report = second.stderr.raw().to_owned();
    assert!(
        report.contains("already up to date"),
        "the second fetch of an unmoved remote did not report a no-op: {report}"
    );
    assert!(
        !report.contains("advanced"),
        "the second fetch of an unmoved remote lifted again: {report}"
    );
    assert!(
        report.contains("elided from the view"),
        "the report did not say why raw ancestry still reads as behind: {report}"
    );
    assert_eq!(
        world.git_id("main"),
        after_first,
        "the second fetch of an unmoved remote moved the bookmark"
    );
}

#[test]
fn test_views_status_reports_an_unchanged_view() {
    let world = World::new();
    world.bookmark_main();

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status"])
        .success();
    let stdout = output.stdout.raw().to_owned();
    assert!(
        stdout.contains("upstream: up to date"),
        "unexpected report: {stdout}"
    );
    assert!(
        stdout.contains(&world.published_tip().to_string()),
        "the report did not name what the published repository points at: {stdout}"
    );
}

#[test]
fn test_views_status_reads_each_remote_after_its_fetch() {
    let world = World::new();
    world.bookmark_main();
    // 31 existing packs give gix-odb 34 slots under its measured 1.1 allowance.
    // ENG-12273 showed that the fourth pack fetched afterwards exhausts it.
    world.seed_pack_indexes(31);
    assert_eq!(world.pack_count(), 31, "the store must start with 31 packs");
    world.configure_packed_views(4);
    assert_eq!(world.pack_count(), 31, "setup must preserve the 31 packs");

    world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--json"])
        .success();
}

#[test]
fn test_views_status_checks_every_no_fetch_ref_before_anchor_derivation() {
    let world = World::new();
    world.bookmark_main();
    world.configure_packed_views(2);
    world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "packed-00"])
        .success();

    let manifest = String::from_utf8(
        world
            .test_env
            .work_dir("monorepo")
            .read_file(".jj-views.toml")
            .into(),
    )
    .expect("a utf8 views manifest");
    let invalid_source = world.git_id("root()");
    let invalid = manifest.replacen(
        &format!("source = \"{}\"", world.git_id("vendored")),
        &format!("source = \"{invalid_source}\""),
        1,
    );
    world
        .test_env
        .work_dir("monorepo")
        .write_file(".jj-views.toml", invalid.as_bytes());

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--no-fetch"]);
    assert!(!output.status.success(), "a missing tracking ref passed");
    assert!(
        output
            .stderr
            .raw()
            .contains("The packed-01 view has never been fetched"),
        "status derived packed-00 before checking packed-01: {}",
        output.stderr.raw()
    );
}

#[test]
fn test_views_status_shortcuts_thirty_two_exact_fetched_tips() {
    let world = World::new();
    world.bookmark_main();
    world.configure_packed_views(32);
    let exact_tip = world.upstream.head.to_string();
    for index in 0..32 {
        let remote = world
            .test_env
            .env_root()
            .join(format!("packed-view-{index}.git"));
        git(
            world.test_env.env_root(),
            &[
                "--git-dir",
                &World::store_path_in(world.test_env.env_root()).to_string_lossy(),
                "push",
                "--force",
                remote.to_str().expect("a utf8 path"),
                &format!("{exact_tip}:refs/heads/main"),
            ],
        );
    }

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--json"])
        .success();
    let value: serde_json::Value =
        serde_json::from_str(output.stdout.raw()).expect("valid status JSON");
    let views = value["views"].as_array().expect("a views array");
    assert_eq!(views.len(), 32, "every fetched view must be reported");
    assert!(
        views.iter().all(|view| {
            view["state"] == "up_to_date"
                && view["ahead"] == 0
                && view["behind"] == 0
                && view["elided"] == 0
        }),
        "every exact tip must take the zero-count current path: {value}"
    );
}

#[test]
fn test_views_status_does_not_treat_equal_trees_as_equal_history() {
    let world = World::new();
    world.bookmark_main();
    world.publish_upstream_commit("REVERTED.md", "add a file to revert");
    let work = world.test_env.env_root().join("upstream-work");
    git(&work, &["rm", "-q", "REVERTED.md"]);
    git(&work, &["commit", "-q", "-m", "revert the file"]);
    git(&work, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--json"])
        .success();
    let value: serde_json::Value =
        serde_json::from_str(output.stdout.raw()).expect("valid status JSON");
    let view = &value["views"][0];
    assert_ne!(
        view["state"], "up_to_date",
        "equal trees with change-then-revert history must run the exhaustive survey: {view}"
    );
    assert_eq!(
        view["published_commit"],
        world.published_tip().to_string(),
        "the survey must report the change-then-revert tip"
    );
}

#[test]
fn test_views_status_treats_an_integrated_topology_only_merge_as_current() {
    let world = World::new();
    world.bookmark_main();
    let host_merge = world.bookmark_topology_only_merge();
    let mut cache = Cache::new();
    let local = jj_views::derive(&world.store(), &host_merge, &filter(), &mut cache)
        .expect("the topology-only merge derives")
        .expect("the view has a tip");
    assert_ne!(
        local,
        world.published_tip(),
        "the merge must change topology"
    );
    let store = World::store_path_in(world.test_env.env_root());
    let local_tree = git(
        world.test_env.env_root(),
        &[
            "--git-dir",
            &store.to_string_lossy(),
            "rev-parse",
            &format!("{local}^{{tree}}"),
        ],
    );
    let published_tree = git(
        world.test_env.env_root(),
        &[
            "--git-dir",
            &store.to_string_lossy(),
            "rev-parse",
            &format!("{}^{{tree}}", world.published_tip()),
        ],
    );
    assert_eq!(local_tree, published_tree, "the view content must be equal");

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--json"])
        .success();
    let value: serde_json::Value =
        serde_json::from_str(output.stdout.raw()).expect("valid status JSON");
    let view = &value["views"][0];
    assert_eq!(view["state"], "up_to_date", "unexpected status: {view}");
    assert_eq!(
        view["ahead"], 0,
        "topology alone is not unpublished content"
    );
}

#[test]
fn test_views_status_treats_a_metadata_only_sibling_as_current() {
    let world = World::new();
    world.bookmark_main();
    let host_merge = world.bookmark_topology_only_merge();
    let mut cache = Cache::new();
    let local = jj_views::derive(&world.store(), &host_merge, &filter(), &mut cache)
        .expect("the topology-only merge derives")
        .expect("the view has a tip");
    let published_path = world.test_env.env_root().join("published.git");
    git(
        world.test_env.env_root(),
        &[
            "--git-dir",
            &World::store_path_in(world.test_env.env_root()).to_string_lossy(),
            "push",
            "-q",
            published_path.to_str().expect("a utf8 path"),
            &format!("{local}:refs/heads/main"),
        ],
    );

    let raw = world
        .store()
        .find_object(local)
        .expect("the local view commit")
        .detach()
        .data;
    let mut raw = String::from_utf8(raw).expect("fixture commits are utf8");
    let start = raw.find("\ncommitter ").expect("a committer line") + 1;
    let end = start + raw[start..].find('\n').expect("the committer line ends");
    raw.replace_range(
        start..end,
        "committer Published <published@example.invalid> 1 +0000",
    );
    let published = world.published();
    let sibling =
        gix::objs::Write::write_buf(&published.objects, gix::objs::Kind::Commit, raw.as_bytes())
            .expect("the sibling commit is writable");
    assert_ne!(sibling, local, "the metadata change must move the hash");
    published
        .reference(
            "refs/heads/main",
            sibling,
            gix::refs::transaction::PreviousValue::MustExistAndMatch(local.into()),
            "metadata-only sibling fixture",
        )
        .expect("the published branch moves to the sibling");

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--json"])
        .success();
    let value: serde_json::Value =
        serde_json::from_str(output.stdout.raw()).expect("valid status JSON");
    let view = &value["views"][0];
    assert_eq!(view["state"], "up_to_date", "unexpected status: {view}");
    assert_eq!(view["ahead"], 0, "metadata is not unpublished content");
    assert_eq!(view["behind"], 0, "metadata is not missing content");
}

#[test]
fn test_views_push_skips_an_integrated_topology_only_merge() {
    let world = World::new();
    world.bookmark_main();
    world.bookmark_topology_only_merge();

    let output = world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "views",
                "push",
                "--revision",
                "main",
                "--branch",
                "no-content",
            ],
        )
        .success();
    assert!(
        output.stderr.raw().contains("nothing pushed"),
        "push did not report its no-content outcome: {}",
        output.stderr.raw()
    );
    assert!(
        world
            .published()
            .try_find_reference("refs/heads/no-content")
            .expect("the published refs are readable")
            .is_none(),
        "push published topology with no content change"
    );
}

#[test]
fn test_views_status_fetches_a_missing_anchor_before_validation() {
    let world = World::new();
    world.bookmark_main();
    let source = world.git_id("vendored");
    world.test_env.add_config(format!(
        "[views.upstream.anchor]\nsource = \"{source}\"\nview = \"{}\"\n",
        world.upstream.head
    ));
    let store_path = World::store_path_in(world.test_env.env_root());
    git(
        world.test_env.env_root(),
        &[
            "--git-dir",
            store_path.to_str().expect("a utf8 path"),
            "prune",
            "--expire",
            "now",
        ],
    );
    assert!(
        world.store().find_object(world.upstream.head).is_err(),
        "the anchor object survived pruning, so status has nothing to fetch"
    );

    world
        .test_env
        .run_jj_in("monorepo", ["views", "status"])
        .success();
    assert!(
        world.store().find_object(world.upstream.head).is_ok(),
        "status did not fetch the anchor before validating it"
    );
}

#[test]
fn test_views_status_json_is_nonempty_and_names_its_scope() {
    let world = World::new();
    world.bookmark_main();

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--json"])
        .success();
    let value: serde_json::Value =
        serde_json::from_str(output.stdout.raw()).expect("valid status JSON");
    let views = value["views"].as_array().expect("a views array");
    assert_eq!(views.len(), 1, "every selected view must emit one record");
    let view = &views[0];
    assert_eq!(view["name"], "upstream");
    assert_eq!(view["path"], PREFIX);
    assert_eq!(view["state"], "up_to_date");
    assert_eq!(view["ahead"], 0);
    assert_eq!(view["behind"], 0);
    assert!(
        view["published_commit"].as_str().is_some(),
        "the comparison did not name the published commit: {view}"
    );
}

/// `status` answers for every view and moves nothing, which is what makes it
/// usable on a view `fetch` refuses.
#[test]
fn test_views_status_reports_a_diverged_view_without_failing() {
    let world = World::new();
    world.bookmark_main();
    diverge(&world);

    let before = world.git_id("main");
    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status"])
        .success();
    let stdout = output.stdout.raw().to_owned();
    assert!(stdout.contains("diverged"), "unexpected report: {stdout}");
    assert!(
        stdout.contains("jj views fetch") && stdout.contains("jj new"),
        "the report did not name the way out: {stdout}"
    );
    assert_eq!(world.git_id("main"), before, "status moved the bookmark");
}

/// The number that made ENG-11873 unreadable, reported before anyone has to
/// reach for `git rev-list`.
#[test]
fn test_views_status_counts_the_elided_commits() {
    let world = World::new();
    world.bookmark_main();
    world.commit_outside_the_prefix();
    world.publish_upstream_commit("LANDED.md", "a change that landed on main");
    world.publish_already_landed_merge();
    world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch"])
        .success();

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status"])
        .success();
    let stdout = output.stdout.raw().to_owned();
    assert!(
        stdout.contains("upstream: up to date"),
        "a fully fetched view was not reported as up to date: {stdout}"
    );
    assert!(
        stdout.contains("1 published commit is here and elided from the view"),
        "the elided count was not reported: {stdout}"
    );
}

/// Grows a commit under the view on each side, so neither contains the other.
fn diverge(world: &World) {
    let work_dir = world.test_env.work_dir("monorepo");
    work_dir.run_jj(["new", "main"]).success();
    work_dir.write_file(format!("{PREFIX}/LOCAL.md"), b"only here\n");
    work_dir
        .run_jj(["describe", "-m", "a local change to the view"])
        .success();
    work_dir
        .run_jj(["bookmark", "set", "main", "-r", "@"])
        .success();
    // Off the bookmark, so a command that moves it is not also moving the
    // working copy by accident.
    work_dir.run_jj(["new", "main"]).success();
    world.publish_upstream_commit("THEIRS.md", "published elsewhere");
}

/// The revision `jj views fetch` reports having brought in, read out of its
/// report the way a person would.
fn lifted_revision(report: &str) -> String {
    let marker = "`jj new main ";
    let at = report
        .find(marker)
        .unwrap_or_else(|| panic!("the report did not name a revision: {report}"));
    report[at + marker.len()..]
        .split('`')
        .next()
        .expect("a closing backtick")
        .trim()
        .to_owned()
}

/// The whole integration story, run as a person would run it: fetch a diverged
/// view, then `jj new` the two sides together. There is no `jj views` verb for
/// this on purpose, so the test uses jj's own commands and would catch the
/// surface being incomplete.
#[test]
fn test_a_diverged_view_is_integrated_with_jj_new() {
    let world = World::new();
    world.bookmark_main();
    // Monorepo work outside the prefix, so the merge can be checked for not
    // losing it. The published side is lifted onto the commit the two sides
    // last agreed on, which predates this, and a lift that took its tree from
    // the local tip instead would carry this file's absence back in.
    world.commit_outside_the_prefix();
    diverge(&world);

    let report = world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch"])
        .success()
        .stderr
        .raw()
        .to_owned();
    let arrived = lifted_revision(&report);

    let work_dir = world.test_env.work_dir("monorepo");
    work_dir.run_jj(["new", "main", &arrived]).success();
    work_dir
        .run_jj(["describe", "-m", "integrate the published view"])
        .success();
    work_dir
        .run_jj(["bookmark", "set", "main", "-r", "@"])
        .success();

    // Both sides' work is under the prefix.
    let files = file_list(&world, &["file", "list", "-r", "main", PREFIX]);
    for name in ["LOCAL.md", "THEIRS.md"] {
        assert!(
            files.contains(&format!("{PREFIX}/{name}")),
            "{name} is not under the prefix after integrating: {files}"
        );
    }

    // Monorepo work outside the prefix survived. The arrived lineage made no
    // change out there relative to the merge base, so a three way merge has to
    // keep it -- which is the property that makes lifting beside correct
    // rather than merely smaller.
    let all = file_list(&world, &["file", "list", "-r", "main"]);
    assert!(
        all.contains("OUTSIDE.md"),
        "integrating lost monorepo work outside the prefix: {all}"
    );

    // The published history is genuinely integrated rather than copied: its tip
    // is one of the view's own commits, which only holds if the lift kept its
    // hash.
    let head = world.git_id("main");
    let mut cache = Cache::new();
    let integrated = jj_views::verify::integrated(&world.store(), &head, &filter(), &mut cache)
        .expect("a derivation");
    assert!(
        integrated.contains(&world.published_tip()),
        "the published tip is not in the integrated view"
    );

    // Which leaves the view ahead, and `push` the next move.
    let status = world
        .test_env
        .run_jj_in("monorepo", ["views", "status"])
        .success();
    assert!(
        status.stdout.raw().contains("ahead"),
        "after integrating, the view should be ahead of the published repository: {}",
        status.stdout.raw()
    );
}

/// Fetching a diverged view twice must not leave two copies of the published
/// history, the same requirement ENG-11873 was about and a different mechanism.
///
/// It holds because a diverged view's commits never consult `onto`: every one
/// of their parents is either another incoming commit or the commit the two
/// sides last agreed on, which this repository already has. So the lift is a
/// pure function of the published history and produces the same ids on every
/// run, whatever the bookmark has done in between.
#[test]
fn test_fetching_a_diverged_view_twice_brings_in_one_copy() {
    let world = World::new();
    world.bookmark_main();
    diverge(&world);

    let first = lifted_revision(
        world
            .test_env
            .run_jj_in("monorepo", ["views", "fetch"])
            .success()
            .stderr
            .raw(),
    );
    let heads_after_first = world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "log",
                "--no-graph",
                "-r",
                "heads(all())",
                "-T",
                "commit_id ++ \"\n\"",
            ],
        )
        .success()
        .stdout
        .raw()
        .to_owned();

    let second = lifted_revision(
        world
            .test_env
            .run_jj_in("monorepo", ["views", "fetch"])
            .success()
            .stderr
            .raw(),
    );
    assert_eq!(
        first, second,
        "the second fetch of an unmoved diverged remote brought in a different revision"
    );
    let heads_after_second = world
        .test_env
        .run_jj_in(
            "monorepo",
            [
                "log",
                "--no-graph",
                "-r",
                "heads(all())",
                "-T",
                "commit_id ++ \"\n\"",
            ],
        )
        .success()
        .stdout
        .raw()
        .to_owned();
    assert_eq!(
        heads_after_first, heads_after_second,
        "the second fetch of an unmoved diverged remote added a head"
    );
}

/// A dry run of a diverged view reports and brings nothing in.
#[test]
fn test_views_fetch_dry_run_of_a_diverged_view_changes_nothing() {
    let world = World::new();
    world.bookmark_main();
    diverge(&world);

    let before = world
        .test_env
        .run_jj_in(
            "monorepo",
            ["log", "--no-graph", "-r", "heads(all())", "-T", "commit_id"],
        )
        .success()
        .stdout
        .raw()
        .to_owned();
    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch", "--dry-run"])
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("would bring 1 commit in beside main"),
        "unexpected report: {}",
        output.stderr.raw()
    );
    let after = world
        .test_env
        .run_jj_in(
            "monorepo",
            ["log", "--no-graph", "-r", "heads(all())", "-T", "commit_id"],
        )
        .success()
        .stdout
        .raw()
        .to_owned();
    assert_eq!(before, after, "a dry run brought commits in");
}

/// The prompt line is read on every shell prompt, so what it says has to
/// track what the surveys last learned: nothing before any survey beyond the
/// view's name, the counts of the last survey after one.
#[test]
fn test_views_prompt_reports_the_last_survey() {
    let world = World::new();
    world.bookmark_main();
    let inside = format!("monorepo/{PREFIX}");
    let prompt = ["views", "prompt", "--ignore-working-copy"];

    // Before anything surveys, the view is known but its position is not.
    let output = world.test_env.run_jj_in(&inside, prompt).success();
    assert_eq!(output.stdout.raw(), "upstream\n");

    // A fetch that lifts everything leaves the view current.
    world.publish_upstream_commit("NEWS.md", "published elsewhere");
    world
        .test_env
        .run_jj_in("monorepo", ["views", "fetch"])
        .success();
    let output = world.test_env.run_jj_in(&inside, prompt).success();
    assert_eq!(output.stdout.raw(), "upstream\t0\t0\n");

    // A survey that finds published commits records how far behind. `status
    // --no-fetch` is deliberate: the tracking ref, not the network, is what
    // the count is allowed to come from.
    world.publish_upstream_commit("MORE.md", "published again");
    world
        .test_env
        .run_jj_in("monorepo", ["views", "status"])
        .success();
    world.publish_upstream_commit("EVEN_MORE.md", "and again");
    world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--no-fetch"])
        .success();
    let output = world.test_env.run_jj_in(&inside, prompt).success();
    assert_eq!(output.stdout.raw(), "upstream\t1\t0\n");

    // Unpublished view commits count on the other side.
    let work_dir = world.test_env.work_dir("monorepo");
    work_dir.run_jj(["new", "main"]).success();
    work_dir.write_file(format!("{PREFIX}/LOCAL.md"), b"only here\n");
    work_dir
        .run_jj(["describe", "-m", "a local change to the view"])
        .success();
    work_dir
        .run_jj(["bookmark", "set", "main", "-r", "@"])
        .success();
    world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--no-fetch"])
        .success();
    let output = world.test_env.run_jj_in(&inside, prompt).success();
    assert_eq!(output.stdout.raw(), "upstream\t1\t1\n");
}

/// Outside every configured view the prompt has nothing to say, and says it
/// with an empty output rather than an error: the caller renders a prompt
/// segment, not a report.
#[test]
fn test_views_prompt_prints_nothing_outside_every_view() {
    let world = World::new();
    world.bookmark_main();

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "prompt", "--ignore-working-copy"])
        .success();
    assert_eq!(output.stdout.raw(), "");
}

/// A view name is a file name and a ref name, so a separator in one is a
/// write outside both namespaces, refused at config parse.
#[test]
fn test_views_refuse_a_name_with_a_path_separator() {
    let world = World::new();
    world.bookmark_main();
    world.test_env.add_config(
        "[views.\"evil/name\"]\npath = \"vendor\"\nremote = \"https://example.com/r.git\"\nbranch \
         = \"main\"\n",
    );

    let output = world
        .test_env
        .run_jj_in("monorepo", ["views", "status", "--no-fetch"]);
    insta::assert_snapshot!(output, @r"
    ------- stderr -------
    Config error: View name evil/name cannot contain a path separator
    For help, see https://docs.jj-vcs.dev/latest/config/ or use `jj help -k config`.
    [EOF]
    [exit status: 1]
    ");
}
