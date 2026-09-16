use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use chrono::Utc;
use uuid::Uuid;

use crate::{
    db::Database,
    models::{GameInstall, Profile},
    paths::AppPaths,
};

pub fn create(
    db: &Database,
    paths: &AppPaths,
    game: &GameInstall,
    name: &str,
    runner: &str,
) -> Result<Profile> {
    let name = name.trim();
    ensure!(!name.is_empty(), "profile name cannot be empty");
    let profile = Profile {
        id: Uuid::new_v4().to_string(),
        name: name.into(),
        game_install_id: game.id.clone(),
        game_build_id: game.build_id.clone(),
        runner: runner.into(),
        launch_args: Vec::new(),
        environment: BTreeMap::new(),
        created_at: Utc::now(),
    };
    db.create_profile(&profile)?;
    std::fs::create_dir_all(paths.profile_dir(&profile.id))?;
    Ok(profile)
}
