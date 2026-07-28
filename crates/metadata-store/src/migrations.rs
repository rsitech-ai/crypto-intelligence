//! Immutable, checksummed SQLite schema migrations.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{APPLICATION_ID, StoreError, map_sqlite};

pub const CURRENT_SCHEMA_VERSION: i64 = 1;

const INITIAL_SQL: &str = include_str!("../migrations/0001_initial.sql");
const INITIAL_NAME: &str = "initial_metadata_and_audit";

pub(crate) fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    let user_version =
        connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
    let has_manifest = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_schema
             WHERE type = 'table' AND name = 'schema_migrations'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;

    if !has_manifest {
        if user_version != 0 {
            return Err(StoreError::SchemaStateMismatch);
        }
        apply_initial(connection)?;
        return Ok(());
    }

    verify_applied(connection, user_version)
}

fn apply_initial(connection: &mut Connection) -> Result<(), StoreError> {
    let checksum = migration_checksum(INITIAL_SQL);
    let applied_at_ns = unix_time_ns()?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    transaction.execute_batch(INITIAL_SQL).map_err(map_sqlite)?;
    transaction
        .execute(
            "INSERT INTO schema_migrations(version, name, applied_at_ns, checksum)
             VALUES(?1, ?2, ?3, ?4)",
            params![
                CURRENT_SCHEMA_VERSION,
                INITIAL_NAME,
                applied_at_ns,
                checksum.as_slice()
            ],
        )
        .map_err(map_sqlite)?;
    transaction
        .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
        .map_err(map_sqlite)?;
    transaction
        .pragma_update(None, "application_id", APPLICATION_ID)
        .map_err(map_sqlite)?;
    transaction.commit().map_err(map_sqlite)
}

fn verify_applied(connection: &Connection, user_version: i64) -> Result<(), StoreError> {
    let maximum = connection
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .map_err(map_sqlite)?
        .unwrap_or(0);
    if maximum > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::UnknownSchemaVersion {
            found: maximum,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    if user_version != CURRENT_SCHEMA_VERSION || maximum != CURRENT_SCHEMA_VERSION {
        return Err(StoreError::SchemaStateMismatch);
    }

    let applied = connection
        .query_row(
            "SELECT name, checksum FROM schema_migrations WHERE version = ?1",
            [CURRENT_SCHEMA_VERSION],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(map_sqlite)?
        .ok_or(StoreError::MissingMigration {
            version: CURRENT_SCHEMA_VERSION,
        })?;
    if applied.0 != INITIAL_NAME {
        return Err(StoreError::MigrationNameMismatch {
            version: CURRENT_SCHEMA_VERSION,
        });
    }
    if applied.1.as_slice() != migration_checksum(INITIAL_SQL) {
        return Err(StoreError::MigrationChecksumMismatch {
            version: CURRENT_SCHEMA_VERSION,
        });
    }

    let count = connection
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(map_sqlite)?;
    if count != CURRENT_SCHEMA_VERSION {
        return Err(StoreError::SchemaStateMismatch);
    }
    Ok(())
}

fn migration_checksum(sql: &str) -> [u8; 32] {
    *blake3::hash(sql.as_bytes()).as_bytes()
}

fn unix_time_ns() -> Result<i64, StoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::SystemClockBeforeEpoch)?;
    i64::try_from(elapsed.as_nanos()).map_err(|_| StoreError::SystemTimeOverflow)
}

#[cfg(test)]
mod tests {
    use super::{INITIAL_SQL, migration_checksum};

    #[test]
    fn initial_migration_checksum_is_frozen() {
        assert_eq!(
            hex::encode(migration_checksum(INITIAL_SQL)),
            "aaf33bba4a2af2efeec442ae11513fa4338c5091657ccee07048aada184486d4"
        );
    }
}
