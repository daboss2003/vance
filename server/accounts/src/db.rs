//! SQLite pool + migrations. SQL is kept portable toward Postgres (spec §12);
//! swapping drivers later is a contained change behind this module.

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;

pub type Db = SqlitePool;

pub async fn init(db_path: &Path) -> Result<Db, sqlx::Error> {
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;

    // Runtime migrator over the crate's migrations dir (absolute path baked at
    // compile time, so it's cwd-independent). Avoids the compile-time `migrate!`
    // macro + its `macros` feature.
    let migrator = Migrator::new(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/migrations"
    )))
    .await?;
    migrator.run(&pool).await?;
    Ok(pool)
}
