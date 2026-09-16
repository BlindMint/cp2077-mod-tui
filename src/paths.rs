use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::APP_ID;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let config = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let data = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        let cache = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"));
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join(APP_ID).join("runtime"));
        Ok(Self {
            config_dir: config.join(APP_ID),
            data_dir: data.join(APP_ID),
            cache_dir: cache.join(APP_ID),
            runtime_dir: runtime.join(APP_ID),
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for path in [
            &self.config_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.runtime_dir,
            &self.archives_dir(),
            &self.releases_dir(),
            &self.profiles_dir(),
            &self.backups_dir(),
            &self.save_sets_dir(),
        ] {
            fs::create_dir_all(path)
                .with_context(|| format!("create application directory {}", path.display()))?;
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("library.sqlite3")
    }
    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn archives_dir(&self) -> PathBuf {
        self.data_dir.join("archives")
    }
    pub fn releases_dir(&self) -> PathBuf {
        self.data_dir.join("releases")
    }
    pub fn profiles_dir(&self) -> PathBuf {
        self.data_dir.join("profiles")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }
    pub fn save_sets_dir(&self) -> PathBuf {
        self.data_dir.join("save-sets")
    }
    pub fn save_set_dir(&self, save_set_id: &str) -> PathBuf {
        self.save_sets_dir().join(save_set_id)
    }
    pub fn profile_dir(&self, profile_id: &str) -> PathBuf {
        self.profiles_dir().join(profile_id)
    }
    pub fn is_managed_path(&self, path: &Path) -> bool {
        path.starts_with(&self.data_dir)
            || path.starts_with(&self.cache_dir)
            || path.starts_with(&self.runtime_dir)
    }
}
