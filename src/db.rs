use std::path::Path;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::models::{
    Dependency, FileOwner, GameInstall, LoadoutEntry, ModKind, ModRelease, Profile, SaveSet,
};

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn in_memory() -> Result<Self> {
        let db = Self {
            conn: Connection::open_in_memory()?,
        };
        db.conn.pragma_update(None, "foreign_keys", "ON")?;
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY
            );
            INSERT OR IGNORE INTO schema_version(version) VALUES (1);

            CREATE TABLE IF NOT EXISTS game_installs (
                id TEXT PRIMARY KEY,
                library TEXT NOT NULL,
                root TEXT NOT NULL UNIQUE,
                manifest TEXT NOT NULL,
                build_id TEXT NOT NULL,
                phantom_liberty INTEGER NOT NULL,
                redmod INTEGER NOT NULL,
                writable INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS mods (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                version TEXT NOT NULL,
                kind TEXT NOT NULL,
                archive_sha256 TEXT NOT NULL,
                layer_path TEXT NOT NULL,
                source TEXT NOT NULL,
                installed_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS mod_files (
                mod_id TEXT NOT NULL REFERENCES mods(id) ON DELETE CASCADE,
                relative_path TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                size INTEGER NOT NULL,
                PRIMARY KEY(mod_id, relative_path)
            );
            CREATE TABLE IF NOT EXISTS dependencies (
                mod_id TEXT NOT NULL REFERENCES mods(id) ON DELETE CASCADE,
                requires_id TEXT NOT NULL,
                inferred INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(mod_id, requires_id)
            );
            CREATE TABLE IF NOT EXISTS profiles (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                game_install_id TEXT NOT NULL REFERENCES game_installs(id),
                game_build_id TEXT NOT NULL,
                runner TEXT NOT NULL,
                launch_args TEXT NOT NULL,
                environment TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS loadout_entries (
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                mod_id TEXT NOT NULL REFERENCES mods(id) ON DELETE CASCADE,
                priority INTEGER NOT NULL,
                requested_enabled INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY(profile_id, mod_id)
            );
            CREATE TABLE IF NOT EXISTS loadout_revisions (
                id TEXT PRIMARY KEY,
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                created_at TEXT NOT NULL,
                reason TEXT NOT NULL,
                snapshot TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY,
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                exit_code INTEGER,
                command TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS compatibility_overrides (
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                build_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY(profile_id, build_id)
            );
            CREATE TABLE IF NOT EXISTS save_sets (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS profile_save_sets (
                profile_id TEXT PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE,
                save_set_id TEXT NOT NULL REFERENCES save_sets(id) ON DELETE RESTRICT
            );
            "#,
        )?;
        Ok(())
    }

    pub fn upsert_game_install(&self, game: &GameInstall) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO game_installs
               (id, library, root, manifest, build_id, phantom_liberty, redmod, writable)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT(id) DO UPDATE SET
                 library=excluded.library, root=excluded.root, manifest=excluded.manifest,
                 build_id=excluded.build_id, phantom_liberty=excluded.phantom_liberty,
                 redmod=excluded.redmod, writable=excluded.writable"#,
            params![
                game.id,
                game.library.to_string_lossy(),
                game.root.to_string_lossy(),
                game.manifest.to_string_lossy(),
                game.build_id,
                game.phantom_liberty,
                game.redmod,
                game.writable
            ],
        )?;
        Ok(())
    }

    pub fn list_game_installs(&self) -> Result<Vec<GameInstall>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, library, root, manifest, build_id, phantom_liberty, redmod, writable
             FROM game_installs ORDER BY root",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(GameInstall {
                id: row.get(0)?,
                library: row.get::<_, String>(1)?.into(),
                root: row.get::<_, String>(2)?.into(),
                manifest: row.get::<_, String>(3)?.into(),
                build_id: row.get(4)?,
                phantom_liberty: row.get(5)?,
                redmod: row.get(6)?,
                writable: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn game_install(&self, id: &str) -> Result<Option<GameInstall>> {
        Ok(self
            .list_game_installs()?
            .into_iter()
            .find(|game| game.id == id))
    }

    pub fn insert_mod(&mut self, release: &ModRelease, files: &[FileOwner]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            r#"INSERT INTO mods
               (id, name, version, kind, archive_sha256, layer_path, source, installed_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            params![
                release.id,
                release.name,
                release.version,
                release.kind.as_str(),
                release.archive_sha256,
                release.layer_path.to_string_lossy(),
                release.source,
                release.installed_at.to_rfc3339(),
            ],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO mod_files(mod_id, relative_path, sha256, size)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for file in files {
                stmt.execute(params![
                    file.mod_id,
                    file.relative_path.to_string_lossy(),
                    file.sha256,
                    file.size
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_mods(&self) -> Result<Vec<ModRelease>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, version, kind, archive_sha256, layer_path, source, installed_at
             FROM mods ORDER BY lower(name), version",
        )?;
        let rows = stmt.query_map([], |row| {
            let at: String = row.get(7)?;
            Ok(ModRelease {
                id: row.get(0)?,
                name: row.get(1)?,
                version: row.get(2)?,
                kind: ModKind::parse(&row.get::<_, String>(3)?),
                archive_sha256: row.get(4)?,
                layer_path: row.get::<_, String>(5)?.into(),
                source: row.get(6)?,
                installed_at: DateTime::parse_from_rfc3339(&at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn mod_release(&self, id: &str) -> Result<Option<ModRelease>> {
        Ok(self.list_mods()?.into_iter().find(|item| item.id == id))
    }

    pub fn mod_by_archive_sha(&self, sha256: &str) -> Result<Option<ModRelease>> {
        Ok(self
            .list_mods()?
            .into_iter()
            .find(|item| item.archive_sha256 == sha256))
    }

    pub fn profiles_using_mod(&self, mod_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT p.name FROM loadout_entries e
             JOIN profiles p ON p.id = e.profile_id
             WHERE e.mod_id=?1
             ORDER BY lower(p.name)",
        )?;
        let rows = stmt.query_map([mod_id], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_mod(&self, id: &str) -> Result<()> {
        let changed = self
            .conn
            .execute("DELETE FROM mods WHERE id=?1", params![id])?;
        ensure!(changed == 1, "mod {id} was not in the library");
        Ok(())
    }

    pub fn files_for_mod(&self, mod_id: &str) -> Result<Vec<FileOwner>> {
        let mut stmt = self.conn.prepare(
            "SELECT mod_id, relative_path, sha256, size FROM mod_files
             WHERE mod_id=?1 ORDER BY relative_path",
        )?;
        let rows = stmt.query_map([mod_id], |row| {
            Ok(FileOwner {
                mod_id: row.get(0)?,
                relative_path: row.get::<_, String>(1)?.into(),
                sha256: row.get(2)?,
                size: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn add_dependency(&self, dependency: &Dependency) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO dependencies(mod_id, requires_id, inferred)
             VALUES (?1, ?2, ?3)",
            params![
                dependency.mod_id,
                dependency.requires_id,
                dependency.inferred
            ],
        )?;
        Ok(())
    }

    pub fn list_dependencies(&self) -> Result<Vec<Dependency>> {
        let mut stmt = self
            .conn
            .prepare("SELECT mod_id, requires_id, inferred FROM dependencies")?;
        let rows = stmt.query_map([], |row| {
            Ok(Dependency {
                mod_id: row.get(0)?,
                requires_id: row.get(1)?,
                inferred: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn create_profile(&self, profile: &Profile) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO profiles
               (id, name, game_install_id, game_build_id, runner, launch_args, environment, created_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            params![
                profile.id,
                profile.name,
                profile.game_install_id,
                profile.game_build_id,
                profile.runner,
                serde_json::to_string(&profile.launch_args)?,
                serde_json::to_string(&profile.environment)?,
                profile.created_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn list_profiles(&self) -> Result<Vec<Profile>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, game_install_id, game_build_id, runner,
                    launch_args, environment, created_at
             FROM profiles ORDER BY lower(name)",
        )?;
        let rows = stmt.query_map([], |row| {
            let args: String = row.get(5)?;
            let env: String = row.get(6)?;
            let at: String = row.get(7)?;
            Ok(Profile {
                id: row.get(0)?,
                name: row.get(1)?,
                game_install_id: row.get(2)?,
                game_build_id: row.get(3)?,
                runner: row.get(4)?,
                launch_args: serde_json::from_str(&args).unwrap_or_default(),
                environment: serde_json::from_str(&env).unwrap_or_default(),
                created_at: DateTime::parse_from_rfc3339(&at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn profile_by_name_or_id(&self, value: &str) -> Result<Option<Profile>> {
        Ok(self
            .list_profiles()?
            .into_iter()
            .find(|profile| profile.id == value || profile.name == value))
    }

    pub fn create_save_set(&self, save_set: &SaveSet) -> Result<()> {
        self.conn.execute(
            "INSERT INTO save_sets(id, name, created_at) VALUES (?1, ?2, ?3)",
            params![save_set.id, save_set.name, save_set.created_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn attach_save_set(&self, profile_id: &str, save_set_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO profile_save_sets(profile_id, save_set_id) VALUES (?1, ?2)
             ON CONFLICT(profile_id) DO UPDATE SET save_set_id=excluded.save_set_id",
            params![profile_id, save_set_id],
        )?;
        Ok(())
    }

    pub fn detach_save_set(&self, profile_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM profile_save_sets WHERE profile_id=?1",
            [profile_id],
        )?;
        Ok(())
    }

    pub fn save_set_for_profile(&self, profile_id: &str) -> Result<Option<SaveSet>> {
        self.conn
            .query_row(
                "SELECT s.id, s.name, s.created_at
                 FROM save_sets s JOIN profile_save_sets p ON p.save_set_id=s.id
                 WHERE p.profile_id=?1",
                [profile_id],
                |row| {
                    let created_at: String = row.get(2)?;
                    Ok(SaveSet {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        created_at: DateTime::parse_from_rfc3339(&created_at)
                            .map(|value| value.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn profile_save_sets(&self) -> Result<Vec<(String, SaveSet)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.profile_id, s.id, s.name, s.created_at
             FROM profile_save_sets p JOIN save_sets s ON s.id=p.save_set_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let created_at: String = row.get(3)?;
            Ok((
                row.get(0)?,
                SaveSet {
                    id: row.get(1)?,
                    name: row.get(2)?,
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map(|value| value.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_loadout_entry(&self, entry: &LoadoutEntry) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO loadout_entries(profile_id, mod_id, priority, requested_enabled)
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(profile_id, mod_id) DO UPDATE SET
                 priority=excluded.priority, requested_enabled=excluded.requested_enabled"#,
            params![
                entry.profile_id,
                entry.mod_id,
                entry.priority,
                entry.requested_enabled
            ],
        )?;
        Ok(())
    }

    pub fn loadout(&self, profile_id: &str) -> Result<Vec<LoadoutEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT profile_id, mod_id, priority, requested_enabled
             FROM loadout_entries WHERE profile_id=?1
             ORDER BY priority DESC, mod_id",
        )?;
        let rows = stmt.query_map([profile_id], |row| {
            Ok(LoadoutEntry {
                profile_id: row.get(0)?,
                mod_id: row.get(1)?,
                priority: row.get(2)?,
                requested_enabled: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn next_priority(&self, profile_id: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT max(priority) FROM loadout_entries WHERE profile_id=?1",
                [profile_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or(0)
            + 10)
    }

    pub fn disable_all_mods(&self, profile_id: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE loadout_entries SET requested_enabled=0
             WHERE profile_id=?1 AND requested_enabled!=0",
            [profile_id],
        )?)
    }

    pub fn revision(&self, profile_id: &str, reason: &str) -> Result<String> {
        let entries = self.loadout(profile_id)?;
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO loadout_revisions(id, profile_id, created_at, reason, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                profile_id,
                Utc::now().to_rfc3339(),
                reason,
                serde_json::to_string(&entries)?
            ],
        )?;
        Ok(id)
    }

    pub fn record_compatibility_override(&self, profile_id: &str, build_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO compatibility_overrides(profile_id, build_id, created_at)
             VALUES (?1, ?2, ?3)",
            params![profile_id, build_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn has_compatibility_override(&self, profile_id: &str, build_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM compatibility_overrides
                 WHERE profile_id=?1 AND build_id=?2",
                params![profile_id, build_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn start_run(&self, profile_id: &str, command: &str) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO runs(id, profile_id, started_at, command)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, profile_id, Utc::now().to_rfc3339(), command],
        )?;
        Ok(id)
    }

    pub fn finish_run(&self, run_id: &str, exit_code: Option<i32>) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET finished_at=?2, exit_code=?3 WHERE id=?1",
            params![run_id, Utc::now().to_rfc3339(), exit_code],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn database_round_trip() {
        let mut db = Database::in_memory().unwrap();
        let game = GameInstall {
            id: "g".into(),
            library: "/steam".into(),
            root: "/steam/game".into(),
            manifest: "/steam/app.acf".into(),
            build_id: "42".into(),
            phantom_liberty: true,
            redmod: false,
            writable: false,
        };
        db.upsert_game_install(&game).unwrap();
        let profile = Profile {
            id: "p".into(),
            name: "Default".into(),
            game_install_id: "g".into(),
            game_build_id: "42".into(),
            runner: "UMU-Proton".into(),
            launch_args: vec!["-modded".into()],
            environment: BTreeMap::new(),
            created_at: Utc::now(),
        };
        db.create_profile(&profile).unwrap();
        assert_eq!(db.list_game_installs().unwrap(), vec![game]);
        assert_eq!(db.list_profiles().unwrap()[0].name, "Default");
        let save_set = SaveSet {
            id: "saves".into(),
            name: "Modded".into(),
            created_at: Utc::now(),
        };
        db.create_save_set(&save_set).unwrap();
        db.attach_save_set("p", "saves").unwrap();
        assert_eq!(db.save_set_for_profile("p").unwrap(), Some(save_set));
        db.detach_save_set("p").unwrap();
        assert!(db.save_set_for_profile("p").unwrap().is_none());
        assert!(!db.has_compatibility_override("p", "43").unwrap());
        db.record_compatibility_override("p", "43").unwrap();
        assert!(db.has_compatibility_override("p", "43").unwrap());

        let release = ModRelease {
            id: "m".into(),
            name: "Test Mod".into(),
            version: "1".into(),
            kind: ModKind::Legacy,
            archive_sha256: "abc".into(),
            layer_path: "/managed/m".into(),
            source: "/inbox/m.zip".into(),
            installed_at: Utc::now(),
        };
        db.insert_mod(&release, &[]).unwrap();
        db.set_loadout_entry(&LoadoutEntry {
            profile_id: "p".into(),
            mod_id: "m".into(),
            priority: 10,
            requested_enabled: true,
        })
        .unwrap();
        assert_eq!(db.disable_all_mods("p").unwrap(), 1);
        assert!(!db.loadout("p").unwrap()[0].requested_enabled);
    }
}
