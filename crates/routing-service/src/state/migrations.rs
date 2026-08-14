use tokio_rusqlite::rusqlite::{
    Connection, OptionalExtension, Result, TransactionBehavior, params,
};

const SCHEMA: &str = include_str!("schema.sql");

pub const SCHEMA_VERSION: u32 = 2;

pub fn migrate(connection: &mut Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS metadata (
           key TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );",
    )?;
    let version = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema-version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)
        })
        .transpose()?;

    match version {
        None | Some(SCHEMA_VERSION) => connection.execute_batch(SCHEMA)?,
        Some(1) => migrate_v1(connection)?,
        Some(_) => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
    }
    Ok(())
}

fn migrate_v1(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE credentials (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target = 'codex'),
           bearer_token TEXT NOT NULL
         );
         CREATE TABLE providers_v2 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target = 'codex'),
           position INTEGER NOT NULL CHECK (position >= 0),
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           name TEXT NOT NULL,
           base_url TEXT NOT NULL,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol = 'openai-responses'),
           credential_id TEXT REFERENCES credentials(id) ON DELETE SET NULL,
           provenance_kind TEXT,
           provenance_key TEXT,
           generated_owner_id TEXT
         );",
    )?;

    let providers = {
        let mut statement = transaction.prepare(
            "SELECT p.id, p.target, p.name, p.base_url, p.model, c.bearer_token
             FROM providers p
             LEFT JOIN provider_credentials c ON c.provider_id = p.id
             ORDER BY p.rowid",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?
    };
    for (position, (id, target, name, base_url, model, credential)) in
        providers.into_iter().enumerate()
    {
        let credential_id = credential.as_ref().map(|_| id.as_str());
        if let Some(credential) = credential {
            transaction.execute(
                "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, ?2, ?3)",
                params![id, target, credential],
            )?;
        }
        transaction.execute(
            "INSERT INTO providers_v2
             (id, target, position, provider_revision, name, base_url, model, protocol, credential_id,
              provenance_kind, provenance_key, generated_owner_id)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, 'openai-responses', ?7, NULL, NULL, NULL)",
            params![id, target, position as u32, name, base_url, model, credential_id],
        )?;
    }
    transaction.execute_batch(
        "DROP TABLE provider_credentials;
         DROP TABLE providers;
         ALTER TABLE providers_v2 RENAME TO providers;
         UPDATE metadata SET value = '2' WHERE key = 'schema-version';",
    )?;
    transaction.commit()?;
    connection.execute_batch(SCHEMA)?;
    Ok(())
}
