use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use anyhow::{Context as _, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceStateSnapshot {
    pub sidebar_open: bool,
    #[serde(default)]
    pub project_groups: Vec<Vec<String>>,
    #[serde(default)]
    pub active_workspace_id: Option<String>,
    #[serde(default)]
    pub workspaces: Vec<HostedWorkspaceSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostedWorkspaceSnapshot {
    pub id: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub activation_generation: u64,
}

pub struct WorkspaceState {
    path: PathBuf,
    snapshot: RwLock<WorkspaceStateSnapshot>,
}

impl WorkspaceState {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(".zed/web-workspace-state.json");
        let mut snapshot = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                WorkspaceStateSnapshot::default()
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        if snapshot.project_groups.is_empty() {
            snapshot.project_groups = project_groups_from_sidebar_database(root)?;
        }
        snapshot.reconcile_workspaces();
        Ok(Self {
            path,
            snapshot: RwLock::new(snapshot),
        })
    }

    pub fn snapshot(&self) -> WorkspaceStateSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn active_paths(&self) -> Option<Vec<String>> {
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let active_workspace_id = snapshot.active_workspace_id.as_ref()?;
        snapshot
            .workspaces
            .iter()
            .find(|workspace| &workspace.id == active_workspace_id)
            .map(|workspace| workspace.paths.clone())
    }

    pub fn set_sidebar_open(&self, open: bool) -> Result<()> {
        self.update(|snapshot| snapshot.sidebar_open = open)
    }

    pub fn set_project_groups(&self, groups: Vec<Vec<String>>) -> Result<()> {
        self.update(|snapshot| {
            snapshot.project_groups = normalize_groups(groups);
            snapshot.reconcile_workspaces();
        })
    }

    pub fn activate(&self, paths: Vec<String>) -> Result<HostedWorkspaceSnapshot> {
        let paths = normalize_paths(paths);
        anyhow::ensure!(!paths.is_empty(), "workspace paths cannot be empty");

        let id = workspace_id(&paths);
        self.update(|snapshot| {
            if snapshot.active_workspace_id.as_ref() == Some(&id)
                && snapshot
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.id == id && workspace.paths == paths)
            {
                return;
            }
            let generation = snapshot
                .workspaces
                .iter()
                .map(|workspace| workspace.activation_generation)
                .max()
                .unwrap_or_default()
                .wrapping_add(1);
            if let Some(workspace) = snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == id)
            {
                workspace.paths = paths.clone();
                workspace.activation_generation = generation;
            } else {
                snapshot.workspaces.push(HostedWorkspaceSnapshot {
                    id: id.clone(),
                    paths: paths.clone(),
                    activation_generation: generation,
                });
            }
            if !snapshot.project_groups.contains(&paths) {
                snapshot.project_groups.push(paths.clone());
            }
            snapshot.active_workspace_id = Some(id.clone());
        })?;

        self.snapshot()
            .workspaces
            .into_iter()
            .find(|workspace| workspace.id == id)
            .context("activated workspace disappeared")
    }

    fn update(&self, update: impl FnOnce(&mut WorkspaceStateSnapshot)) -> Result<()> {
        let mut snapshot = self
            .snapshot
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = snapshot.clone();
        update(&mut snapshot);
        if *snapshot == previous {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&*snapshot)?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, bytes).with_context(|| format!("writing {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

impl WorkspaceStateSnapshot {
    fn reconcile_workspaces(&mut self) {
        self.project_groups = normalize_groups(std::mem::take(&mut self.project_groups));

        for paths in &self.project_groups {
            let id = workspace_id(paths);
            if !self.workspaces.iter().any(|workspace| workspace.id == id) {
                self.workspaces.push(HostedWorkspaceSnapshot {
                    id,
                    paths: paths.clone(),
                    activation_generation: 0,
                });
            }
        }
        if self
            .active_workspace_id
            .as_ref()
            .is_some_and(|id| !self.workspaces.iter().any(|workspace| &workspace.id == id))
        {
            self.active_workspace_id = None;
        }
    }
}

fn normalize_groups(groups: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let mut normalized = Vec::new();
    for group in groups {
        let group = normalize_paths(group);
        if !group.is_empty() && !normalized.contains(&group) {
            normalized.push(group);
        }
    }
    normalized
}

fn normalize_paths(paths: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for path in paths {
        let path = path.trim();
        let path = if path == "/" {
            path.to_string()
        } else {
            path.trim_end_matches('/').to_string()
        };
        if !path.is_empty() && !normalized.contains(&path) {
            normalized.push(path);
        }
    }
    normalized
}

fn workspace_id(paths: &[String]) -> String {
    let mut digest = Sha256::new();
    for path in paths {
        digest.update(path.as_bytes());
        digest.update([0]);
    }
    format!("workspace-{}", hex::encode(digest.finalize()))
}

fn project_groups_from_sidebar_database(root: &Path) -> Result<Vec<Vec<String>>> {
    let path = root.join(".zed/remote.sqlite");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection = Connection::open(path)?;
    let mut groups = Vec::new();
    for table in ["sidebar_threads", "sidebar_terminal_threads"] {
        let table_exists = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )?;
        if !table_exists {
            continue;
        }
        let has_folder_paths = connection
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|column| column == "folder_paths");
        if !has_folder_paths {
            continue;
        }
        let values = connection
            .prepare(&format!(
                "SELECT folder_paths FROM {table} WHERE folder_paths IS NOT NULL"
            ))?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for value in values {
            let group = value
                .lines()
                .filter(|path| !path.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            if !group.is_empty() && !groups.contains(&group) {
                groups.push(group);
            }
        }
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_workspace_state_across_server_restart() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join(".zed"))?;
        let state = WorkspaceState::load(root.path())?;
        state.set_sidebar_open(true)?;
        state.set_project_groups(vec![
            vec!["/srv/project-a".into()],
            vec!["/srv/project-b".into()],
        ])?;

        let restored = WorkspaceState::load(root.path())?.snapshot();
        assert!(restored.sidebar_open);
        assert_eq!(
            restored.project_groups,
            vec![
                vec!["/srv/project-a".to_string()],
                vec!["/srv/project-b".to_string()]
            ]
        );
        assert_eq!(restored.workspaces.len(), 2);
        Ok(())
    }

    #[test]
    fn persists_active_workspace_with_a_stable_identity() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join(".zed"))?;
        let state = WorkspaceState::load(root.path())?;

        let activated = state.activate(vec![
            "/srv/project-a/".into(),
            "/srv/project-a/extra".into(),
        ])?;
        assert_eq!(activated.activation_generation, 1);

        let restored = WorkspaceState::load(root.path())?.snapshot();
        assert_eq!(
            restored.active_workspace_id.as_deref(),
            Some(activated.id.as_str())
        );
        assert_eq!(restored.workspaces, vec![activated.clone()]);

        let reactivated = WorkspaceState::load(root.path())?.activate(activated.paths.clone())?;
        assert_eq!(reactivated.id, activated.id);
        assert_eq!(reactivated.activation_generation, 1);
        Ok(())
    }

    #[test]
    fn keeps_hosted_workspaces_when_sidebar_groups_change() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join(".zed"))?;
        let state = WorkspaceState::load(root.path())?;
        let project_a = state.activate(vec!["/srv/project-a".into()])?;
        state.activate(vec!["/srv/project-b".into()])?;

        state.set_project_groups(vec![vec!["/srv/project-b".into()]])?;

        let snapshot = state.snapshot();
        assert!(
            snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.id == project_a.id)
        );
        assert_eq!(
            state.active_paths(),
            Some(vec!["/srv/project-b".to_string()])
        );
        Ok(())
    }

    #[test]
    fn bootstraps_project_groups_from_persisted_threads() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join(".zed"))?;
        let connection = Connection::open(root.path().join(".zed/remote.sqlite"))?;
        connection.execute_batch(
            "CREATE TABLE sidebar_threads (folder_paths TEXT);
             CREATE TABLE sidebar_terminal_threads (folder_paths TEXT);
             INSERT INTO sidebar_threads VALUES ('/srv/project-a');
             INSERT INTO sidebar_threads VALUES ('/srv/project-b
/srv/project-b/extra');
             INSERT INTO sidebar_terminal_threads VALUES ('/srv/project-a');",
        )?;
        drop(connection);

        let restored = WorkspaceState::load(root.path())?.snapshot();
        assert_eq!(
            restored.project_groups,
            vec![
                vec!["/srv/project-a".to_string()],
                vec![
                    "/srv/project-b".to_string(),
                    "/srv/project-b/extra".to_string()
                ]
            ]
        );
        Ok(())
    }
}
