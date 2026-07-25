//! WASM migrations against server-side SQLite.
//!
//! Same control flow as native `migrations.rs`, but SQL runs via the remote
//! HTTP bridge instead of libsqlite3.

use anyhow::{Context as _, Result};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::connection::Connection;
use crate::remote_sql;

/// Prevent infinite reset loops if migrations keep failing after wipe.
static DID_RESET_FOR_DRIFT: AtomicBool = AtomicBool::new(false);

impl Connection {
    /// Migrate the database for the given domain.
    pub fn migrate(
        &self,
        domain: &'static str,
        migrations: &[&'static str],
        should_allow_migration_change: &mut dyn FnMut(usize, &str, &str) -> bool,
    ) -> Result<()> {
        match self.migrate_inner(domain, migrations, should_allow_migration_change) {
            Ok(()) => Ok(()),
            Err(err) => {
                let msg = format!("{err:#}");
                // Dev / smoke-test pollution: stored migration text no longer
                // matches the binary. Wipe the shared server DB once and retry.
                if msg.contains("Migration changed for")
                    && !DID_RESET_FOR_DRIFT.swap(true, Ordering::SeqCst)
                {
                    log::warn!(
                        "remote SQLite migration drift for {domain}; resetting server DB and retrying: {msg}"
                    );
                    remote_sql::reset_database().context("Sql::reset after migration drift")?;
                    return self.migrate_inner(domain, migrations, should_allow_migration_change);
                }
                Err(err)
            }
        }
    }

    fn migrate_inner(
        &self,
        domain: &'static str,
        migrations: &[&'static str],
        should_allow_migration_change: &mut dyn FnMut(usize, &str, &str) -> bool,
    ) -> Result<()> {
        let drift = remote_sql::migrate_domain(domain, migrations, &[])
            .with_context(|| format!("migrating {domain}"))?;
        if drift.is_empty() {
            return Ok(());
        }

        let mut allowed_changes = Vec::with_capacity(drift.len());
        for change in drift {
            if should_allow_migration_change(change.index, &change.stored, &change.proposed) {
                allowed_changes.push(change.index);
            } else {
                anyhow::bail!(
                    "Migration changed for {domain} at step {}\n\n\
                     Stored migration:\n{}\n\n\
                     Proposed migration:\n{}",
                    change.index,
                    change.stored,
                    change.proposed
                );
            }
        }

        remote_sql::migrate_domain(domain, migrations, &allowed_changes)
            .with_context(|| format!("migrating {domain} with allowed changes"))?;
        Ok(())
    }
}
