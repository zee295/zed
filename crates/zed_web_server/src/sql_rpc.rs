use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rusqlite::{
    Connection, Row, ToSql,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::{Value, json};

pub struct SqlRpc {
    root: PathBuf,
    path: PathBuf,
    state: Mutex<SqlState>,
    transaction_available: Condvar,
}

struct SqlState {
    connection: Connection,
    savepoint_depth: usize,
    explicit_transaction: bool,
    transaction_owner: Option<String>,
}

impl SqlRpc {
    pub fn new(root: &Path) -> Result<Self> {
        let directory = root.join(".zed");
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("remote.sqlite");
        Ok(Self {
            state: Mutex::new(SqlState {
                connection: open_connection(&path, root)?,
                savepoint_depth: 0,
                explicit_transaction: false,
                transaction_owner: None,
            }),
            root: root.to_path_buf(),
            path,
            transaction_available: Condvar::new(),
        })
    }

    pub fn dispatch(&self, method: &str, params: &Value) -> Result<Value> {
        let client_id = params
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or("legacy");
        let mut state = self.lock_for_client(client_id)?;
        let result = match method {
            "Sql::query" => query(&mut state, params),
            "Sql::batch" => batch(&mut state, params),
            "Sql::script" => script(&mut state, params),
            "Sql::migrate" => migrate(&mut state, params),
            "Sql::bootstrap_kvp" => bootstrap_kvp(&mut state),
            "Sql::reset" => self.reset(&mut state),
            _ => bail!("unknown sql method: {method}"),
        };
        if state.savepoint_depth > 0 || state.explicit_transaction {
            state.transaction_owner = Some(client_id.to_string());
        } else if state.transaction_owner.as_deref() == Some(client_id) {
            state.transaction_owner = None;
            self.transaction_available.notify_all();
        }
        result
    }

    pub fn release_client(&self, client_id: &str) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.transaction_owner.as_deref() != Some(client_id) {
            return;
        }
        state.connection.execute_batch("ROLLBACK").ok();
        state.savepoint_depth = 0;
        state.explicit_transaction = false;
        state.transaction_owner = None;
        self.transaction_available.notify_all();
    }

    fn lock_for_client(&self, client_id: &str) -> Result<MutexGuard<'_, SqlState>> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("SQL state lock poisoned"))?;
        while state
            .transaction_owner
            .as_deref()
            .is_some_and(|owner| owner != client_id)
        {
            let now = Instant::now();
            if now >= deadline {
                state.connection.execute_batch("ROLLBACK").ok();
                state.savepoint_depth = 0;
                state.explicit_transaction = false;
                state.transaction_owner = None;
                self.transaction_available.notify_all();
                break;
            }
            let (next, _) = self
                .transaction_available
                .wait_timeout(state, deadline - now)
                .map_err(|_| anyhow!("SQL state lock poisoned"))?;
            state = next;
        }
        Ok(state)
    }

    fn reset(&self, state: &mut SqlState) -> Result<Value> {
        let temporary = Connection::open_in_memory()?;
        let old = std::mem::replace(&mut state.connection, temporary);
        drop(old);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let candidate = PathBuf::from(format!("{}{}", self.path.display(), suffix));
            if candidate.exists() {
                std::fs::remove_file(candidate)?;
            }
        }
        state.connection = open_connection(&self.path, &self.root)?;
        state.savepoint_depth = 0;
        state.explicit_transaction = false;
        state.transaction_owner = None;
        Ok(json!({"ok": true, "path": self.path}))
    }
}

fn open_connection(path: &Path, root: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
    )?;
    repair_orphaned_editor_items(&connection)?;
    migrate_legacy_workspace_paths(&connection, root)?;
    Ok(connection)
}

fn migrate_legacy_workspace_paths(connection: &Connection, root: &Path) -> Result<()> {
    const PATH_COLUMNS: &[(&str, &[&str])] = &[
        ("sidebar_threads", &["folder_paths", "main_worktree_paths"]),
        (
            "sidebar_terminal_threads",
            &["working_directory", "folder_paths", "main_worktree_paths"],
        ),
    ];

    for (table, columns) in PATH_COLUMNS {
        let table_exists = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )?;
        if !table_exists {
            continue;
        }

        let existing_columns = connection
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        for column in *columns {
            if !existing_columns.contains(*column) {
                continue;
            }
            let rows = connection
                .prepare(&format!(
                    "SELECT rowid, {column} FROM {table} WHERE {column} IS NOT NULL"
                ))?
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (rowid, value) in rows {
                let rewritten = rewrite_legacy_path_list(&value, root);
                if rewritten != value {
                    connection.execute(
                        &format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2"),
                        (&rewritten, rowid),
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn rewrite_legacy_path_list(value: &str, root: &Path) -> String {
    value
        .split('\n')
        .map(|path| {
            if path == "/workspace" {
                root.display().to_string()
            } else if let Some(relative) = path.strip_prefix("/workspace/") {
                root.join(relative).display().to_string()
            } else {
                path.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn repair_orphaned_editor_items(connection: &Connection) -> Result<()> {
    let has_required_tables = connection.query_row(
        "SELECT COUNT(*) = 2
         FROM sqlite_master
         WHERE type = 'table' AND name IN ('items', 'editors')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if has_required_tables {
        connection.execute(
            "DELETE FROM items
             WHERE kind = 'Editor'
               AND NOT EXISTS (
                   SELECT 1
                   FROM editors
                   WHERE editors.item_id = items.item_id
                     AND editors.workspace_id = items.workspace_id
               )",
            [],
        )?;
    }
    Ok(())
}

fn query(state: &mut SqlState, params: &Value) -> Result<Value> {
    let sql = params
        .get("sql")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if sql.is_empty() {
        bail!("empty sql");
    }
    let arguments = decode_arguments(params.get("params"));
    let statements = split_statements(sql);
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let mut changes = 0_u64;
    for statement in statements {
        let kind = statement_kind(&statement);
        let bind_count = placeholder_count(&statement).min(arguments.len());
        let bind_values = arguments[..bind_count]
            .iter()
            .map(|value| value as &dyn ToSql)
            .collect::<Vec<_>>();
        {
            let mut prepared = state.connection.prepare(&statement)?;
            if prepared.column_count() > 0 {
                columns = prepared
                    .column_names()
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect();
                rows = prepared
                    .query_map(bind_values.as_slice(), encode_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
            } else {
                changes += prepared.execute(bind_values.as_slice())? as u64;
            }
        }
        apply_transaction_state(state, kind, &statement)?;
    }
    Ok(json!({
        "columns": columns,
        "rows": rows,
        "last_rowid": state.connection.last_insert_rowid(),
        "changes": changes,
        "savepoint_depth": state.savepoint_depth,
    }))
}

fn batch(state: &mut SqlState, params: &Value) -> Result<Value> {
    let queries = params
        .get("queries")
        .and_then(Value::as_array)
        .context("SQL batch queries must be an array")?;

    state.connection.execute_batch("SAVEPOINT zed_rpc_batch")?;
    let result = (|| {
        let mut last_rowids = Vec::with_capacity(queries.len());
        for (index, request) in queries.iter().enumerate() {
            let sql = request
                .get("sql")
                .and_then(Value::as_str)
                .filter(|sql| !sql.trim().is_empty())
                .with_context(|| format!("SQL batch query {index} has no SQL"))?;
            let raw_params = request
                .get("params")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let resolved_params = raw_params
                .iter()
                .map(|param| resolve_batch_param(param, &last_rowids))
                .collect::<Result<Vec<_>>>()?;
            query(
                state,
                &json!({
                    "sql": sql,
                    "params": resolved_params,
                }),
            )?;
            last_rowids.push(state.connection.last_insert_rowid());
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            state.connection.execute_batch("RELEASE zed_rpc_batch")?;
            Ok(json!({"ok": true, "queries": queries.len()}))
        }
        Err(error) => {
            state
                .connection
                .execute_batch("ROLLBACK TO zed_rpc_batch; RELEASE zed_rpc_batch")?;
            Err(error)
        }
    }
}

fn resolve_batch_param(param: &Value, last_rowids: &[i64]) -> Result<Value> {
    if param.get("type").and_then(Value::as_str) != Some("batch_last_rowid") {
        return Ok(param.clone());
    }
    let query = param
        .get("query")
        .and_then(Value::as_u64)
        .context("SQL batch row ID reference has no query index")? as usize;
    let rowid = last_rowids
        .get(query)
        .with_context(|| format!("SQL batch row ID references future query {query}"))?;
    Ok(json!(rowid))
}

fn script(state: &mut SqlState, params: &Value) -> Result<Value> {
    let statements = split_statements(
        params
            .get("sql")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    for statement in &statements {
        state.connection.execute_batch(statement)?;
        apply_transaction_state(state, statement_kind(statement), statement)?;
    }
    Ok(json!({
        "ok": true,
        "statements": statements.len(),
        "savepoint_depth": state.savepoint_depth,
    }))
}

fn migrate(state: &mut SqlState, params: &Value) -> Result<Value> {
    let domain = params
        .get("domain")
        .and_then(Value::as_str)
        .filter(|domain| !domain.is_empty())
        .ok_or_else(|| anyhow!("migration domain must be a non-empty string"))?;
    let migrations = params
        .get("migrations")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("migrations must be an array of SQL strings"))?
        .iter()
        .map(|migration| {
            migration
                .as_str()
                .map(str::trim)
                .ok_or_else(|| anyhow!("migrations must be an array of SQL strings"))
        })
        .collect::<Result<Vec<_>>>()?;
    let allowed_changes = params
        .get("allowed_changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .collect::<HashSet<_>>();

    const SAVEPOINT: &str = "zed_remote_domain_migration";
    state
        .connection
        .execute_batch(&format!("SAVEPOINT {SAVEPOINT}"))?;
    let migration_result = (|| -> Result<Value> {
        state.connection.execute(
            "CREATE TABLE IF NOT EXISTS migrations (domain TEXT, step INTEGER, migration TEXT)",
            [],
        )?;
        let completed = {
            let mut statement = state
                .connection
                .prepare("SELECT step, migration FROM migrations WHERE domain = ? ORDER BY step")?;
            statement
                .query_map([domain], |row| {
                    Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<HashMap<_, _>>>()?
        };
        let drift = migrations
            .iter()
            .enumerate()
            .filter_map(|(index, proposed)| {
                let stored = completed.get(&(index as u64))?;
                (stored.trim() != *proposed && !allowed_changes.contains(&(index as u64)))
                    .then(|| json!({"index": index, "stored": stored, "proposed": proposed}))
            })
            .collect::<Vec<_>>();
        if !drift.is_empty() {
            return Ok(json!({"status": "drift", "changes": drift}));
        }
        let mut applied = 0;
        for (index, migration) in migrations.iter().enumerate() {
            if completed.contains_key(&(index as u64)) {
                continue;
            }
            for statement in split_statements(migration) {
                state.connection.execute_batch(&statement)?;
            }
            state.connection.execute(
                "INSERT INTO migrations (domain, step, migration) VALUES (?, ?, ?)",
                (domain, index as u64, migration),
            )?;
            applied += 1;
        }
        Ok(json!({"status": "ok", "applied": applied}))
    })();

    match migration_result {
        Ok(result) if result["status"] == "drift" => {
            state
                .connection
                .execute_batch(&format!("ROLLBACK TO {SAVEPOINT}; RELEASE {SAVEPOINT}"))?;
            Ok(result)
        }
        Ok(result) => {
            state
                .connection
                .execute_batch(&format!("RELEASE {SAVEPOINT}"))?;
            Ok(result)
        }
        Err(error) => {
            state
                .connection
                .execute_batch(&format!("ROLLBACK TO {SAVEPOINT}; RELEASE {SAVEPOINT}"))
                .ok();
            Err(error)
        }
    }
}

fn bootstrap_kvp(state: &mut SqlState) -> Result<Value> {
    Ok(json!({
        "kv": collect_text_rows(&state.connection, "SELECT key, value FROM kv_store", 2)?,
        "scoped": collect_text_rows(
            &state.connection,
            "SELECT namespace, key, value FROM scoped_kv_store",
            3
        )?,
    }))
}

fn collect_text_rows(
    connection: &Connection,
    sql: &str,
    columns: usize,
) -> Result<Vec<Vec<String>>> {
    let mut statement = connection.prepare(sql)?;
    Ok(statement
        .query_map([], |row| {
            (0..columns)
                .map(|index| row.get::<_, String>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn decode_arguments(raw: Option<&Value>) -> Vec<SqlValue> {
    raw.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(decode_argument)
        .collect()
}

fn decode_argument(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(value) => SqlValue::Integer(i64::from(*value)),
        Value::Number(value) => value
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| value.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        Value::String(value) => SqlValue::Text(value.clone()),
        Value::Array(values) => SqlValue::Blob(
            values
                .iter()
                .filter_map(Value::as_u64)
                .map(|value| value as u8)
                .collect(),
        ),
        Value::Object(value) => match value.get("type").and_then(Value::as_str) {
            Some("blob") => SqlValue::Blob(
                BASE64
                    .decode(
                        value
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
                    .unwrap_or_default(),
            ),
            Some("int") => SqlValue::Integer(
                value
                    .get("value")
                    .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
                    .unwrap_or_default(),
            ),
            Some("float") => SqlValue::Real(
                value
                    .get("value")
                    .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
                    .unwrap_or_default(),
            ),
            Some("text") => SqlValue::Text(
                value
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
            _ => SqlValue::Null,
        },
    }
}

fn encode_row(row: &Row<'_>) -> rusqlite::Result<Vec<Value>> {
    (0..row.as_ref().column_count())
        .map(|index| {
            Ok(match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => json!(value),
                ValueRef::Real(value) => json!(value),
                ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
                ValueRef::Blob(value) => json!({"type": "blob", "data": BASE64.encode(value)}),
            })
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatementKind {
    Savepoint,
    Release,
    Begin,
    Commit,
    Rollback,
    Other,
}

fn statement_kind(sql: &str) -> StatementKind {
    let normalized = sql.trim().to_ascii_uppercase();
    if normalized.starts_with("SAVEPOINT") {
        StatementKind::Savepoint
    } else if normalized.starts_with("RELEASE") {
        StatementKind::Release
    } else if normalized.starts_with("BEGIN") {
        StatementKind::Begin
    } else if normalized.starts_with("COMMIT")
        || normalized.starts_with("END TRANSACTION")
        || normalized == "END"
    {
        StatementKind::Commit
    } else if normalized.starts_with("ROLLBACK") {
        StatementKind::Rollback
    } else {
        StatementKind::Other
    }
}

fn apply_transaction_state(
    state: &mut SqlState,
    kind: StatementKind,
    sql: &str,
) -> rusqlite::Result<()> {
    match kind {
        StatementKind::Savepoint => state.savepoint_depth += 1,
        StatementKind::Release => {
            state.savepoint_depth = state.savepoint_depth.saturating_sub(1);
            if state.savepoint_depth == 0 && !state.explicit_transaction {
                state.connection.execute_batch("PRAGMA foreign_keys=ON")?;
            }
        }
        StatementKind::Begin => state.explicit_transaction = true,
        StatementKind::Commit => {
            state.explicit_transaction = false;
            state.savepoint_depth = 0;
            state.connection.execute_batch("PRAGMA foreign_keys=ON")?;
        }
        StatementKind::Rollback => {
            if !sql.to_ascii_uppercase().contains(" TO ") {
                state.explicit_transaction = false;
                state.savepoint_depth = 0;
                state.connection.execute_batch("PRAGMA foreign_keys=ON")?;
            }
        }
        StatementKind::Other => {}
    }
    Ok(())
}

fn split_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut buffer = String::new();
    let mut single_quote = false;
    let mut double_quote = false;
    for character in sql.chars() {
        match character {
            '\'' if !double_quote => {
                single_quote = !single_quote;
                buffer.push(character);
            }
            '"' if !single_quote => {
                double_quote = !double_quote;
                buffer.push(character);
            }
            ';' if !single_quote && !double_quote => {
                if !buffer.trim().is_empty() {
                    statements.push(buffer.trim().to_string());
                }
                buffer.clear();
            }
            _ => buffer.push(character),
        }
    }
    if !buffer.trim().is_empty() {
        statements.push(buffer.trim().to_string());
    }
    statements
}

fn placeholder_count(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut anonymous = 0;
    let mut maximum_numbered = 0;
    while index < bytes.len() {
        if bytes[index] == b'?' {
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if start == index {
                anonymous += 1;
            } else if let Ok(number) = sql[start..index].parse::<usize>() {
                maximum_numbered = maximum_numbered.max(number);
            }
        } else {
            index += 1;
        }
    }
    maximum_numbered + anonymous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_resolves_inserted_row_ids_and_commits_atomically() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sql = SqlRpc::new(root.path())?;
        sql.dispatch(
            "Sql::query",
            &json!({
                "sql": "
                    CREATE TABLE batch_parents(id INTEGER PRIMARY KEY, name TEXT);
                    CREATE TABLE batch_children(
                        id INTEGER PRIMARY KEY,
                        parent_id INTEGER NOT NULL REFERENCES batch_parents(id),
                        name TEXT
                    )"
            }),
        )?;

        let result = sql.dispatch(
            "Sql::batch",
            &json!({
                "queries": [
                    {
                        "sql": "INSERT INTO batch_parents(name) VALUES (?)",
                        "params": ["parent"]
                    },
                    {
                        "sql": "INSERT INTO batch_children(parent_id, name) VALUES (?, ?)",
                        "params": [
                            {"type": "batch_last_rowid", "query": 0},
                            "child"
                        ]
                    }
                ]
            }),
        )?;
        assert_eq!(result, json!({"ok": true, "queries": 2}));

        let rows = sql.dispatch(
            "Sql::query",
            &json!({
                "sql": "
                    SELECT batch_parents.name, batch_children.name
                    FROM batch_children
                    JOIN batch_parents ON batch_parents.id = batch_children.parent_id"
            }),
        )?;
        assert_eq!(rows["rows"], json!([["parent", "child"]]));
        Ok(())
    }

    #[test]
    fn batch_rolls_back_all_queries_on_error() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sql = SqlRpc::new(root.path())?;
        sql.dispatch(
            "Sql::query",
            &json!({"sql": "CREATE TABLE batch_records(id INTEGER PRIMARY KEY, name TEXT UNIQUE)"}),
        )?;

        assert!(
            sql.dispatch(
                "Sql::batch",
                &json!({
                    "queries": [
                        {
                            "sql": "INSERT INTO batch_records(name) VALUES (?)",
                            "params": ["duplicate"]
                        },
                        {
                            "sql": "INSERT INTO batch_records(name) VALUES (?)",
                            "params": ["duplicate"]
                        }
                    ]
                }),
            )
            .is_err()
        );

        let count = sql.dispatch(
            "Sql::query",
            &json!({"sql": "SELECT COUNT(*) FROM batch_records"}),
        )?;
        assert_eq!(count["rows"], json!([[0]]));
        Ok(())
    }

    #[test]
    fn migration_is_idempotent_and_detects_drift() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sql = SqlRpc::new(root.path())?;
        let first = sql.dispatch(
            "Sql::migrate",
            &json!({
                "domain": "Test",
                "migrations": ["CREATE TABLE records(id INTEGER PRIMARY KEY)"]
            }),
        )?;
        assert_eq!(first, json!({"status": "ok", "applied": 1}));
        let repeated = sql.dispatch(
            "Sql::migrate",
            &json!({
                "domain": "Test",
                "migrations": ["CREATE TABLE changed(id INTEGER PRIMARY KEY)"]
            }),
        )?;
        assert_eq!(repeated["status"], "drift");
        Ok(())
    }

    #[test]
    fn migration_nests_inside_client_savepoint() -> Result<()> {
        let root = tempfile::tempdir()?;
        let sql = SqlRpc::new(root.path())?;
        let client = "browser-tab";
        sql.dispatch(
            "Sql::query",
            &json!({"client_id": client, "sql": "SAVEPOINT app_migration"}),
        )?;
        let migrated = sql.dispatch(
            "Sql::migrate",
            &json!({
                "client_id": client,
                "domain": "Nested",
                "migrations": ["CREATE TABLE nested_records(id INTEGER PRIMARY KEY)"]
            }),
        )?;
        assert_eq!(migrated, json!({"status": "ok", "applied": 1}));
        sql.dispatch(
            "Sql::query",
            &json!({"client_id": client, "sql": "RELEASE app_migration"}),
        )?;
        let table = sql.dispatch(
            "Sql::query",
            &json!({
                "client_id": client,
                "sql": "SELECT name FROM sqlite_master WHERE name = 'nested_records'"
            }),
        )?;
        assert_eq!(table["rows"], json!([["nested_records"]]));
        Ok(())
    }

    #[test]
    fn startup_repairs_orphaned_editor_items() -> Result<()> {
        let root = tempfile::tempdir()?;
        let database_dir = root.path().join(".zed");
        std::fs::create_dir_all(&database_dir)?;
        let path = database_dir.join("remote.sqlite");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE items (
                item_id INTEGER NOT NULL,
                workspace_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY (item_id, workspace_id)
            );
            CREATE TABLE editors (
                item_id INTEGER NOT NULL,
                workspace_id INTEGER NOT NULL,
                PRIMARY KEY (item_id, workspace_id)
            );
            INSERT INTO items VALUES (1, 1, 'Editor');
            INSERT INTO items VALUES (2, 1, 'Editor');
            INSERT INTO items VALUES (3, 1, 'Terminal');
            INSERT INTO editors VALUES (2, 1);",
        )?;
        drop(connection);

        let _sql = SqlRpc::new(root.path())?;
        let connection = Connection::open(path)?;
        let remaining = connection
            .prepare("SELECT item_id FROM items ORDER BY item_id")?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(remaining, vec![2, 3]);
        Ok(())
    }

    #[test]
    fn startup_migrates_legacy_sidebar_workspace_paths() -> Result<()> {
        let root = tempfile::tempdir()?;
        let database_dir = root.path().join(".zed");
        std::fs::create_dir_all(&database_dir)?;
        let path = database_dir.join("remote.sqlite");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE sidebar_threads (
                folder_paths TEXT,
                main_worktree_paths TEXT
            );
            CREATE TABLE sidebar_terminal_threads (
                working_directory TEXT,
                folder_paths TEXT,
                main_worktree_paths TEXT
            );
            INSERT INTO sidebar_threads VALUES (
                '/workspace/project-a
/workspace/project-b',
                '/workspace/project-a'
            );
            INSERT INTO sidebar_terminal_threads VALUES (
                '/workspace/project-a',
                '/workspace/project-a',
                '/home/dev/web/workspace'
            );",
        )?;
        drop(connection);

        let _sql = SqlRpc::new(root.path())?;
        let connection = Connection::open(path)?;
        let root = root.path();
        let thread_paths = connection.query_row(
            "SELECT folder_paths, main_worktree_paths FROM sidebar_threads",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        assert_eq!(
            thread_paths,
            (
                format!("{}/project-a\n{}/project-b", root.display(), root.display()),
                format!("{}/project-a", root.display())
            )
        );
        let terminal_paths = connection.query_row(
            "SELECT working_directory, folder_paths, main_worktree_paths
             FROM sidebar_terminal_threads",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        assert_eq!(
            terminal_paths,
            (
                format!("{}/project-a", root.display()),
                format!("{}/project-a", root.display()),
                "/home/dev/web/workspace".to_string()
            )
        );
        Ok(())
    }
}
