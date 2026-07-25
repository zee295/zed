use anyhow::Result;
use collections::HashMap;
use futures::future::{BoxFuture, FutureExt};
use git::blame::BlameEntry;
use git::repository::{
    AskPassDelegate, Branch, BranchesScanResult, CommitData, CommitDataReader, CommitDetails,
    CommitDiff, CommitFile, CommitOptions, CreateWorktreeTarget, DiffStatType, DiffType,
    FetchOptions, FileHistoryChangedFileSets, GitCommitTemplate, GitRepository,
    GitRepositoryCheckpoint, InitialGraphCommitData, LogOrder, LogSource, PushOptions, Remote,
    RemoteCommandOutput, RepoPath, ResetMode, SearchCommitArgs, Upstream, UpstreamTracking,
    UpstreamTrackingStatus, Worktree,
};
use git::stash::GitStash;
use git::status::{DiffTreeType, GitDiffStat, GitStatus, TreeDiff, parse_numstat};
use git::{Oid, RunHook};
use gpui::{AsyncApp, BackgroundExecutor, SharedString, Task};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use crate::transport::RemoteClient;

/// A remote implementation of `git::repository::GitRepository` that proxies
/// simple read/write operations over a WebSocket to a backend server.
///
/// Operations that require types that are not JSON-serializable today (for
/// example `CommitDiff`, `Blame`, or `AskPassDelegate`) currently return an
/// error. They can be promoted to remote calls as the protocol evolves.
pub struct RemoteGitRepository {
    client: RemoteClient,
    repo_path: PathBuf,
    executor: BackgroundExecutor,
    trusted: AtomicBool,
}

impl RemoteGitRepository {
    pub fn new(client: RemoteClient, repo_path: PathBuf, executor: BackgroundExecutor) -> Self {
        Self {
            client,
            repo_path,
            executor,
            trusted: AtomicBool::new(true),
        }
    }
}

#[derive(Deserialize)]
struct SystemTimeResponse {
    secs: u64,
    nanos: u32,
}

#[derive(Deserialize)]
struct RemoteCommandResponse {
    stdout: String,
    stderr: String,
}

#[derive(Deserialize)]
struct RemoteResponse {
    name: String,
}

#[derive(Deserialize)]
struct BranchResponse {
    is_head: bool,
    ref_name: String,
    upstream: Option<UpstreamResponse>,
    commit: Option<CommitResponse>,
}

#[derive(Deserialize)]
struct UpstreamResponse {
    ref_name: String,
    gone: bool,
    ahead: u32,
    behind: u32,
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
    subject: String,
    timestamp: i64,
    author_name: String,
    has_parent: bool,
}

#[derive(Deserialize)]
struct WorktreeResponse {
    path: String,
    ref_name: Option<String>,
    sha: String,
    is_main: bool,
    is_bare: bool,
}

#[derive(Deserialize)]
struct CommitDetailsResponse {
    sha: String,
    message: String,
    commit_timestamp: i64,
    author_email: String,
    author_name: String,
}

#[derive(Deserialize)]
struct CheckpointResponse {
    commit_sha: String,
}

#[derive(Deserialize)]
struct GraphCommitResponse {
    sha: String,
    parents: Vec<String>,
    ref_names: Vec<String>,
}

#[derive(Deserialize)]
struct CommitDataResponse {
    sha: String,
    parents: Vec<String>,
    author_name: String,
    author_email: String,
    commit_timestamp: i64,
    subject: String,
    message: String,
}

#[derive(Deserialize)]
struct CommitFileResponse {
    path: String,
    old_text: Option<String>,
    new_text: Option<String>,
    is_binary: bool,
}

#[derive(Deserialize)]
struct BlameResponse {
    entries: Vec<BlameEntry>,
    messages: HashMap<String, String>,
    tag_names: HashMap<String, Vec<String>>,
}

fn log_source_arg(source: LogSource) -> serde_json::Value {
    match source {
        LogSource::All => json!({ "kind": "all" }),
        LogSource::Branch(branch) => json!({ "kind": "branch", "value": branch }),
        LogSource::Sha(sha) => json!({ "kind": "sha", "value": sha.to_string() }),
        LogSource::Path(path) => json!({ "kind": "path", "value": repo_path_arg(&path) }),
    }
}

fn log_order_arg(order: LogOrder) -> &'static str {
    match order {
        LogOrder::DateOrder => "date",
        LogOrder::TopoOrder => "topo",
        LogOrder::AuthorDateOrder => "author_date",
        LogOrder::ReverseChronological => "reverse",
    }
}

impl From<RemoteCommandResponse> for RemoteCommandOutput {
    fn from(response: RemoteCommandResponse) -> Self {
        Self {
            stdout: response.stdout,
            stderr: response.stderr,
        }
    }
}

fn repo_path_arg(path: &RepoPath) -> String {
    path.as_std_path().to_string_lossy().to_string()
}

fn env_to_map(env: &Arc<HashMap<String, String>>) -> HashMap<String, String> {
    (**env).clone()
}

fn reset_mode_label(mode: ResetMode) -> &'static str {
    match mode {
        ResetMode::Soft => "soft",
        ResetMode::Mixed => "mixed",
    }
}

impl GitRepository for RemoteGitRepository {
    fn load_blob_content(&self, oid: git::Oid) -> BoxFuture<'_, Result<String>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call(
                    "GitRepository::load_blob_content",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "oid": oid.to_string(),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn set_index_text(
        &self,
        path: RepoPath,
        content: Option<String>,
        env: Arc<HashMap<String, String>>,
        is_executable: bool,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::set_index_text",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "path": repo_path_arg(&path),
                        "content": content,
                        "env": env_to_map(&env),
                        "is_executable": is_executable,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn remote_urls(&self) -> BoxFuture<'_, HashMap<String, String>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call(
                    "GitRepository::remote_urls",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await
                .unwrap_or_default()
        }
        .boxed()
    }

    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call(
                    "GitRepository::revparse_batch",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "revs": revs,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn load_revisions(&self, revisions: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call(
                    "GitRepository::load_revisions",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "revisions": revisions,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call(
                    "GitRepository::merge_message",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await
                .ok()
                .flatten()
        }
        .boxed()
    }

    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let path_prefixes: Vec<String> = path_prefixes.iter().map(repo_path_arg).collect();
        self.executor.spawn(async move {
            let raw: String = client
                .call(
                    "GitRepository::status",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "path_prefixes": path_prefixes,
                    }),
                )
                .await?;
            raw.parse()
        })
    }

    fn diff_tree(&self, request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let (kind, base, head) = match request {
            DiffTreeType::MergeBase { base, head } => ("merge_base", base, head),
            DiffTreeType::Since { base, head } => ("since", base, head),
        };
        async move {
            let raw: String = client
                .call(
                    "GitRepository::diff_tree",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "kind": kind,
                        "base": base,
                        "head": head,
                    }),
                )
                .await?;
            raw.parse()
        }
        .boxed()
    }

    fn stash_entries(&self) -> BoxFuture<'static, Result<GitStash>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let raw: String = client
                .call(
                    "GitRepository::stash_entries",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await?;
            raw.parse()
        }
        .boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let branches: Vec<BranchResponse> = client
                .call(
                    "GitRepository::branches",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await?;
            Ok(BranchesScanResult {
                branches: branches
                    .into_iter()
                    .map(|branch| Branch {
                        is_head: branch.is_head,
                        ref_name: branch.ref_name.into(),
                        upstream: branch.upstream.map(|upstream| Upstream {
                            ref_name: upstream.ref_name.into(),
                            tracking: if upstream.gone {
                                UpstreamTracking::Gone
                            } else {
                                UpstreamTrackingStatus {
                                    ahead: upstream.ahead,
                                    behind: upstream.behind,
                                }
                                .into()
                            },
                        }),
                        most_recent_commit: branch.commit.map(|commit| {
                            git::repository::CommitSummary {
                                sha: commit.sha.into(),
                                subject: commit.subject.into(),
                                commit_timestamp: commit.timestamp,
                                author_name: commit.author_name.into(),
                                has_parent: commit.has_parent,
                            }
                        }),
                    })
                    .collect(),
                error: None,
            })
        }
        .boxed()
    }

    fn change_branch(&self, name: String) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::change_branch",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "name": name,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn create_branch(
        &self,
        name: String,
        base_branch: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::create_branch",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "name": name,
                        "base_branch": base_branch,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn rename_branch(&self, branch: String, new_name: String) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::rename_branch",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "branch": branch,
                        "new_name": new_name,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn delete_branch(
        &self,
        is_remote: bool,
        name: String,
        force: bool,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::delete_branch",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "is_remote": is_remote,
                        "name": name,
                        "force": force,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn worktrees(&self) -> BoxFuture<'_, Result<Vec<Worktree>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let worktrees: Vec<WorktreeResponse> = client
                .call(
                    "GitRepository::worktrees",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await?;
            Ok(worktrees
                .into_iter()
                .map(|worktree| Worktree {
                    path: PathBuf::from(worktree.path),
                    ref_name: worktree.ref_name.map(Into::into),
                    sha: worktree.sha.into(),
                    is_main: worktree.is_main,
                    is_bare: worktree.is_bare,
                })
                .collect())
        }
        .boxed()
    }

    fn worktree_created_at(
        &self,
        worktree_path: PathBuf,
    ) -> BoxFuture<'_, Result<Option<SystemTime>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let response: Option<SystemTimeResponse> = client
                .call(
                    "GitRepository::worktree_created_at",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "worktree_path": worktree_path.to_string_lossy(),
                    }),
                )
                .await?;
            Ok(response.map(|t| SystemTime::UNIX_EPOCH + Duration::new(t.secs, t.nanos)))
        }
        .boxed()
    }

    fn create_worktree(
        &self,
        target: CreateWorktreeTarget,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let target = match target {
            CreateWorktreeTarget::ExistingBranch { branch_name } => {
                json!({ "kind": "existing", "branch_name": branch_name })
            }
            CreateWorktreeTarget::NewBranch {
                branch_name,
                base_sha,
            } => json!({
                "kind": "new",
                "branch_name": branch_name,
                "base_sha": base_sha,
            }),
            CreateWorktreeTarget::Detached { base_sha } => {
                json!({ "kind": "detached", "base_sha": base_sha })
            }
        };
        async move {
            client
                .call_void(
                    "GitRepository::create_worktree",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "target": target,
                        "path": path.to_string_lossy(),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn checkout_branch_in_worktree(
        &self,
        branch_name: String,
        worktree_path: PathBuf,
        create: bool,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::checkout_branch_in_worktree",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "branch_name": branch_name,
                        "worktree_path": worktree_path.to_string_lossy(),
                        "create": create,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn remove_worktree(&self, path: PathBuf, force: bool) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::remove_worktree",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "path": path.to_string_lossy(),
                        "force": force,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn rename_worktree(&self, old_path: PathBuf, new_path: PathBuf) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::rename_worktree",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "old_path": old_path.to_string_lossy(),
                        "new_path": new_path.to_string_lossy(),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn reset(
        &self,
        commit: String,
        mode: ResetMode,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::reset",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "commit": commit,
                        "mode": reset_mode_label(mode),
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn checkout_files(
        &self,
        commit: String,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let paths: Vec<String> = paths.into_iter().map(|path| repo_path_arg(&path)).collect();
        async move {
            client
                .call_void(
                    "GitRepository::checkout_files",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "commit": commit,
                        "paths": paths,
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let details: CommitDetailsResponse = client
                .call(
                    "GitRepository::show",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "commit": commit,
                    }),
                )
                .await?;
            Ok(CommitDetails {
                sha: details.sha.into(),
                message: details.message.into(),
                commit_timestamp: details.commit_timestamp,
                author_email: details.author_email.into(),
                author_name: details.author_name.into(),
            })
        }
        .boxed()
    }

    fn load_commit(&self, commit: String, _cx: AsyncApp) -> BoxFuture<'_, Result<CommitDiff>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let files: Vec<CommitFileResponse> = client
                .call(
                    "GitRepository::load_commit",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "commit": commit,
                    }),
                )
                .await?;
            Ok(CommitDiff {
                files: files
                    .into_iter()
                    .map(|file| {
                        Ok(CommitFile {
                            path: RepoPath::new(file.path.as_str())?,
                            old_text: file.old_text,
                            new_text: file.new_text,
                            is_binary: file.is_binary,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            })
        }
        .boxed()
    }

    fn blame(
        &self,
        path: RepoPath,
        content: rope::Rope,
        line_ending: text::LineEnding,
    ) -> BoxFuture<'_, Result<git::blame::Blame>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let path = repo_path_arg(&path);
        let content = text::chunks_with_line_ending(&content, line_ending).collect::<String>();
        async move {
            let response: BlameResponse = client
                .call(
                    "GitRepository::blame",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "path": path,
                        "content": content,
                    }),
                )
                .await?;
            Ok(git::blame::Blame {
                entries: response.entries,
                messages: response
                    .messages
                    .into_iter()
                    .map(|(sha, message)| Ok((sha.parse::<Oid>()?, message)))
                    .collect::<Result<HashMap<_, _>>>()?,
                tag_names: response
                    .tag_names
                    .into_iter()
                    .map(|(sha, names)| Ok((sha.parse::<Oid>()?, names)))
                    .collect::<Result<HashMap<_, _>>>()?,
            })
        }
        .boxed()
    }

    fn path(&self) -> PathBuf {
        self.repo_path.clone()
    }

    fn main_repository_path(&self) -> PathBuf {
        self.repo_path.clone()
    }

    fn stage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let paths: Vec<String> = paths.into_iter().map(|path| repo_path_arg(&path)).collect();
        async move {
            client
                .call_void(
                    "GitRepository::stage_paths",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "paths": paths,
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn unstage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let paths: Vec<String> = paths.into_iter().map(|path| repo_path_arg(&path)).collect();
        async move {
            client
                .call_void(
                    "GitRepository::unstage_paths",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "paths": paths,
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn run_hook(
        &self,
        hook: RunHook,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::run_hook",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "hook": hook.as_str(),
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn commit(
        &self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        _askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let identity = name_and_email.map(|(name, email)| (name.to_string(), email.to_string()));
        async move {
            client
                .call_void(
                    "GitRepository::commit",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "message": message.to_string(),
                        "name_and_email": identity,
                        "options": {
                            "amend": options.amend,
                            "signoff": options.signoff,
                            "allow_empty": options.allow_empty,
                        },
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn stash_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let paths: Vec<String> = paths.into_iter().map(|path| repo_path_arg(&path)).collect();
        async move {
            client
                .call_void(
                    "GitRepository::stash_paths",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "paths": paths,
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn stash_pop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::stash_pop",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "index": index,
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn stash_apply(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::stash_apply",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "index": index,
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn stash_drop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::stash_drop",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "index": index,
                        "env": env_to_map(&env),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn push(
        &self,
        branch_name: String,
        remote_branch_name: String,
        upstream_name: String,
        options: Option<PushOptions>,
        _askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let option = options.map(|option| match option {
            PushOptions::SetUpstream => "set_upstream",
            PushOptions::Force => "force",
        });
        async move {
            let response: RemoteCommandResponse = client
                .call(
                    "GitRepository::push",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "branch_name": branch_name,
                        "remote_branch_name": remote_branch_name,
                        "upstream_name": upstream_name,
                        "option": option,
                        "env": env_to_map(&env),
                    }),
                )
                .await?;
            Ok(response.into())
        }
        .boxed()
    }

    fn pull(
        &self,
        branch_name: Option<String>,
        upstream_name: String,
        rebase: bool,
        _askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let response: RemoteCommandResponse = client
                .call(
                    "GitRepository::pull",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "branch_name": branch_name,
                        "upstream_name": upstream_name,
                        "rebase": rebase,
                        "env": env_to_map(&env),
                    }),
                )
                .await?;
            Ok(response.into())
        }
        .boxed()
    }

    fn fetch(
        &self,
        fetch_options: FetchOptions,
        _askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let remote = fetch_options.to_proto();
        async move {
            let response: RemoteCommandResponse = client
                .call(
                    "GitRepository::fetch",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "remote": remote,
                        "env": env_to_map(&env),
                    }),
                )
                .await?;
            Ok(response.into())
        }
        .boxed()
    }

    fn get_push_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let remote: Option<RemoteResponse> = client
                .call(
                    "GitRepository::get_push_remote",
                    &json!({ "repo_path": repo_path.to_string_lossy(), "branch": branch }),
                )
                .await?;
            Ok(remote.map(|remote| Remote {
                name: remote.name.into(),
            }))
        }
        .boxed()
    }

    fn get_branch_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let remote: Option<RemoteResponse> = client
                .call(
                    "GitRepository::get_branch_remote",
                    &json!({ "repo_path": repo_path.to_string_lossy(), "branch": branch }),
                )
                .await?;
            Ok(remote.map(|remote| Remote {
                name: remote.name.into(),
            }))
        }
        .boxed()
    }

    fn get_all_remotes(&self) -> BoxFuture<'_, Result<Vec<Remote>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let remotes: Vec<RemoteResponse> = client
                .call(
                    "GitRepository::get_all_remotes",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await?;
            Ok(remotes
                .into_iter()
                .map(|remote| Remote {
                    name: remote.name.into(),
                })
                .collect())
        }
        .boxed()
    }

    fn remove_remote(&self, name: String) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::remove_remote",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "name": name,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn create_remote(&self, name: String, url: String) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::create_remote",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "name": name,
                        "url": url,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn check_for_pushed_commit(&self) -> BoxFuture<'_, Result<Vec<SharedString>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let branches: Vec<String> = client
                .call(
                    "GitRepository::check_for_pushed_commit",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await?;
            Ok(branches.into_iter().map(SharedString::from).collect())
        }
        .boxed()
    }

    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let (kind, base_ref) = match diff {
            DiffType::HeadToIndex => ("head_to_index", None),
            DiffType::HeadToWorktree => ("head_to_worktree", None),
            DiffType::MergeBase { base_ref } => ("merge_base", Some(base_ref.to_string())),
        };
        async move {
            client
                .call(
                    "GitRepository::diff",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "kind": kind,
                        "base_ref": base_ref,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn diff_stat(
        &self,
        diff: DiffStatType,
        path_prefixes: &[RepoPath],
    ) -> BoxFuture<'static, Result<GitDiffStat>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let kind = match diff {
            DiffStatType::HeadToIndex => "head_to_index",
            DiffStatType::HeadToWorktree => "head_to_worktree",
            DiffStatType::IndexToWorktree => "index_to_worktree",
        };
        let path_prefixes: Vec<String> = path_prefixes.iter().map(repo_path_arg).collect();
        async move {
            let raw: String = client
                .call(
                    "GitRepository::diff_stat",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "kind": kind,
                        "path_prefixes": path_prefixes,
                    }),
                )
                .await?;
            Ok(parse_numstat(&raw))
        }
        .boxed()
    }

    fn checkpoint(&self) -> BoxFuture<'static, Result<GitRepositoryCheckpoint>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let response: CheckpointResponse = client
                .call(
                    "GitRepository::checkpoint",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await?;
            Ok(GitRepositoryCheckpoint {
                commit_sha: response.commit_sha.parse()?,
            })
        }
        .boxed()
    }

    fn restore_checkpoint(&self, checkpoint: GitRepositoryCheckpoint) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::restore_checkpoint",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "commit_sha": checkpoint.commit_sha.to_string(),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn create_archive_checkpoint(&self) -> BoxFuture<'_, Result<(String, String)>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call(
                    "GitRepository::create_archive_checkpoint",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await
        }
        .boxed()
    }

    fn restore_archive_checkpoint(
        &self,
        staged_sha: String,
        unstaged_sha: String,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::restore_archive_checkpoint",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "staged_sha": staged_sha,
                        "unstaged_sha": unstaged_sha,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn compare_checkpoints(
        &self,
        left: GitRepositoryCheckpoint,
        right: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<bool>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call(
                    "GitRepository::compare_checkpoints",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "left": left.commit_sha.to_string(),
                        "right": right.commit_sha.to_string(),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn diff_checkpoints(
        &self,
        base_checkpoint: GitRepositoryCheckpoint,
        target_checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<String>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call(
                    "GitRepository::diff_checkpoints",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "base": base_checkpoint.commit_sha.to_string(),
                        "target": target_checkpoint.commit_sha.to_string(),
                    }),
                )
                .await
        }
        .boxed()
    }

    fn load_commit_template(&self) -> BoxFuture<'_, Result<Option<GitCommitTemplate>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call(
                    "GitRepository::load_commit_template",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await
        }
        .boxed()
    }

    fn default_branch(
        &self,
        include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let branch: Option<String> = client
                .call(
                    "GitRepository::default_branch",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "include_remote_name": include_remote_name,
                    }),
                )
                .await?;
            Ok(branch.map(SharedString::from))
        }
        .boxed()
    }

    fn initial_graph_data(
        &self,
        log_source: LogSource,
        log_order: LogOrder,
        request_tx: async_channel::Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let commits: Vec<GraphCommitResponse> = client
                .call(
                    "GitRepository::initial_graph_data",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "source": log_source_arg(log_source),
                        "order": log_order_arg(log_order),
                    }),
                )
                .await?;
            for chunk in commits.chunks(git::repository::GRAPH_CHUNK_SIZE) {
                let values = chunk
                    .iter()
                    .map(|commit| {
                        Ok(Arc::new(InitialGraphCommitData {
                            sha: commit.sha.parse()?,
                            parents: commit
                                .parents
                                .iter()
                                .filter_map(|parent| parent.parse().ok())
                                .collect(),
                            ref_names: commit.ref_names.iter().cloned().map(Into::into).collect(),
                        }))
                    })
                    .collect::<Result<Vec<_>>>()?;
                if request_tx.send(values).await.is_err() {
                    break;
                }
            }
            Ok(())
        }
        .boxed()
    }

    fn search_commits(
        &self,
        log_source: LogSource,
        search_args: SearchCommitArgs,
        request_tx: async_channel::Sender<git::Oid>,
    ) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            let commits: Vec<String> = client
                .call(
                    "GitRepository::search_commits",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "source": log_source_arg(log_source),
                        "query": search_args.query,
                        "case_sensitive": search_args.case_sensitive,
                    }),
                )
                .await?;
            for commit in commits {
                if let Ok(oid) = commit.parse()
                    && request_tx.send(oid).await.is_err()
                {
                    break;
                }
            }
            Ok(())
        }
        .boxed()
    }

    fn file_history_changed_files(
        &self,
        paths: Vec<RepoPath>,
        commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<FileHistoryChangedFileSets>>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        let paths: Vec<String> = paths.into_iter().map(|path| repo_path_arg(&path)).collect();
        async move {
            let histories: Vec<Vec<Vec<String>>> = client
                .call(
                    "GitRepository::file_history_changed_files",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "paths": paths,
                        "commit_limit": commit_limit,
                    }),
                )
                .await?;
            histories
                .into_iter()
                .map(|file_sets| {
                    Ok(FileHistoryChangedFileSets {
                        file_sets: file_sets
                            .into_iter()
                            .map(|files| {
                                files
                                    .into_iter()
                                    .map(|file| RepoPath::new(file.as_str()))
                                    .collect::<Result<Vec<_>>>()
                            })
                            .collect::<Result<Vec<_>>>()?,
                    })
                })
                .collect()
        }
        .boxed()
    }

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        Ok(CommitDataReader::from_async_resolver(
            self.executor.clone(),
            move |sha| {
                let client = client.clone();
                let repo_path = repo_path.clone();
                async move {
                    let commit: CommitDataResponse = client
                        .call(
                            "GitRepository::commit_data",
                            &json!({
                                "repo_path": repo_path.to_string_lossy(),
                                "sha": sha.to_string(),
                            }),
                        )
                        .await?;
                    Ok(CommitData {
                        sha: commit.sha.parse()?,
                        parents: commit
                            .parents
                            .into_iter()
                            .filter_map(|parent| parent.parse().ok())
                            .collect(),
                        author_name: commit.author_name.into(),
                        author_email: commit.author_email.into(),
                        commit_timestamp: commit.commit_timestamp,
                        subject: commit.subject.into(),
                        message: commit.message.into(),
                    })
                }
                .boxed()
            },
        ))
    }

    fn update_ref(&self, ref_name: String, commit: String) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::update_ref",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "ref_name": ref_name,
                        "commit": commit,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn delete_ref(&self, ref_name: String) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::delete_ref",
                    &json!({
                        "repo_path": repo_path.to_string_lossy(),
                        "ref_name": ref_name,
                    }),
                )
                .await
        }
        .boxed()
    }

    fn repair_worktrees(&self) -> BoxFuture<'_, Result<()>> {
        let client = self.client.clone();
        let repo_path = self.repo_path.clone();
        async move {
            client
                .call_void(
                    "GitRepository::repair_worktrees",
                    &json!({ "repo_path": repo_path.to_string_lossy() }),
                )
                .await
        }
        .boxed()
    }

    fn set_trusted(&self, trusted: bool) {
        self.trusted.store(trusted, Ordering::SeqCst);
    }

    fn is_trusted(&self) -> bool {
        self.trusted.load(Ordering::SeqCst)
    }
}
