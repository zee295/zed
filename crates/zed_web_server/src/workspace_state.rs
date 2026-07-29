use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use anyhow::{Context as _, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceStateSnapshot {
    pub sidebar_open: bool,
    pub project_groups: Vec<Vec<String>>,
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

    pub fn set_sidebar_open(&self, open: bool) -> Result<()> {
        self.update(|snapshot| snapshot.sidebar_open = open)
    }

    pub fn set_project_groups(&self, groups: Vec<Vec<String>>) -> Result<()> {
        self.update(|snapshot| snapshot.project_groups = groups)
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
