//! Game-owned durable progression state, separate from the engine world save.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use spall_protocol::{
    InventoryEntry, PlayerId, ProgressionOutcome, ProgressionRejectCode, ProgressionRequest,
    ProgressionResponse, Record,
};

use crate::game::{self, Inventory, ItemStack};

const STORE_SCHEMA_VERSION: i64 = 1;
const RESPONSE_SCHEMA_VERSION: i64 = 1;
const REQUEST_CACHE_LIMIT: i64 = 128;

#[derive(Debug, thiserror::Error)]
pub enum ProgressionStoreError {
    #[error("progression database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("progression response codec error: {0}")]
    Codec(String),
    #[error("unsupported progression store schema {0}")]
    UnsupportedSchema(i64),
    #[error("request ID exceeds the durable request ledger range")]
    RequestIdOutOfRange,
    #[error("persisted inventory snapshot is invalid")]
    InvalidInventory,
    #[error("persisted progression response failed protocol validation")]
    InvalidResponse,
    #[error("progression database writer stopped")]
    WriterStopped,
    #[error("could not start progression database writer: {0}")]
    WriterStart(String),
}

/// One SQLite writer for stable-player inventory snapshots and craft request
/// receipts. Each craft result, inventory replacement, request receipt, and
/// high-water mark commits in one SQLite transaction.
pub struct ProgressionStore {
    commands: std::sync::mpsc::SyncSender<ProgressionCommand>,
}

enum ProgressionCommand {
    Load {
        player_id: PlayerId,
        reply: std::sync::mpsc::SyncSender<Result<Inventory, ProgressionStoreError>>,
    },
    Execute {
        player_id: PlayerId,
        request: ProgressionRequest,
        process: fn(&mut Inventory, ProgressionRequest) -> ProgressionResponse,
        reply: std::sync::mpsc::SyncSender<Result<ProgressionResponse, ProgressionStoreError>>,
    },
    RecordCut {
        player_id: PlayerId,
        request_id: u64,
        removed: std::collections::BTreeMap<spall_core::MaterialId, u64>,
        reply: std::sync::mpsc::SyncSender<Result<Vec<ItemStack>, ProgressionStoreError>>,
    },
}

struct ProgressionDatabase {
    connection: Connection,
}

impl ProgressionDatabase {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ProgressionStoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let journal_mode: String =
            connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if synchronous != 2 {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > STORE_SCHEMA_VERSION {
            return Err(ProgressionStoreError::UnsupportedSchema(version));
        }
        if version == 0 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE player_inventory (
                    player_id BLOB PRIMARY KEY NOT NULL CHECK(length(player_id) = 16),
                    revision BLOB NOT NULL CHECK(length(revision) = 8)
                 );
                 CREATE TABLE inventory_stack (
                    player_id BLOB NOT NULL REFERENCES player_inventory(player_id) ON DELETE CASCADE,
                    item_id INTEGER NOT NULL CHECK(item_id BETWEEN 1 AND 65535),
                    count INTEGER NOT NULL CHECK(count BETWEEN 1 AND 4294967295),
                    PRIMARY KEY(player_id, item_id)
                 );
                 CREATE TABLE progression_request (
                    player_id BLOB NOT NULL CHECK(length(player_id) = 16),
                    request_id INTEGER NOT NULL CHECK(request_id > 0),
                    response_schema INTEGER NOT NULL,
                    response BLOB NOT NULL,
                    PRIMARY KEY(player_id, request_id)
                 );
                 CREATE TABLE progression_cursor (
                    player_id BLOB PRIMARY KEY NOT NULL CHECK(length(player_id) = 16),
                    high_water INTEGER NOT NULL CHECK(high_water >= 0)
                 );
                 CREATE TABLE committed_harvest (
                    player_id BLOB NOT NULL CHECK(length(player_id) = 16),
                    action_request_id INTEGER NOT NULL CHECK(action_request_id > 0),
                    PRIMARY KEY(player_id, action_request_id)
                 );
                 PRAGMA user_version=1;
                 COMMIT;",
            )?;
        }
        Ok(Self { connection })
    }

    pub fn load_inventory(&self, player_id: PlayerId) -> Result<Inventory, ProgressionStoreError> {
        load_inventory(&self.connection, player_id)
    }

    /// Applies a request to a private inventory copy, then publishes inventory
    /// and its replayable response together only after SQLite commits.
    pub fn execute_request(
        &mut self,
        player_id: PlayerId,
        request: ProgressionRequest,
        process: fn(&mut Inventory, ProgressionRequest) -> ProgressionResponse,
    ) -> Result<ProgressionResponse, ProgressionStoreError> {
        let request_id = i64::try_from(request.request_id)
            .ok()
            .filter(|id| *id > 0)
            .ok_or(ProgressionStoreError::RequestIdOutOfRange)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pid = player_id.0.as_slice();
        if let Some((schema, bytes)) = tx
            .query_row(
                "SELECT response_schema, response FROM progression_request
                 WHERE player_id=?1 AND request_id=?2",
                params![pid, request_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
        {
            if schema != RESPONSE_SCHEMA_VERSION {
                return Err(ProgressionStoreError::UnsupportedSchema(schema));
            }
            let response: ProgressionResponse = postcard::from_bytes(&bytes)
                .map_err(|error| ProgressionStoreError::Codec(error.to_string()))?;
            response
                .validate()
                .map_err(|_| ProgressionStoreError::InvalidResponse)?;
            tx.commit()?;
            return Ok(response);
        }

        let inventory = load_inventory(&tx, player_id)?;
        let high_water = tx
            .query_row(
                "SELECT high_water FROM progression_cursor WHERE player_id=?1",
                [pid],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
        if request_id <= high_water {
            let response = inventory_response(
                request.request_id,
                ProgressionOutcome::Rejected(ProgressionRejectCode::Unavailable),
                &inventory,
            );
            tx.commit()?;
            return Ok(response);
        }

        let mut candidate = inventory.clone();
        let response = process(&mut candidate, request);
        if response.request_id != request.request_id
            || response.catalog_version == 0
            || response.validate().is_err()
        {
            return Err(ProgressionStoreError::InvalidResponse);
        }
        write_inventory(&tx, player_id, &candidate)?;
        let response_bytes = postcard::to_stdvec(&response)
            .map_err(|error| ProgressionStoreError::Codec(error.to_string()))?;
        tx.execute(
            "INSERT INTO progression_request(player_id, request_id, response_schema, response)
             VALUES(?1, ?2, ?3, ?4)",
            params![pid, request_id, RESPONSE_SCHEMA_VERSION, response_bytes],
        )?;
        tx.execute(
            "INSERT INTO progression_cursor(player_id, high_water) VALUES(?1, ?2)
             ON CONFLICT(player_id) DO UPDATE SET high_water=excluded.high_water",
            params![pid, request_id],
        )?;
        tx.execute(
            "DELETE FROM progression_request WHERE player_id=?1 AND request_id NOT IN
             (SELECT request_id FROM progression_request WHERE player_id=?1
              ORDER BY request_id DESC LIMIT ?2)",
            params![pid, REQUEST_CACHE_LIMIT],
        )?;
        tx.commit()?;
        Ok(response)
    }

    /// Durably awards drops from a committed cut once per player/action ID.
    /// The world journal outbox delivers committed edits at least once. This
    /// transaction deduplicates the stable player/action ID before granting.
    pub fn record_committed_cut(
        &mut self,
        player_id: PlayerId,
        action_request_id: u64,
        removed: &std::collections::BTreeMap<spall_core::MaterialId, u64>,
    ) -> Result<Vec<ItemStack>, ProgressionStoreError> {
        let request_id = i64::try_from(action_request_id)
            .ok()
            .filter(|id| *id > 0)
            .ok_or(ProgressionStoreError::RequestIdOutOfRange)?;
        let drops = cut_drops(removed)?;
        if drops.is_empty() {
            return Ok(drops);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pid = player_id.0.as_slice();
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM committed_harvest
             WHERE player_id=?1 AND action_request_id=?2)",
            params![pid, request_id],
            |row| row.get(0),
        )?;
        if exists {
            tx.commit()?;
            return Ok(Vec::new());
        }
        let mut inventory = load_inventory(&tx, player_id)?;
        inventory
            .grant_many(drops.iter().copied())
            .map_err(|error| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
                    "{error:?}"
                ))))
            })?;
        write_inventory(&tx, player_id, &inventory)?;
        tx.execute(
            "INSERT INTO committed_harvest(player_id, action_request_id) VALUES(?1, ?2)",
            params![pid, request_id],
        )?;
        tx.commit()?;
        Ok(drops)
    }
}

impl ProgressionStore {
    /// Opens the game-owned progression file and starts its single bounded
    /// SQLite writer thread. The caller waits for each authoritative request's
    /// commit result, but filesystem work itself never runs on the simulation
    /// thread.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ProgressionStoreError> {
        let database = ProgressionDatabase::open(path)?;
        let (commands, receiver) = std::sync::mpsc::sync_channel::<ProgressionCommand>(64);
        std::thread::Builder::new()
            .name("sandbox-progression-writer".into())
            .spawn(move || {
                let mut database = database;
                while let Ok(command) = receiver.recv() {
                    match command {
                        ProgressionCommand::Load { player_id, reply } => {
                            let _ = reply.send(database.load_inventory(player_id));
                        }
                        ProgressionCommand::Execute {
                            player_id,
                            request,
                            process,
                            reply,
                        } => {
                            let _ =
                                reply.send(database.execute_request(player_id, request, process));
                        }
                        ProgressionCommand::RecordCut {
                            player_id,
                            request_id,
                            removed,
                            reply,
                        } => {
                            let _ = reply.send(
                                database.record_committed_cut(player_id, request_id, &removed),
                            );
                        }
                    }
                }
            })
            .map_err(|error| ProgressionStoreError::WriterStart(error.to_string()))?;
        Ok(Self { commands })
    }

    pub fn load_inventory(&self, player_id: PlayerId) -> Result<Inventory, ProgressionStoreError> {
        let (reply, response) = std::sync::mpsc::sync_channel(1);
        self.commands
            .send(ProgressionCommand::Load { player_id, reply })
            .map_err(|_| ProgressionStoreError::WriterStopped)?;
        response
            .recv()
            .map_err(|_| ProgressionStoreError::WriterStopped)?
    }

    pub fn execute_request(
        &self,
        player_id: PlayerId,
        request: ProgressionRequest,
        process: fn(&mut Inventory, ProgressionRequest) -> ProgressionResponse,
    ) -> Result<ProgressionResponse, ProgressionStoreError> {
        let (reply, response) = std::sync::mpsc::sync_channel(1);
        self.commands
            .send(ProgressionCommand::Execute {
                player_id,
                request,
                process,
                reply,
            })
            .map_err(|_| ProgressionStoreError::WriterStopped)?;
        response
            .recv()
            .map_err(|_| ProgressionStoreError::WriterStopped)?
    }

    pub fn record_committed_cut(
        &self,
        player_id: PlayerId,
        request_id: u64,
        removed: &std::collections::BTreeMap<spall_core::MaterialId, u64>,
    ) -> Result<Vec<ItemStack>, ProgressionStoreError> {
        let (reply, response) = std::sync::mpsc::sync_channel(1);
        self.commands
            .send(ProgressionCommand::RecordCut {
                player_id,
                request_id,
                removed: removed.clone(),
                reply,
            })
            .map_err(|_| ProgressionStoreError::WriterStopped)?;
        response
            .recv()
            .map_err(|_| ProgressionStoreError::WriterStopped)?
    }
}

fn load_inventory(
    connection: &Connection,
    player_id: PlayerId,
) -> Result<Inventory, ProgressionStoreError> {
    let pid = player_id.0.as_slice();
    let Some(revision_bytes) = connection
        .query_row(
            "SELECT revision FROM player_inventory WHERE player_id=?1",
            [pid],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
    else {
        return Ok(Inventory::default());
    };
    let revision = u64::from_be_bytes(
        revision_bytes
            .as_slice()
            .try_into()
            .map_err(|_| ProgressionStoreError::InvalidInventory)?,
    );
    let mut statement = connection.prepare(
        "SELECT item_id, count FROM inventory_stack WHERE player_id=?1 ORDER BY item_id",
    )?;
    let rows = statement.query_map([pid], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut stacks = Vec::new();
    for row in rows {
        let (item, count) = row?;
        stacks.push(ItemStack {
            item: game::ItemId(u16::try_from(item).map_err(|_| rusqlite::Error::InvalidQuery)?),
            count: u32::try_from(count).map_err(|_| rusqlite::Error::InvalidQuery)?,
        });
    }
    Inventory::restore(revision, stacks).map_err(|_| ProgressionStoreError::InvalidInventory)
}

fn write_inventory(
    tx: &Transaction<'_>,
    player_id: PlayerId,
    inventory: &Inventory,
) -> Result<(), rusqlite::Error> {
    let pid = player_id.0.as_slice();
    tx.execute(
        "INSERT INTO player_inventory(player_id, revision) VALUES(?1, ?2)
         ON CONFLICT(player_id) DO UPDATE SET revision=excluded.revision",
        params![pid, inventory.revision().to_be_bytes().as_slice()],
    )?;
    tx.execute("DELETE FROM inventory_stack WHERE player_id=?1", [pid])?;
    for stack in inventory.stacks() {
        tx.execute(
            "INSERT INTO inventory_stack(player_id, item_id, count) VALUES(?1, ?2, ?3)",
            params![pid, stack.item.0, stack.count],
        )?;
    }
    Ok(())
}

fn inventory_response(
    request_id: u64,
    outcome: ProgressionOutcome,
    inventory: &Inventory,
) -> ProgressionResponse {
    ProgressionResponse {
        request_id,
        catalog_version: game::RECIPE_CATALOG_VERSION,
        inventory_revision: inventory.revision(),
        outcome,
        inventory: inventory
            .stacks()
            .map(|stack| InventoryEntry {
                item_id: stack.item.0,
                count: stack.count,
            })
            .collect(),
    }
}

fn cut_drops(
    removed: &std::collections::BTreeMap<spall_core::MaterialId, u64>,
) -> Result<Vec<ItemStack>, ProgressionStoreError> {
    let mut drops = Vec::new();
    for (material, cells_per_item, item) in [
        (game::materials::WOOD, 16_u64, game::items::WOOD_LOG),
        (game::materials::STONE, 16_u64, game::items::STONE_CHUNK),
    ] {
        let count = removed.get(&material).copied().unwrap_or(0) / cells_per_item;
        if count > 0 {
            let count = u32::try_from(count)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            drops.push(ItemStack { item, count });
        }
    }
    Ok(drops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_protocol::ProgressionOperation;

    struct TestDatabasePath(std::path::PathBuf);

    impl TestDatabasePath {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "spall-progression-{}-{unique}.sqlite",
                std::process::id()
            )))
        }
    }

    impl Drop for TestDatabasePath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("sqlite-wal"));
            let _ = std::fs::remove_file(self.0.with_extension("sqlite-shm"));
        }
    }

    fn player(byte: u8) -> PlayerId {
        PlayerId([byte; 16])
    }

    fn craft_one_plank(
        inventory: &mut Inventory,
        request: ProgressionRequest,
    ) -> ProgressionResponse {
        let result = match request.operation {
            ProgressionOperation::InspectInventory => ProgressionOutcome::Inventory,
            ProgressionOperation::Craft {
                recipe_id,
                batch_count,
            } if request.catalog_version == game::RECIPE_CATALOG_VERSION
                && request.expected_inventory_revision == inventory.revision() =>
            {
                let catalog = game::recipe_catalog();
                match catalog.stage(
                    inventory,
                    game::CraftRequest {
                        recipe: game::RecipeId(recipe_id),
                        batch_count,
                        expected_catalog_version: request.catalog_version,
                        expected_inventory_revision: request.expected_inventory_revision,
                    },
                ) {
                    Ok(transaction) => match inventory.commit(transaction) {
                        Ok(_) => ProgressionOutcome::Crafted,
                        Err(_) => ProgressionOutcome::Rejected(ProgressionRejectCode::Unavailable),
                    },
                    Err(_) => ProgressionOutcome::Rejected(ProgressionRejectCode::Unavailable),
                }
            }
            ProgressionOperation::Craft { .. } => {
                ProgressionOutcome::Rejected(ProgressionRejectCode::InventoryRevision)
            }
        };
        inventory_response(request.request_id, result, inventory)
    }

    fn craft_request(id: u64, revision: u64) -> ProgressionRequest {
        ProgressionRequest {
            request_id: id,
            catalog_version: game::RECIPE_CATALOG_VERSION,
            expected_inventory_revision: revision,
            operation: ProgressionOperation::Craft {
                recipe_id: game::recipe_ids::SAW_PLANKS.0,
                batch_count: 1,
            },
        }
    }

    #[test]
    fn committed_harvest_is_player_scoped_durable_and_idempotent() {
        let path = TestDatabasePath::new();
        let wood_cells = [(game::materials::WOOD, 32)].into();
        {
            let mut database = ProgressionDatabase::open(&path.0).unwrap();
            assert_eq!(
                database
                    .record_committed_cut(player(1), 7, &wood_cells)
                    .unwrap(),
                [ItemStack {
                    item: game::items::WOOD_LOG,
                    count: 2,
                }]
            );
            assert!(
                database
                    .record_committed_cut(player(1), 7, &wood_cells)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                database.load_inventory(player(2)).unwrap(),
                Inventory::default()
            );
        }

        let database = ProgressionDatabase::open(&path.0).unwrap();
        let restored = database.load_inventory(player(1)).unwrap();
        assert_eq!(restored.count(game::items::WOOD_LOG), 2);
        assert_eq!(restored.revision(), 1);
    }

    #[test]
    fn crafting_commit_and_exact_retry_survive_database_reopen() {
        let path = TestDatabasePath::new();
        let request = craft_request(1, 1);
        let first_response;
        {
            let mut database = ProgressionDatabase::open(&path.0).unwrap();
            let wood_cells = [(game::materials::WOOD, 16)].into();
            database
                .record_committed_cut(player(3), 11, &wood_cells)
                .unwrap();
            first_response = database
                .execute_request(player(3), request, craft_one_plank)
                .unwrap();
            assert_eq!(first_response.outcome, ProgressionOutcome::Crafted);
            assert_eq!(first_response.inventory_revision, 2);
        }

        let mut database = ProgressionDatabase::open(&path.0).unwrap();
        let retried = database
            .execute_request(player(3), request, craft_one_plank)
            .unwrap();
        assert_eq!(retried, first_response);
        let inventory = database.load_inventory(player(3)).unwrap();
        assert_eq!(inventory.count(game::items::WOOD_LOG), 0);
        assert_eq!(inventory.count(game::items::WOOD_PLANK), 4);
        assert_eq!(inventory.revision(), 2);
    }

    #[test]
    fn an_evicted_old_request_cannot_be_applied_again() {
        let path = TestDatabasePath::new();
        let mut database = ProgressionDatabase::open(&path.0).unwrap();
        let wood_cells = [(game::materials::WOOD, 16)].into();
        database
            .record_committed_cut(player(4), 13, &wood_cells)
            .unwrap();
        let original = database
            .execute_request(player(4), craft_request(1, 1), craft_one_plank)
            .unwrap();

        for request_id in 2..=REQUEST_CACHE_LIMIT as u64 + 1 {
            database
                .execute_request(
                    player(4),
                    ProgressionRequest {
                        request_id,
                        catalog_version: game::RECIPE_CATALOG_VERSION,
                        expected_inventory_revision: 2,
                        operation: ProgressionOperation::InspectInventory,
                    },
                    craft_one_plank,
                )
                .unwrap();
        }

        let stale = database
            .execute_request(player(4), craft_request(1, 1), craft_one_plank)
            .unwrap();
        assert_ne!(stale, original);
        assert_eq!(
            stale.outcome,
            ProgressionOutcome::Rejected(ProgressionRejectCode::Unavailable)
        );
        let inventory = database.load_inventory(player(4)).unwrap();
        assert_eq!(inventory.count(game::items::WOOD_PLANK), 4);
        assert_eq!(inventory.revision(), 2);
    }
}
