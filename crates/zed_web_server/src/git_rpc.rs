use std::{
    collections::HashMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::fs_rpc::FsRpc;

pub fn handles(method: &str) -> bool {
    method.starts_with("GitRepository::")
}

pub fn dispatch(fs: &FsRpc, method: &str, params: &Value) -> Result<Value> {
    match method {
        "GitRepository::revparse_batch" => revparse_batch(fs, params),
        "GitRepository::load_revisions" => load_revisions(fs, params),
        "GitRepository::load_commit_template" => load_commit_template(fs, params),
        "GitRepository::load_blob_content" => Ok(Value::String(git_text(
            fs,
            params,
            &["cat-file", "-p", string(params, "oid")],
        )?)),
        "GitRepository::status" => status(fs, params),
        "GitRepository::diff_tree" => diff_tree(fs, params),
        "GitRepository::diff_stat" => diff_stat(fs, params),
        "GitRepository::stash_entries" => Ok(Value::String(git_text(
            fs,
            params,
            &["stash", "list", "--pretty=format:%gd%x00%H%x00%ct%x00%s"],
        )?)),
        "GitRepository::branches" => branches(fs, params),
        "GitRepository::worktrees" => worktrees(fs, params),
        "GitRepository::create_worktree" => create_worktree(fs, params),
        "GitRepository::run_hook" => run_hook(fs, params),
        "GitRepository::checkpoint" => checkpoint(fs, params),
        "GitRepository::restore_checkpoint" => restore_checkpoint(fs, params),
        "GitRepository::create_archive_checkpoint" => create_archive_checkpoint(fs, params),
        "GitRepository::restore_archive_checkpoint" => restore_archive_checkpoint(fs, params),
        "GitRepository::compare_checkpoints" => compare_checkpoints(fs, params),
        "GitRepository::diff_checkpoints" => diff_checkpoints(fs, params),
        "GitRepository::file_history_changed_files" => file_history_changed_files(fs, params),
        "GitRepository::commit_data" => commit_data(fs, params),
        "GitRepository::load_commit" => load_commit(fs, params),
        "GitRepository::blame" => blame(fs, params),
        "GitRepository::remote_urls" => remote_urls(fs, params),
        "GitRepository::merge_message" => merge_message(fs, params),
        "GitRepository::show" => show(fs, params),
        "GitRepository::set_index_text" => set_index_text(fs, params),
        "GitRepository::change_branch" => change_branch(fs, params),
        "GitRepository::create_branch" => create_branch(fs, params),
        "GitRepository::rename_branch" => rename_branch(fs, params),
        "GitRepository::delete_branch" => delete_branch(fs, params),
        "GitRepository::worktree_created_at" => worktree_created_at(fs, params),
        "GitRepository::checkout_branch_in_worktree" => checkout_branch_in_worktree(fs, params),
        "GitRepository::remove_worktree" => remove_worktree(fs, params),
        "GitRepository::rename_worktree" => rename_worktree(fs, params),
        "GitRepository::reset" => reset(fs, params),
        "GitRepository::checkout_files" => checkout_files(fs, params),
        "GitRepository::stage_paths" => paths_command(fs, params, &["add", "--all", "--"]),
        "GitRepository::unstage_paths" => paths_command(fs, params, &["reset", "--"]),
        "GitRepository::stash_paths" => {
            paths_command(fs, params, &["stash", "push", "--keep-index", "--"])
        }
        "GitRepository::stash_pop" => stash(fs, params, "pop"),
        "GitRepository::stash_apply" => stash(fs, params, "apply"),
        "GitRepository::stash_drop" => stash(fs, params, "drop"),
        "GitRepository::remove_remote" => remove_remote(fs, params),
        "GitRepository::create_remote" => create_remote(fs, params),
        "GitRepository::check_for_pushed_commit" => check_for_pushed_commit(fs, params),
        "GitRepository::diff" => diff(fs, params),
        "GitRepository::default_branch" => default_branch(fs, params),
        "GitRepository::update_ref" => update_ref(fs, params),
        "GitRepository::delete_ref" => delete_ref(fs, params),
        "GitRepository::repair_worktrees" => null_command(fs, params, &["worktree", "repair"]),
        "GitRepository::commit" => commit(fs, params),
        "GitRepository::push" => push(fs, params),
        "GitRepository::pull" => pull(fs, params),
        "GitRepository::fetch" => fetch(fs, params),
        "GitRepository::get_branch_remote" => get_branch_remote(fs, params),
        "GitRepository::get_push_remote" => get_push_remote(fs, params),
        "GitRepository::get_all_remotes" => get_all_remotes(fs, params),
        "GitRepository::initial_graph_data" => initial_graph_data(fs, params),
        "GitRepository::search_commits" => search_commits(fs, params),
        _ => bail!("unknown git method: {method}"),
    }
}

fn work_directory(fs: &FsRpc, params: &Value) -> Result<PathBuf> {
    let path = fs.path(
        params
            .get("repo_path")
            .and_then(Value::as_str)
            .unwrap_or("/workspace"),
    )?;
    Ok(if path.file_name().is_some_and(|name| name == ".git") {
        path.parent().unwrap_or(&path).to_path_buf()
    } else if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(&path).to_path_buf()
    })
}

fn git(fs: &FsRpc, params: &Value, args: &[&str]) -> Result<Output> {
    git_command(fs, params, args, None, true)
}

fn git_command(
    fs: &FsRpc,
    params: &Value,
    args: &[&str],
    input: Option<&[u8]>,
    check: bool,
) -> Result<Output> {
    let directory = work_directory(fs, params)?;
    let mut command = Command::new("git");
    command.args(args).current_dir(&directory);
    for (key, value) in environment(params) {
        command.env(key, value);
    }
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("running git in {}", directory.display()))?;
    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if check && !output.status.success() {
        bail!(
            "{}",
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        );
    }
    Ok(output)
}

fn environment(params: &Value) -> HashMap<String, String> {
    match params.get("env") {
        Some(Value::Object(values)) => values
            .iter()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
            .collect(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                Some((
                    pair.first()?.as_str()?.to_string(),
                    pair.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect(),
        _ => HashMap::new(),
    }
}

fn git_text(fs: &FsRpc, params: &Value, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(fs, params, args)?.stdout).context("git output is not UTF-8")?)
}

fn string<'a>(params: &'a Value, key: &str) -> &'a str {
    params.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn string_array(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

fn revparse_batch(fs: &FsRpc, params: &Value) -> Result<Value> {
    let results = string_array(params, "revs")
        .into_iter()
        .map(|revision| {
            git(fs, params, &["rev-parse", "--verify", &revision])
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|value| Value::String(value.trim().to_string()))
                .unwrap_or(Value::Null)
        })
        .collect();
    Ok(Value::Array(results))
}

fn load_revisions(fs: &FsRpc, params: &Value) -> Result<Value> {
    let results = string_array(params, "revisions")
        .into_iter()
        .map(|revision| {
            let is_blob = git(fs, params, &["cat-file", "-t", &revision])
                .ok()
                .is_some_and(|output| output.stdout == b"blob\n");
            if !is_blob {
                return Value::Null;
            }
            git(fs, params, &["cat-file", "-p", &revision])
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(Value::String)
                .unwrap_or(Value::Null)
        })
        .collect();
    Ok(Value::Array(results))
}

fn load_commit_template(fs: &FsRpc, params: &Value) -> Result<Value> {
    let Ok(configured) = git_text(fs, params, &["config", "--get", "commit.template"]) else {
        return Ok(Value::Null);
    };
    let raw_path = configured.trim();
    if raw_path.is_empty() {
        return Ok(Value::Null);
    }
    let path = if let Some(rest) = raw_path.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest)
    } else {
        let path = PathBuf::from(raw_path);
        if path.is_absolute() {
            path
        } else {
            work_directory(fs, params)?.join(path)
        }
    };
    let Ok(template) = std::fs::read_to_string(path) else {
        return Ok(Value::Null);
    };
    if template.trim().is_empty() {
        Ok(Value::Null)
    } else {
        Ok(json!({"template": template}))
    }
}

fn status(fs: &FsRpc, params: &Value) -> Result<Value> {
    let prefixes = string_array(params, "path_prefixes");
    let mut owned = vec![
        "status".to_string(),
        "--porcelain=v1".to_string(),
        "--untracked-files=all".to_string(),
        "--no-renames".to_string(),
        "-z".to_string(),
        "--".to_string(),
    ];
    owned.extend(prefixes.into_iter().map(|path| {
        if path.is_empty() {
            ".".to_string()
        } else {
            path
        }
    }));
    let args = owned.iter().map(String::as_str).collect::<Vec<_>>();
    Ok(Value::String(git_text(fs, params, &args)?))
}

fn diff_tree(fs: &FsRpc, params: &Value) -> Result<Value> {
    let mut args = vec!["diff-tree", "-r", "-z", "--no-renames"];
    if string(params, "kind") == "merge_base" {
        args.push("--merge-base");
    }
    args.extend([string(params, "base"), string(params, "head")]);
    Ok(Value::String(git_text(fs, params, &args)?))
}

fn diff_stat(fs: &FsRpc, params: &Value) -> Result<Value> {
    let mut owned = vec![
        "diff".to_string(),
        "--numstat".to_string(),
        "--no-renames".to_string(),
    ];
    match string(params, "kind") {
        "head_to_index" => owned.extend(["--cached".to_string(), "HEAD".to_string()]),
        "head_to_worktree" => owned.push("HEAD".to_string()),
        _ => {}
    }
    let prefixes = string_array(params, "path_prefixes");
    if !prefixes.is_empty() {
        owned.push("--".to_string());
        owned.extend(prefixes);
    }
    let args = owned.iter().map(String::as_str).collect::<Vec<_>>();
    Ok(Value::String(git_text(fs, params, &args)?))
}

fn branches(fs: &FsRpc, params: &Value) -> Result<Value> {
    let fields = "%(HEAD)%00%(objectname)%00%(parent)%00%(refname)%00%(upstream)%00%(upstream:track)%00%(committerdate:unix)%00%(authorname)%00%(contents:subject)";
    let output = git_text(
        fs,
        params,
        &[
            "for-each-ref",
            "refs/heads/**/*",
            "refs/remotes/**/*",
            "--format",
            fields,
        ],
    )?;
    let mut result = output
        .lines()
        .filter_map(|line| {
            let fields = line.split('\0').collect::<Vec<_>>();
            if fields.len() != 9 {
                return None;
            }
            let upstream = (!fields[4].is_empty()).then(|| {
                json!({
                    "ref_name": fields[4],
                    "gone": fields[5].contains("gone"),
                    "ahead": tracking_count(fields[5], "ahead"),
                    "behind": tracking_count(fields[5], "behind"),
                })
            });
            Some(json!({
                "is_head": fields[0] == "*",
                "ref_name": fields[3],
                "upstream": upstream,
                "commit": {
                    "sha": fields[1],
                    "subject": fields[8],
                    "timestamp": fields[6].parse::<i64>().unwrap_or_default(),
                    "author_name": fields[7],
                    "has_parent": !fields[2].is_empty(),
                }
            }))
        })
        .collect::<Vec<_>>();
    if result.is_empty()
        && let Ok(output) = git(fs, params, &["symbolic-ref", "--quiet", "HEAD"])
        && output.status.success()
    {
        result.push(json!({
            "is_head": true,
            "ref_name": String::from_utf8_lossy(&output.stdout).trim(),
            "upstream": null,
            "commit": null,
        }));
    }
    Ok(Value::Array(result))
}

fn tracking_count(value: &str, label: &str) -> u64 {
    value
        .split([' ', '[', ']', ','])
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|pair| (pair[0] == label).then(|| pair[1].parse().ok()).flatten())
        .unwrap_or_default()
}

fn worktrees(fs: &FsRpc, params: &Value) -> Result<Value> {
    let output = git_text(fs, params, &["worktree", "list", "--porcelain"])?;
    let entries = output
        .trim()
        .split("\n\n")
        .enumerate()
        .filter_map(|(index, block)| {
            let mut values = HashMap::new();
            let mut bare = false;
            for line in block.lines() {
                let (key, value) = line.split_once(' ').unwrap_or((line, ""));
                if key == "bare" {
                    bare = true;
                } else {
                    values.insert(key, value);
                }
            }
            Some(json!({
                "path": values.get("worktree")?,
                "ref_name": values.get("branch"),
                "sha": values.get("HEAD").copied().unwrap_or_default(),
                "is_main": index == 0,
                "is_bare": bare,
            }))
        })
        .collect();
    Ok(Value::Array(entries))
}

fn create_worktree(fs: &FsRpc, params: &Value) -> Result<Value> {
    let path = fs.path(string(params, "path"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let target = params.get("target").and_then(Value::as_object);
    let kind = target
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let value = |key| {
        target
            .and_then(|target| target.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let path = path.to_string_lossy().to_string();
    let args = match kind {
        "existing" => vec![
            "worktree".to_string(),
            "add".to_string(),
            "--".to_string(),
            path,
            value("branch_name").to_string(),
        ],
        "new" => vec![
            "worktree".to_string(),
            "add".to_string(),
            "-b".to_string(),
            value("branch_name").to_string(),
            "--".to_string(),
            path,
            target
                .and_then(|target| target.get("base_sha"))
                .and_then(Value::as_str)
                .unwrap_or("HEAD")
                .to_string(),
        ],
        "detached" => vec![
            "worktree".to_string(),
            "add".to_string(),
            "--detach".to_string(),
            "--".to_string(),
            path,
            target
                .and_then(|target| target.get("base_sha"))
                .and_then(Value::as_str)
                .unwrap_or("HEAD")
                .to_string(),
        ],
        _ => bail!("unknown worktree target: {kind}"),
    };
    null_owned_command(fs, params, args)
}

fn run_hook(fs: &FsRpc, params: &Value) -> Result<Value> {
    let hook = string(params, "hook");
    if hook != "pre-commit" {
        bail!("unsupported git hook: {hook}");
    }
    null_command(fs, params, &["hook", "run", "--ignore-missing", hook])
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temporary_index(git_dir: &Path, label: &str) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    git_dir.join(format!(
        "zed-web-{label}-{}-{sequence}.index",
        std::process::id()
    ))
}

fn git_dir(fs: &FsRpc, params: &Value) -> Result<PathBuf> {
    let raw = git_text(fs, params, &["rev-parse", "--git-dir"])?;
    let path = PathBuf::from(raw.trim());
    Ok(if path.is_absolute() {
        path
    } else {
        work_directory(fs, params)?.join(path)
    })
}

fn checkpoint_params(params: &Value, index: &Path) -> Value {
    let mut value = params.clone();
    let object = value.as_object_mut().expect("RPC params must be an object");
    let mut env = environment(params)
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect::<serde_json::Map<_, _>>();
    env.insert(
        "GIT_INDEX_FILE".to_string(),
        Value::String(index.to_string_lossy().to_string()),
    );
    for (key, value) in [
        ("GIT_AUTHOR_NAME", "Zed"),
        ("GIT_AUTHOR_EMAIL", "hi@zed.dev"),
        ("GIT_COMMITTER_NAME", "Zed"),
        ("GIT_COMMITTER_EMAIL", "hi@zed.dev"),
    ] {
        env.insert(key.to_string(), Value::String(value.to_string()));
    }
    object.insert("env".to_string(), Value::Object(env));
    value
}

fn checkpoint(fs_rpc: &FsRpc, params: &Value) -> Result<Value> {
    let git_dir = git_dir(fs_rpc, params)?;
    let index = temporary_index(&git_dir, "checkpoint");
    if git_dir.join("index").exists() {
        fs::copy(git_dir.join("index"), &index)?;
    }
    let result = (|| {
        let checkpoint = checkpoint_params(params, &index);
        git(fs_rpc, &checkpoint, &["add", "--all"])?;
        let tree = git_text(fs_rpc, &checkpoint, &["write-tree"])?;
        let head = git_command(fs_rpc, params, &["rev-parse", "HEAD"], None, false)?;
        let mut args = vec!["commit-tree".to_string(), tree.trim().to_string()];
        if head.status.success() {
            args.extend([
                "-p".to_string(),
                String::from_utf8_lossy(&head.stdout).trim().to_string(),
            ]);
        }
        args.extend(["-m".to_string(), "Checkpoint".to_string()]);
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        Ok(json!({"commit_sha": git_text(fs_rpc, &checkpoint, &refs)?.trim()}))
    })();
    fs::remove_file(index).ok();
    result
}

fn restore_checkpoint(fs: &FsRpc, params: &Value) -> Result<Value> {
    null_command(
        fs,
        params,
        &[
            "restore",
            "--source",
            string(params, "commit_sha"),
            "--worktree",
            ".",
        ],
    )
}

fn create_archive_checkpoint(fs_rpc: &FsRpc, params: &Value) -> Result<Value> {
    let git_dir = git_dir(fs_rpc, params)?;
    let regular = checkpoint_params(params, &git_dir.join("index"));
    let head = git_text(fs_rpc, params, &["rev-parse", "HEAD"])?;
    let tree = git_text(fs_rpc, params, &["write-tree"])?;
    let staged = git_text(
        fs_rpc,
        &regular,
        &[
            "commit-tree",
            tree.trim(),
            "-p",
            head.trim(),
            "-m",
            "WIP staged",
        ],
    )?;
    let temp_index = temporary_index(&git_dir, "archive");
    if git_dir.join("index").exists() {
        fs::copy(git_dir.join("index"), &temp_index)?;
    }
    let result = (|| {
        let full = checkpoint_params(params, &temp_index);
        git(fs_rpc, &full, &["add", "--all"])?;
        let tree = git_text(fs_rpc, &full, &["write-tree"])?;
        let unstaged = git_text(
            fs_rpc,
            &full,
            &[
                "commit-tree",
                tree.trim(),
                "-p",
                staged.trim(),
                "-m",
                "WIP unstaged",
            ],
        )?;
        Ok(json!([staged.trim(), unstaged.trim()]))
    })();
    fs::remove_file(temp_index).ok();
    result
}

fn restore_archive_checkpoint(fs: &FsRpc, params: &Value) -> Result<Value> {
    git(
        fs,
        params,
        &["read-tree", "--reset", "-u", string(params, "unstaged_sha")],
    )?;
    null_command(fs, params, &["read-tree", string(params, "staged_sha")])
}

fn compare_checkpoints(fs: &FsRpc, params: &Value) -> Result<Value> {
    let output = git_command(
        fs,
        params,
        &[
            "diff-tree",
            "--quiet",
            string(params, "left"),
            string(params, "right"),
        ],
        None,
        false,
    )?;
    match output.status.code() {
        Some(0) => Ok(Value::Bool(true)),
        Some(1) => Ok(Value::Bool(false)),
        _ => bail!("{}", String::from_utf8_lossy(&output.stderr).trim()),
    }
}

fn diff_checkpoints(fs: &FsRpc, params: &Value) -> Result<Value> {
    Ok(Value::String(git_text(
        fs,
        params,
        &[
            "diff",
            "--find-renames",
            "--patch",
            string(params, "base"),
            string(params, "target"),
        ],
    )?))
}

fn file_history_changed_files(fs: &FsRpc, params: &Value) -> Result<Value> {
    let paths = string_array(params, "paths");
    if paths.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    let limit = params
        .get("commit_limit")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if limit == 0 {
        return Ok(Value::Array(
            paths.iter().map(|_| Value::Array(Vec::new())).collect(),
        ));
    }
    let mut args = vec![
        "log".to_string(),
        format!("--max-count={limit}"),
        "--full-diff".to_string(),
        "--no-renames".to_string(),
        "--name-only".to_string(),
        "-z".to_string(),
        "--format=%x1e".to_string(),
        "--".to_string(),
    ];
    args.extend(paths.iter().cloned());
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = git_text(fs, params, &refs)?;
    let mut histories = vec![Vec::<Value>::new(); paths.len()];
    for record in output.split('\u{1e}') {
        let mut changed = record
            .split('\0')
            .map(|field| field.trim_start_matches('\n'))
            .filter(|field| !field.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        changed.sort();
        changed.dedup();
        for (index, path) in paths.iter().enumerate() {
            if changed.contains(path) {
                histories[index].push(json!(changed));
            }
        }
    }
    Ok(json!(histories))
}

fn commit_data(fs: &FsRpc, params: &Value) -> Result<Value> {
    let sha = string(params, "sha");
    let content = git_text(fs, params, &["cat-file", "-p", sha])?;
    let (headers, message) = content.split_once("\n\n").unwrap_or((&content, ""));
    let mut parents = Vec::new();
    let mut author_name = "";
    let mut author_email = "";
    let mut timestamp = 0i64;
    for line in headers.lines() {
        if let Some(parent) = line.strip_prefix("parent ") {
            parents.push(parent.trim());
        } else if let Some(author) = line.strip_prefix("author ")
            && let Some(email_start) = author.rfind(" <")
            && let Some(email_end) = author[email_start + 2..].find("> ")
        {
            author_name = &author[..email_start];
            author_email = &author[email_start + 2..email_start + 2 + email_end];
            timestamp = author[email_start + 3 + email_end..]
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or_default();
        }
    }
    Ok(json!({
        "sha": sha,
        "parents": parents,
        "author_name": author_name,
        "author_email": author_email,
        "commit_timestamp": timestamp,
        "subject": message.lines().next().unwrap_or_default(),
        "message": message.trim_end_matches('\n'),
    }))
}

fn load_commit(fs: &FsRpc, params: &Value) -> Result<Value> {
    let output = git(
        fs,
        params,
        &[
            "show",
            "--format=",
            "-z",
            "--no-renames",
            "--raw",
            "--no-abbrev",
            "--first-parent",
            string(params, "commit"),
        ],
    )?;
    let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut files = Vec::new();
    for pair in fields.chunks(2) {
        if pair.len() != 2 || pair[0].is_empty() || pair[1].is_empty() {
            continue;
        }
        let metadata = String::from_utf8_lossy(pair[0]);
        let path = String::from_utf8_lossy(pair[1]);
        let values = metadata
            .trim_start_matches(':')
            .split_whitespace()
            .collect::<Vec<_>>();
        if values.len() < 5 {
            bail!("invalid raw commit diff metadata: {metadata}");
        }
        let (old_text, old_binary) = load_object(fs, params, values[0], values[2])?;
        let (new_text, new_binary) = load_object(fs, params, values[1], values[3])?;
        let is_binary = old_binary || new_binary;
        files.push(json!({
            "path": path,
            "old_text": if is_binary && old_text.is_some() { Some(String::new()) } else { old_text },
            "new_text": if is_binary && new_text.is_some() { Some(String::new()) } else { new_text },
            "is_binary": is_binary,
        }));
    }
    Ok(Value::Array(files))
}

fn load_object(
    fs: &FsRpc,
    params: &Value,
    mode: &str,
    oid: &str,
) -> Result<(Option<String>, bool)> {
    if oid.bytes().all(|byte| byte == b'0') {
        return Ok((None, false));
    }
    if mode == "160000" {
        return Ok((Some(format!("Subproject commit {oid}\n")), false));
    }
    let data = git(fs, params, &["cat-file", "-p", oid])?.stdout;
    let binary = data.iter().take(8000).any(|byte| *byte == 0);
    Ok((
        Some(if binary {
            String::new()
        } else {
            String::from_utf8_lossy(&data).into_owned()
        }),
        binary,
    ))
}

fn blame(fs: &FsRpc, params: &Value) -> Result<Value> {
    let output = git_command(
        fs,
        params,
        &[
            "blame",
            "--incremental",
            "--contents",
            "-",
            "--",
            string(params, "path"),
        ],
        Some(string(params, "content").as_bytes()),
        false,
    )?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if error == "fatal: no such ref: HEAD" || error.contains("fatal: no such path") {
            return Ok(json!({"entries": [], "messages": {}, "tag_names": {}}));
        }
        bail!("{error}");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    let mut cached = HashMap::<String, serde_json::Map<String, Value>>::new();
    let mut current: Option<serde_json::Map<String, Value>> = None;
    for line in text.lines() {
        let header = line.split_whitespace().collect::<Vec<_>>();
        if header.len() == 4
            && matches!(header[0].len(), 40 | 64)
            && header[0].bytes().all(|byte| byte.is_ascii_hexdigit())
            && header[1..].iter().all(|value| value.parse::<u64>().is_ok())
        {
            let sha = header[0].to_string();
            let start = header[2].parse::<u64>()? - 1;
            let mut entry = cached.get(&sha).cloned().unwrap_or_default();
            entry.extend([
                ("sha".to_string(), json!(sha)),
                (
                    "range".to_string(),
                    json!({"start": start, "end": start + header[3].parse::<u64>()?}),
                ),
                (
                    "original_line_number".to_string(),
                    json!(header[1].parse::<u64>()?),
                ),
                ("filename".to_string(), json!("")),
            ]);
            for name in [
                "author",
                "author_mail",
                "author_time",
                "author_tz",
                "committer_name",
                "committer_email",
                "committer_time",
                "committer_tz",
                "summary",
                "previous",
            ] {
                entry.entry(name.to_string()).or_insert(Value::Null);
            }
            current = Some(entry);
            continue;
        }
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let Some(entry) = current.as_mut() else {
            continue;
        };
        let field = match key {
            "author" => Some("author"),
            "author-mail" => Some("author_mail"),
            "author-time" => Some("author_time"),
            "author-tz" => Some("author_tz"),
            "committer" => Some("committer_name"),
            "committer-mail" => Some("committer_email"),
            "committer-time" => Some("committer_time"),
            "committer-tz" => Some("committer_tz"),
            "summary" => Some("summary"),
            "previous" => Some("previous"),
            _ => None,
        };
        if let Some(field) = field {
            entry.insert(
                field.to_string(),
                if matches!(key, "author-time" | "committer-time") {
                    json!(value.parse::<i64>().unwrap_or_default())
                } else {
                    json!(value)
                },
            );
        }
        if key == "filename" {
            entry.insert("filename".to_string(), json!(value));
            let sha = entry["sha"].as_str().unwrap_or_default().to_string();
            let metadata = entry
                .iter()
                .filter(|(key, _)| {
                    matches!(
                        key.as_str(),
                        "author"
                            | "author_mail"
                            | "author_time"
                            | "author_tz"
                            | "committer_name"
                            | "committer_email"
                            | "committer_time"
                            | "committer_tz"
                            | "summary"
                    )
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            cached.insert(sha.clone(), metadata);
            if sha.bytes().any(|byte| byte != b'0') {
                entries.push(Value::Object(current.take().expect("current blame entry")));
            } else {
                current = None;
            }
        }
    }
    entries.sort_by_key(|entry| entry["range"]["start"].as_u64().unwrap_or_default());
    let mut messages = serde_json::Map::new();
    let mut tags = serde_json::Map::new();
    let mut shas = entries
        .iter()
        .filter_map(|entry| entry["sha"].as_str())
        .collect::<Vec<_>>();
    shas.sort();
    shas.dedup();
    for sha in shas {
        let message = git_text(fs, params, &["show", "-s", "--format=%B", sha])?;
        messages.insert(
            sha.to_string(),
            json!(message.trim().replace('<', "&lt;").replace('>', "&gt;")),
        );
        let names = git_text(fs, params, &["tag", "--points-at", sha])?
            .lines()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if !names.is_empty() {
            tags.insert(sha.to_string(), json!(names));
        }
    }
    Ok(json!({"entries": entries, "messages": messages, "tag_names": tags}))
}

fn remote_urls(fs: &FsRpc, params: &Value) -> Result<Value> {
    let output = git_text(fs, params, &["remote", "-v"])?;
    let mut remotes = serde_json::Map::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(name), Some(url)) = (fields.next(), fields.next()) {
            remotes
                .entry(name.to_string())
                .or_insert_with(|| json!(url));
        }
    }
    Ok(Value::Object(remotes))
}

fn merge_message(fs: &FsRpc, params: &Value) -> Result<Value> {
    let git_dir = git_text(fs, params, &["rev-parse", "--git-dir"])?;
    let path = {
        let path = Path::new(git_dir.trim());
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            work_directory(fs, params)?.join(path)
        }
    };
    Ok(std::fs::read_to_string(path.join("MERGE_MSG"))
        .ok()
        .map(Value::String)
        .unwrap_or(Value::Null))
}

fn show(fs: &FsRpc, params: &Value) -> Result<Value> {
    let output = git_text(
        fs,
        params,
        &[
            "show",
            "--no-patch",
            "--format=%H%x00%B%x00%at%x00%ae%x00%an%x00",
            string(params, "commit"),
        ],
    )?;
    let fields = output.split('\0').collect::<Vec<_>>();
    if fields.len() < 5 {
        bail!("unexpected git show output");
    }
    Ok(json!({
        "sha": fields[0],
        "message": fields[1],
        "commit_timestamp": fields[2].parse::<i64>().unwrap_or_default(),
        "author_email": fields[3],
        "author_name": fields[4],
    }))
}

fn set_index_text(fs: &FsRpc, params: &Value) -> Result<Value> {
    let path = string(params, "path");
    let Some(content) = params.get("content").and_then(Value::as_str) else {
        return null_command(fs, params, &["update-index", "--force-remove", "--", path]);
    };
    let output = git_command(
        fs,
        params,
        &["hash-object", "-w", "--stdin"],
        Some(content.as_bytes()),
        true,
    )?;
    let blob = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mode = if params
        .get("is_executable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "100755"
    } else {
        "100644"
    };
    null_command(
        fs,
        params,
        &["update-index", "--add", "--cacheinfo", mode, &blob, path],
    )
}

fn change_branch(fs: &FsRpc, params: &Value) -> Result<Value> {
    null_command(fs, params, &["switch", string(params, "name")])
}

fn create_branch(fs: &FsRpc, params: &Value) -> Result<Value> {
    let mut args = vec![
        "switch".to_string(),
        "-c".to_string(),
        string(params, "name").to_string(),
    ];
    if let Some(base) = params
        .get("base_branch")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        args.push(base.to_string());
    }
    null_owned_command(fs, params, args)
}

fn rename_branch(fs: &FsRpc, params: &Value) -> Result<Value> {
    null_command(
        fs,
        params,
        &[
            "branch",
            "-m",
            string(params, "branch"),
            string(params, "new_name"),
        ],
    )
}

fn delete_branch(fs: &FsRpc, params: &Value) -> Result<Value> {
    if params
        .get("is_remote")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let name = string(params, "name");
        let (remote, branch) = name
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("remote branch must include the remote name"))?;
        null_command(fs, params, &["push", remote, "--delete", branch])
    } else {
        null_command(
            fs,
            params,
            &[
                "branch",
                if params
                    .get("force")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    "-D"
                } else {
                    "-d"
                },
                string(params, "name"),
            ],
        )
    }
}

fn worktree_created_at(fs: &FsRpc, params: &Value) -> Result<Value> {
    let path = fs.path(string(params, "worktree_path"))?;
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(Value::Null);
    };
    let created = metadata.created().or_else(|_| metadata.modified())?;
    let duration = created.duration_since(UNIX_EPOCH).unwrap_or_default();
    Ok(json!({"secs": duration.as_secs(), "nanos": duration.subsec_nanos()}))
}

fn checkout_branch_in_worktree(fs: &FsRpc, params: &Value) -> Result<Value> {
    let path = fs.path(string(params, "worktree_path"))?;
    let mut args = vec!["worktree".to_string(), "add".to_string()];
    if params
        .get("create")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        args.extend(["-b".to_string(), string(params, "branch_name").to_string()]);
    }
    args.extend([
        path.to_string_lossy().to_string(),
        string(params, "branch_name").to_string(),
    ]);
    null_owned_command(fs, params, args)
}

fn remove_worktree(fs: &FsRpc, params: &Value) -> Result<Value> {
    let path = fs.path(string(params, "path"))?;
    let mut args = vec!["worktree".to_string(), "remove".to_string()];
    if params
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        args.push("--force".to_string());
    }
    args.push(path.to_string_lossy().to_string());
    null_owned_command(fs, params, args)
}

fn rename_worktree(fs: &FsRpc, params: &Value) -> Result<Value> {
    let old = fs.path(string(params, "old_path"))?;
    let new = fs.path(string(params, "new_path"))?;
    let args = vec![
        "worktree".to_string(),
        "move".to_string(),
        old.to_string_lossy().to_string(),
        new.to_string_lossy().to_string(),
    ];
    null_owned_command(fs, params, args)
}

fn reset(fs: &FsRpc, params: &Value) -> Result<Value> {
    null_command(
        fs,
        params,
        &[
            "reset",
            if string(params, "mode") == "soft" {
                "--soft"
            } else {
                "--mixed"
            },
            string(params, "commit"),
        ],
    )
}

fn checkout_files(fs: &FsRpc, params: &Value) -> Result<Value> {
    let mut args = vec![
        "checkout".to_string(),
        string(params, "commit").to_string(),
        "--".to_string(),
    ];
    args.extend(string_array(params, "paths"));
    null_owned_command(fs, params, args)
}

fn paths_command(fs: &FsRpc, params: &Value, prefix: &[&str]) -> Result<Value> {
    let mut args = prefix
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    args.extend(string_array(params, "paths"));
    null_owned_command(fs, params, args)
}

fn stash(fs: &FsRpc, params: &Value, action: &str) -> Result<Value> {
    let mut args = vec!["stash".to_string(), action.to_string()];
    if let Some(index) = params.get("index").and_then(Value::as_u64) {
        args.push(format!("stash@{{{index}}}"));
    }
    null_owned_command(fs, params, args)
}

fn remove_remote(fs: &FsRpc, params: &Value) -> Result<Value> {
    null_command(fs, params, &["remote", "remove", string(params, "name")])
}

fn create_remote(fs: &FsRpc, params: &Value) -> Result<Value> {
    null_command(
        fs,
        params,
        &[
            "remote",
            "add",
            string(params, "name"),
            string(params, "url"),
        ],
    )
}

fn check_for_pushed_commit(fs: &FsRpc, params: &Value) -> Result<Value> {
    let output = git_command(
        fs,
        params,
        &["branch", "-r", "--contains", "HEAD"],
        None,
        false,
    )?;
    Ok(json!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
    ))
}

fn diff(fs: &FsRpc, params: &Value) -> Result<Value> {
    let args = match string(params, "kind") {
        "head_to_index" => vec!["diff".to_string(), "--cached".to_string()],
        "merge_base" => vec![
            "diff".to_string(),
            format!(
                "{}...HEAD",
                params
                    .get("base_ref")
                    .and_then(Value::as_str)
                    .unwrap_or("HEAD")
            ),
        ],
        _ => vec!["diff".to_string(), "HEAD".to_string()],
    };
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    Ok(Value::String(git_text(fs, params, &refs)?))
}

fn update_ref(fs: &FsRpc, params: &Value) -> Result<Value> {
    null_command(
        fs,
        params,
        &[
            "update-ref",
            string(params, "ref_name"),
            string(params, "commit"),
        ],
    )
}

fn delete_ref(fs: &FsRpc, params: &Value) -> Result<Value> {
    null_command(
        fs,
        params,
        &["update-ref", "-d", string(params, "ref_name")],
    )
}

fn commit(fs: &FsRpc, params: &Value) -> Result<Value> {
    let mut owned_params = params.clone();
    let mut env = environment(params)
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect::<serde_json::Map<_, _>>();
    if let Some(identity) = params.get("name_and_email").and_then(Value::as_array)
        && identity.len() >= 2
    {
        let name = identity[0].as_str().unwrap_or_default();
        let email = identity[1].as_str().unwrap_or_default();
        for (key, value) in [
            ("GIT_AUTHOR_NAME", name),
            ("GIT_AUTHOR_EMAIL", email),
            ("GIT_COMMITTER_NAME", name),
            ("GIT_COMMITTER_EMAIL", email),
        ] {
            env.insert(key.to_string(), json!(value));
        }
    }
    owned_params
        .as_object_mut()
        .expect("params object")
        .insert("env".to_string(), Value::Object(env));
    let mut args = vec![
        "commit".to_string(),
        "-m".to_string(),
        string(params, "message").to_string(),
    ];
    let options = params.get("options").and_then(Value::as_object);
    for (key, flag) in [
        ("amend", "--amend"),
        ("signoff", "--signoff"),
        ("allow_empty", "--allow-empty"),
    ] {
        if options
            .and_then(|options| options.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            args.push(flag.to_string());
        }
    }
    null_owned_command(fs, &owned_params, args)
}

fn command_output(fs: &FsRpc, params: &Value, args: Vec<String>) -> Result<Value> {
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = git_command(fs, params, &refs, None, false)?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(json!({
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

fn push(fs: &FsRpc, params: &Value) -> Result<Value> {
    let mut args = vec!["push".to_string()];
    match string(params, "option") {
        "set_upstream" => args.push("--set-upstream".to_string()),
        "force" => args.push("--force-with-lease".to_string()),
        _ => {}
    }
    args.extend([
        string(params, "upstream_name").to_string(),
        format!(
            "{}:{}",
            string(params, "branch_name"),
            string(params, "remote_branch_name")
        ),
    ]);
    command_output(fs, params, args)
}

fn pull(fs: &FsRpc, params: &Value) -> Result<Value> {
    let mut args = vec!["pull".to_string()];
    if params
        .get("rebase")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        args.push("--rebase".to_string());
    }
    args.push(string(params, "upstream_name").to_string());
    if let Some(branch) = params
        .get("branch_name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        args.push(branch.to_string());
    }
    command_output(fs, params, args)
}

fn fetch(fs: &FsRpc, params: &Value) -> Result<Value> {
    let args = params
        .get("remote")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|remote| vec!["fetch".to_string(), remote.to_string()])
        .unwrap_or_else(|| vec!["fetch".to_string(), "--all".to_string()]);
    command_output(fs, params, args)
}

fn get_branch_remote(fs: &FsRpc, params: &Value) -> Result<Value> {
    let key = format!("branch.{}.remote", string(params, "branch"));
    optional_remote(fs, params, &key)
}

fn get_push_remote(fs: &FsRpc, params: &Value) -> Result<Value> {
    let branch = string(params, "branch");
    for key in [
        format!("branch.{branch}.pushRemote"),
        "remote.pushDefault".to_string(),
        format!("branch.{branch}.remote"),
    ] {
        let value = optional_remote(fs, params, &key)?;
        if !value.is_null() {
            return Ok(value);
        }
    }
    Ok(Value::Null)
}

fn optional_remote(fs: &FsRpc, params: &Value, key: &str) -> Result<Value> {
    let output = git_command(fs, params, &["config", "--get", key], None, false)?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if name.is_empty() {
        Value::Null
    } else {
        json!({"name": name})
    })
}

fn get_all_remotes(fs: &FsRpc, params: &Value) -> Result<Value> {
    let output = git_command(fs, params, &["remote"], None, false)?;
    Ok(json!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|name| json!({"name": name}))
            .collect::<Vec<_>>()
    ))
}

fn null_command(fs: &FsRpc, params: &Value, args: &[&str]) -> Result<Value> {
    git(fs, params, args)?;
    Ok(Value::Null)
}

fn null_owned_command(fs: &FsRpc, params: &Value, args: Vec<String>) -> Result<Value> {
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    null_command(fs, params, &refs)
}

fn default_branch(fs: &FsRpc, params: &Value) -> Result<Value> {
    if let Ok(output) = git(
        fs,
        params,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        let mut branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !branch.is_empty() {
            if !params
                .get("include_remote_name")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                branch = branch
                    .split_once('/')
                    .map(|(_, name)| name.to_string())
                    .unwrap_or(branch);
            }
            return Ok(Value::String(branch));
        }
    }
    for candidate in ["main", "master"] {
        if git(
            fs,
            params,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ],
        )
        .is_ok()
        {
            return Ok(Value::String(candidate.to_string()));
        }
    }
    Ok(Value::Null)
}

fn initial_graph_data(fs: &FsRpc, params: &Value) -> Result<Value> {
    let order = match string(params, "order") {
        "topo" => "--topo-order",
        "author_date" => "--author-date-order",
        "reverse" => "--reverse",
        _ => "--date-order",
    };
    let mut args = vec![
        "log".to_string(),
        "--format=%H%x00%P%x00%D".to_string(),
        order.to_string(),
    ];
    args.extend(log_source_args(params)?);
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let commits = git_text(fs, params, &args)?
        .lines()
        .filter_map(|line| {
            let fields = line.split('\0').collect::<Vec<_>>();
            (fields.len() >= 2).then(|| {
                json!({
                    "sha": fields[0],
                    "parents": fields[1].split_whitespace().collect::<Vec<_>>(),
                    "ref_names": fields.get(2).filter(|value| !value.is_empty()).map(|value| value.split(", ").collect::<Vec<_>>()).unwrap_or_default(),
                })
            })
        })
        .collect();
    Ok(Value::Array(commits))
}

fn search_commits(fs: &FsRpc, params: &Value) -> Result<Value> {
    let query = string(params, "query").trim();
    let is_hash =
        (7..=40).contains(&query.len()) && query.bytes().all(|byte| byte.is_ascii_hexdigit());
    let mut args = vec!["log".to_string(), "--format=%H".to_string()];
    if !is_hash {
        args.push("--fixed-strings".to_string());
        if !params
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            args.push("--regexp-ignore-case".to_string());
        }
        args.extend(["--grep".to_string(), query.to_string()]);
    }
    args.extend(log_source_args(params)?);
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let query = query.to_ascii_lowercase();
    Ok(Value::Array(
        git_text(fs, params, &args)?
            .lines()
            .filter(|commit| !is_hash || commit.to_ascii_lowercase().starts_with(&query))
            .map(|commit| Value::String(commit.to_string()))
            .collect(),
    ))
}

fn log_source_args(params: &Value) -> Result<Vec<String>> {
    let source = params
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("missing git log source"))?;
    match source.get("kind").and_then(Value::as_str) {
        Some("all") => Ok([
            "--ignore-missing",
            "--branches",
            "--remotes",
            "--tags",
            "HEAD",
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()),
        Some("branch" | "sha") => Ok(vec![
            source
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ]),
        Some("path") => Ok(vec![
            "--follow".to_string(),
            "--".to_string(),
            source
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ]),
        kind => bail!("unknown git log source: {kind:?}"),
    }
}
