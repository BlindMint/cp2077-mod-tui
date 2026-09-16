use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use uuid::Uuid;

use crate::{
    backup,
    db::Database,
    doctor,
    models::{GameInstall, Profile, SaveSet},
    paths::AppPaths,
};

const SAVE_RELATIVE: &str =
    "prefix/drive_c/users/steamuser/Saved Games/CD Projekt Red/Cyberpunk 2077";

#[derive(Debug)]
pub struct ShareResult {
    pub save_set: SaveSet,
    pub detached_target: Option<PathBuf>,
}

#[derive(Debug)]
pub struct TransferResult {
    pub save_slots: usize,
    pub backup: Option<PathBuf>,
}

pub fn profile_save_dir(paths: &AppPaths, profile_id: &str) -> PathBuf {
    paths.profile_dir(profile_id).join(SAVE_RELATIVE)
}

pub fn managed_save_dir(paths: &AppPaths, save_set_id: &str) -> PathBuf {
    paths.save_set_dir(save_set_id).join("Cyberpunk 2077")
}

pub fn steam_save_dir(game: &GameInstall) -> PathBuf {
    game.library
        .join("steamapps/compatdata/1091500/pfx/drive_c/users/steamuser")
        .join("Saved Games/CD Projekt Red/Cyberpunk 2077")
}

pub fn import_from_steam(
    db: &Database,
    paths: &AppPaths,
    profile: &Profile,
    game: &GameInstall,
) -> Result<TransferResult> {
    ensure_transfer_idle(paths, profile)?;
    let source = steam_save_dir(game);
    let save_slots = count_save_slots(&source)?;
    ensure!(
        save_slots > 0,
        "Steam save directory contains no Cyberpunk save slots: {}",
        source.display()
    );

    let (target, backup) = if let Some(save_set) = db.save_set_for_profile(&profile.id)? {
        ensure_link(db, paths, profile)?;
        let backup = backup::create_save_set(paths, &save_set.id, "before-steam-import")?;
        (managed_save_dir(paths, &save_set.id), Some(backup))
    } else {
        let backup = backup::create(paths, &profile.id, "before-steam-import")?;
        (profile_save_dir(paths, &profile.id), Some(backup))
    };
    replace_save_tree(&source, &target, false)?;
    Ok(TransferResult { save_slots, backup })
}

pub fn export_to_steam(
    db: &Database,
    paths: &AppPaths,
    profile: &Profile,
    game: &GameInstall,
) -> Result<TransferResult> {
    ensure_transfer_idle(paths, profile)?;
    let source = if let Some(save_set) = db.save_set_for_profile(&profile.id)? {
        ensure_link(db, paths, profile)?;
        managed_save_dir(paths, &save_set.id)
    } else {
        profile_save_dir(paths, &profile.id)
    };
    let save_slots = count_save_slots(&source)?;
    ensure!(
        save_slots > 0,
        "profile contains no Cyberpunk save slots: {}",
        source.display()
    );

    let target = steam_save_dir(game);
    let backup = target
        .is_dir()
        .then(|| backup::create_steam_saves(paths, &target, "before-profile-export"))
        .transpose()?;
    replace_save_tree(&source, &target, true)?;
    Ok(TransferResult { save_slots, backup })
}

pub fn share(
    db: &Database,
    paths: &AppPaths,
    source: &Profile,
    target: &Profile,
) -> Result<ShareResult> {
    ensure!(source.id != target.id, "choose two different profiles");
    ensure_profile_idle(paths, source)?;
    ensure_profile_idle(paths, target)?;

    backup::create(paths, &source.id, "before-save-sharing")?;
    backup::create(paths, &target.id, "before-save-sharing")?;

    let save_set = match db.save_set_for_profile(&source.id)? {
        Some(save_set) => {
            ensure_link(db, paths, source)?;
            save_set
        }
        None => create_from_profile(db, paths, source)?,
    };
    backup::create_save_set(paths, &save_set.id, "before-attach")?;

    let target_save_set = db.save_set_for_profile(&target.id)?;
    if target_save_set
        .as_ref()
        .is_some_and(|current| current.id == save_set.id)
    {
        ensure_link(db, paths, target)?;
        return Ok(ShareResult {
            save_set,
            detached_target: None,
        });
    }
    if let Some(current) = &target_save_set {
        backup::create_save_set(paths, &current.id, "before-reassign")?;
    }

    let target_save = profile_save_dir(paths, &target.id);
    let detached_target = detach_existing_path(paths, target, &target_save)?;
    link(&target_save, &managed_save_dir(paths, &save_set.id))?;
    if let Err(error) = db.attach_save_set(&target.id, &save_set.id) {
        fs::remove_file(&target_save).ok();
        if let Some(detached) = &detached_target {
            fs::rename(detached, &target_save).ok();
        }
        return Err(error).context("record shared save set for target profile");
    }

    Ok(ShareResult {
        save_set,
        detached_target,
    })
}

pub fn make_private(db: &Database, paths: &AppPaths, profile: &Profile) -> Result<bool> {
    ensure_profile_idle(paths, profile)?;
    let Some(save_set) = db.save_set_for_profile(&profile.id)? else {
        return Ok(false);
    };
    backup::create_save_set(paths, &save_set.id, "before-detach")?;
    backup::create(paths, &profile.id, "before-save-detach")?;

    let profile_save = profile_save_dir(paths, &profile.id);
    let managed = managed_save_dir(paths, &save_set.id);
    ensure_managed_link(&profile_save, &managed)?;
    fs::remove_file(&profile_save)?;
    if let Err(error) = copy_tree(&managed, &profile_save) {
        fs::remove_dir_all(&profile_save).ok();
        link(&profile_save, &managed).ok();
        return Err(error).context("copy shared saves into private profile storage");
    }
    if let Err(error) = db.detach_save_set(&profile.id) {
        fs::remove_dir_all(&profile_save).ok();
        link(&profile_save, &managed).ok();
        return Err(error).context("detach save set from profile");
    }
    Ok(true)
}

pub fn ensure_link(db: &Database, paths: &AppPaths, profile: &Profile) -> Result<()> {
    let Some(save_set) = db.save_set_for_profile(&profile.id)? else {
        return Ok(());
    };
    let profile_save = profile_save_dir(paths, &profile.id);
    let managed = managed_save_dir(paths, &save_set.id);
    ensure!(
        managed.is_dir(),
        "managed save set is missing: {}",
        managed.display()
    );
    if fs::symlink_metadata(&profile_save).is_ok() {
        ensure_managed_link(&profile_save, &managed)?;
    } else {
        link(&profile_save, &managed)?;
    }
    Ok(())
}

fn create_from_profile(db: &Database, paths: &AppPaths, profile: &Profile) -> Result<SaveSet> {
    let id = Uuid::new_v4().to_string();
    let save_set = SaveSet {
        id: id.clone(),
        name: format!("{} modded saves ({})", profile.name, &id[..8]),
        created_at: Utc::now(),
    };
    let profile_save = profile_save_dir(paths, &profile.id);
    ensure!(
        fs::symlink_metadata(&profile_save)
            .map(|metadata| !metadata.file_type().is_symlink())
            .unwrap_or(true),
        "profile save path is already an unmanaged symbolic link"
    );
    let managed = managed_save_dir(paths, &id);
    fs::create_dir_all(managed.parent().context("save set has no parent")?)?;
    let had_private_saves = profile_save.exists();
    if had_private_saves {
        fs::rename(&profile_save, &managed).context("move private saves into managed save set")?;
    } else {
        fs::create_dir_all(&managed)?;
    }
    if let Err(error) = link(&profile_save, &managed) {
        if had_private_saves {
            fs::rename(&managed, &profile_save).ok();
        }
        return Err(error);
    }
    if let Err(error) = db
        .create_save_set(&save_set)
        .and_then(|()| db.attach_save_set(&profile.id, &id))
    {
        fs::remove_file(&profile_save).ok();
        if had_private_saves {
            fs::rename(&managed, &profile_save).ok();
        }
        return Err(error).context("record managed save set");
    }
    Ok(save_set)
}

fn link(profile_save: &Path, managed: &Path) -> Result<()> {
    let parent = profile_save
        .parent()
        .context("profile save path has no parent")?;
    fs::create_dir_all(parent)?;
    symlink(managed, profile_save).with_context(|| {
        format!(
            "link profile saves {} to {}",
            profile_save.display(),
            managed.display()
        )
    })?;
    Ok(())
}

fn ensure_managed_link(profile_save: &Path, managed: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(profile_save)
        .with_context(|| format!("inspect save link {}", profile_save.display()))?;
    ensure!(
        metadata.file_type().is_symlink(),
        "profile has private saves where a managed save link was expected: {}",
        profile_save.display()
    );
    let target = fs::read_link(profile_save)?;
    ensure!(
        target == managed,
        "profile save link points outside its managed save set: {}",
        target.display()
    );
    Ok(())
}

fn detach_existing_path(
    paths: &AppPaths,
    profile: &Profile,
    existing: &Path,
) -> Result<Option<PathBuf>> {
    let Ok(metadata) = fs::symlink_metadata(existing) else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() {
        fs::remove_file(existing)?;
        return Ok(None);
    }
    let destination = paths
        .profile_dir(&profile.id)
        .join("detached-saves")
        .join(Utc::now().format("%Y%m%d-%H%M%S%.3f").to_string());
    fs::create_dir_all(
        destination
            .parent()
            .context("detached save has no parent")?,
    )?;
    fs::rename(existing, &destination)?;
    Ok(Some(destination))
}

fn ensure_profile_idle(paths: &AppPaths, profile: &Profile) -> Result<()> {
    ensure!(
        !paths.profile_dir(&profile.id).join("run.lock").exists(),
        "profile {} is running; stop it before changing save sharing",
        profile.name
    );
    Ok(())
}

fn ensure_transfer_idle(paths: &AppPaths, profile: &Profile) -> Result<()> {
    ensure_profile_idle(paths, profile)?;
    ensure!(
        !doctor::steam_running(),
        "exit Steam completely before importing or exporting saves to avoid a Steam Cloud race"
    );
    ensure!(
        !cyberpunk_running(),
        "Cyberpunk is still running; exit it before transferring saves"
    );
    Ok(())
}

fn cyberpunk_running() -> bool {
    let Ok(processes) = fs::read_dir("/proc") else {
        return false;
    };
    processes.filter_map(Result::ok).any(|process| {
        process
            .file_name()
            .to_str()
            .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
            && fs::read(process.path().join("cmdline"))
                .ok()
                .is_some_and(|command| {
                    String::from_utf8_lossy(&command)
                        .to_ascii_lowercase()
                        .contains("cyberpunk2077.exe")
                })
    })
}

fn count_save_slots(root: &Path) -> Result<usize> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut count = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() && entry.file_name() == "sav.dat" {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn replace_save_tree(
    source: &Path,
    destination: &Path,
    preserve_steam_control: bool,
) -> Result<()> {
    let parent = destination
        .parent()
        .context("save destination has no parent")?;
    fs::create_dir_all(parent)?;
    let operation_id = Uuid::new_v4().to_string();
    let staging = parent.join(format!(".cp2077-mod-tui-stage-{operation_id}"));
    let replaced = parent.join(format!(".cp2077-mod-tui-replaced-{operation_id}"));
    if let Err(error) = copy_save_tree(source, &staging) {
        fs::remove_dir_all(&staging).ok();
        return Err(error).context("stage save transfer");
    }
    ensure!(
        count_save_slots(&staging)? > 0,
        "staged transfer contains no Cyberpunk save slots"
    );

    let had_destination = fs::symlink_metadata(destination).is_ok();
    if had_destination {
        fs::rename(destination, &replaced).context("park existing save directory")?;
    }
    if let Err(error) = fs::rename(&staging, destination) {
        if had_destination {
            fs::rename(&replaced, destination).ok();
        }
        fs::remove_dir_all(&staging).ok();
        return Err(error).context("activate transferred save directory");
    }
    if had_destination {
        if preserve_steam_control {
            let control = replaced.join("steam_autocloud.vdf");
            if control.is_file() {
                fs::rename(control, destination.join("steam_autocloud.vdf"))?;
            }
        }
        if fs::symlink_metadata(&replaced)?.file_type().is_symlink() {
            fs::remove_file(replaced)?;
        } else {
            fs::remove_dir_all(replaced)?;
        }
    }
    Ok(())
}

fn copy_save_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() == "steam_autocloud.vdf" {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_save_tree(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path)?;
        } else {
            bail!(
                "unsupported item in save transfer: {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path)?;
        } else {
            bail!("unsupported item in save set: {}", source_path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::models::GameInstall;

    fn profile(id: &str, name: &str) -> Profile {
        Profile {
            id: id.into(),
            name: name.into(),
            game_install_id: "game".into(),
            game_build_id: "1".into(),
            runner: "UMU-Proton".into(),
            launch_args: Vec::new(),
            environment: BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn shares_saves_without_touching_target_private_copy() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_dir: temp.path().join("config"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            runtime_dir: temp.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let db = Database::in_memory().unwrap();
        db.upsert_game_install(&GameInstall {
            id: "game".into(),
            library: "/steam".into(),
            root: "/steam/game".into(),
            manifest: "/steam/app.acf".into(),
            build_id: "1".into(),
            phantom_liberty: false,
            redmod: false,
            writable: true,
        })
        .unwrap();
        let source = profile("source", "Source");
        let target = profile("target", "Target");
        db.create_profile(&source).unwrap();
        db.create_profile(&target).unwrap();
        let source_save = profile_save_dir(&paths, &source.id);
        let target_save = profile_save_dir(&paths, &target.id);
        fs::create_dir_all(source_save.join("ManualSave-0")).unwrap();
        fs::write(source_save.join("ManualSave-0/sav.dat"), b"source").unwrap();
        fs::create_dir_all(&target_save).unwrap();
        fs::write(target_save.join("user.gls"), b"target").unwrap();

        let result = share(&db, &paths, &source, &target).unwrap();

        assert_eq!(
            fs::read(target_save.join("ManualSave-0/sav.dat")).unwrap(),
            b"source"
        );
        assert!(
            fs::symlink_metadata(&source_save)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::symlink_metadata(&target_save)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(result.detached_target.unwrap().join("user.gls")).unwrap(),
            b"target"
        );
        assert_eq!(
            db.save_set_for_profile(&target.id).unwrap().unwrap().id,
            result.save_set.id
        );
    }

    #[test]
    fn staged_export_preserves_steam_control_file() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("modded");
        let destination = temp.path().join("steam");
        fs::create_dir_all(source.join("ManualSave-0")).unwrap();
        fs::write(source.join("ManualSave-0/sav.dat"), b"modded").unwrap();
        fs::write(source.join("steam_autocloud.vdf"), b"must-not-copy").unwrap();
        fs::create_dir_all(destination.join("QuickSave-0")).unwrap();
        fs::write(destination.join("QuickSave-0/sav.dat"), b"vanilla").unwrap();
        fs::write(destination.join("steam_autocloud.vdf"), b"steam-owned").unwrap();

        replace_save_tree(&source, &destination, true).unwrap();

        assert_eq!(
            fs::read(destination.join("ManualSave-0/sav.dat")).unwrap(),
            b"modded"
        );
        assert!(!destination.join("QuickSave-0").exists());
        assert_eq!(
            fs::read(destination.join("steam_autocloud.vdf")).unwrap(),
            b"steam-owned"
        );
    }
}
