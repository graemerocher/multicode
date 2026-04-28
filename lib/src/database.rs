use std::path::{Path, PathBuf};

use diesel::{
    RunQueryDsl,
    r2d2::{ConnectionManager, CustomizeConnection, Pool},
    sql_query,
    sqlite::SqliteConnection,
};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

pub type SqlitePool = Pool<ConnectionManager<SqliteConnection>>;

/// Workspace metadata database. At the moment, this is only used for caching GitHub status.
#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn open_in_workspace(
        workspace_directory: impl AsRef<Path>,
    ) -> Result<Self, DatabaseError> {
        let database_path = workspace_directory
            .as_ref()
            .join(".multicode")
            .join("cache.sqlite");
        Self::open_path(database_path).await
    }

    pub async fn open_path(path: impl Into<PathBuf>) -> Result<Self, DatabaseError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let path_for_connection = path.clone();
        let pool = tokio::task::spawn_blocking(move || {
            let database_url = path_for_connection.to_string_lossy().into_owned();
            let manager = ConnectionManager::<SqliteConnection>::new(database_url);
            let pool = Pool::builder()
                .max_size(1)
                .connection_customizer(Box::new(SqliteConnectionCustomizer))
                .build(manager)?;
            let mut connection = pool.get()?;
            configure_database(&mut connection)?;
            connection.run_pending_migrations(MIGRATIONS)?;
            Ok::<_, DatabaseError>(pool)
        })
        .await
        .map_err(DatabaseError::Join)??;

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[derive(Debug, Clone, Copy)]
struct SqliteConnectionCustomizer;

impl CustomizeConnection<SqliteConnection, diesel::r2d2::Error> for SqliteConnectionCustomizer {
    fn on_acquire(&self, connection: &mut SqliteConnection) -> Result<(), diesel::r2d2::Error> {
        configure_connection(connection).map_err(diesel::r2d2::Error::QueryError)
    }
}

fn configure_connection(connection: &mut SqliteConnection) -> Result<(), diesel::result::Error> {
    sql_query("PRAGMA foreign_keys = ON;").execute(connection)?;
    sql_query("PRAGMA busy_timeout = 5000;").execute(connection)?;
    Ok(())
}

fn configure_database(connection: &mut SqliteConnection) -> Result<(), diesel::result::Error> {
    sql_query("PRAGMA journal_mode = WAL;").execute(connection)?;
    Ok(())
}

#[derive(Debug, diesel::QueryableByName)]
#[cfg(test)]
struct SqliteTableExistsRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    table_exists: bool,
}

#[derive(Debug, diesel::QueryableByName)]
#[cfg(test)]
struct SqliteForeignKeysPragmaRow {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    foreign_keys: i32,
}

#[derive(Debug, diesel::QueryableByName)]
#[cfg(test)]
struct SqliteBusyTimeoutPragmaRow {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    timeout: i32,
}

#[cfg(test)]
fn foreign_keys_enabled(connection: &mut SqliteConnection) -> Result<bool, diesel::result::Error> {
    let rows = sql_query("PRAGMA foreign_keys;").load::<SqliteForeignKeysPragmaRow>(connection)?;
    Ok(rows.first().is_some_and(|row| row.foreign_keys == 1))
}

#[cfg(test)]
fn busy_timeout_millis(connection: &mut SqliteConnection) -> Result<i32, diesel::result::Error> {
    let rows = sql_query("PRAGMA busy_timeout;").load::<SqliteBusyTimeoutPragmaRow>(connection)?;
    Ok(rows.first().map_or(0, |row| row.timeout))
}

#[cfg(test)]
fn table_exists(
    connection: &mut SqliteConnection,
    table_name: &str,
) -> Result<bool, diesel::result::Error> {
    let rows = sql_query(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?) AS table_exists",
    )
    .bind::<diesel::sql_types::Text, _>(table_name)
    .load::<SqliteTableExistsRow>(connection)?;
    Ok(rows.first().is_some_and(|row| row.table_exists))
}

#[derive(Debug)]
pub enum DatabaseError {
    Io(std::io::Error),
    Pool(diesel::r2d2::PoolError),
    Diesel(diesel::result::Error),
    Migration(Box<dyn std::error::Error + Send + Sync>),
    Join(tokio::task::JoinError),
}

impl From<std::io::Error> for DatabaseError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<diesel::r2d2::PoolError> for DatabaseError {
    fn from(value: diesel::r2d2::PoolError) -> Self {
        Self::Pool(value)
    }
}

impl From<diesel::result::Error> for DatabaseError {
    fn from(value: diesel::result::Error) -> Self {
        Self::Diesel(value)
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for DatabaseError {
    fn from(value: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self::Migration(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "multicode-database-{}-{}",
                std::process::id(),
                unique
            ));
            fs::create_dir_all(&path).expect("test dir should be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn open_in_workspace_creates_database_and_runs_migrations() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();

            let database = Database::open_in_workspace(root.path())
                .await
                .expect("database should open");

            let mut connection = database
                .pool()
                .get()
                .expect("database connection should be available");
            assert!(
                table_exists(&mut connection, "__diesel_schema_migrations")
                    .expect("diesel migrations table check should succeed")
            );
            assert!(
                table_exists(&mut connection, "github_link_statuses")
                    .expect("github status cache table check should succeed")
            );
            assert!(
                !table_exists(&mut connection, "workspace_metadata")
                    .expect("workspace table check should succeed")
            );
            sql_query("SELECT 1")
                .execute(&mut connection)
                .expect("database should respond");
        });
    }

    #[test]
    fn database_reopens_existing_file_without_losing_schema() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let path = root.path().join(".multicode").join("data.sqlite");

            let database = Database::open_path(&path)
                .await
                .expect("database should open first time");
            {
                let mut connection = database
                    .pool()
                    .get()
                    .expect("first database connection should be available");
                sql_query("SELECT 1")
                    .execute(&mut connection)
                    .expect("first database should respond");
            }
            drop(database);

            let reopened = Database::open_path(&path)
                .await
                .expect("database should reopen");
            let mut connection = reopened
                .pool()
                .get()
                .expect("reopened database connection should be available");
            sql_query("SELECT 1")
                .execute(&mut connection)
                .expect("reopened database should respond");
            assert!(
                table_exists(&mut connection, "__diesel_schema_migrations")
                    .expect("diesel migrations table should still exist")
            );
            assert!(
                table_exists(&mut connection, "github_link_statuses")
                    .expect("github status cache table should still exist")
            );
            assert!(
                !table_exists(&mut connection, "workspace_metadata")
                    .expect("workspace table check should succeed")
            );
        });
    }

    #[test]
    fn pool_customizer_enables_foreign_keys_for_each_connection() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let database = Database::open_in_workspace(root.path())
                .await
                .expect("database should open");
            let pool = database.pool().clone();

            let (first_enabled, second_enabled) = tokio::task::spawn_blocking(move || {
                let first_enabled = {
                    let mut first = pool.get()?;
                    foreign_keys_enabled(&mut first)?
                };
                let second_enabled = {
                    let mut second = pool.get()?;
                    foreign_keys_enabled(&mut second)?
                };
                Ok::<_, DatabaseError>((first_enabled, second_enabled))
            })
            .await
            .map_err(DatabaseError::Join)
            .expect("join should succeed")
            .expect("pragma query should succeed");

            assert!(
                first_enabled,
                "first pooled connection should enforce foreign keys"
            );
            assert!(
                second_enabled,
                "second pooled connection should enforce foreign keys"
            );
        });
    }

    #[test]
    fn pool_customizer_sets_busy_timeout_for_each_connection() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let database = Database::open_in_workspace(root.path())
                .await
                .expect("database should open");
            let pool = database.pool().clone();

            let (first_timeout, second_timeout) = tokio::task::spawn_blocking(move || {
                let first_timeout = {
                    let mut first = pool.get()?;
                    busy_timeout_millis(&mut first)?
                };
                let second_timeout = {
                    let mut second = pool.get()?;
                    busy_timeout_millis(&mut second)?
                };
                Ok::<_, DatabaseError>((first_timeout, second_timeout))
            })
            .await
            .map_err(DatabaseError::Join)
            .expect("join should succeed")
            .expect("pragma query should succeed");

            assert_eq!(
                first_timeout, 5000,
                "first pooled connection should set sqlite busy timeout"
            );
            assert_eq!(
                second_timeout, 5000,
                "second pooled connection should set sqlite busy timeout"
            );
        });
    }
}
