use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use chrono::Utc;

use crate::paths::AppPaths;

pub fn create(paths: &AppPaths, profile_id: &str, label: &str) -> Result<PathBuf> {
    let profile = paths.profile_dir(profile_id);
    ensure!(
        profile.is_dir(),
        "profile state does not exist: {}",
        profile.display()
    );
    let destination_dir = paths.backups_dir().join(profile_id);
    fs::create_dir_all(&destination_dir)?;
    let safe_label = safe_label(label);
    let destination = destination_dir.join(format!(
        "{}-{}.tar.zst",
        Utc::now().format("%Y%m%d-%H%M%S"),
        safe_label
    ));
    let marker = profile.join("backup-marker.txt");
    let mut marker_file = fs::File::create(&marker)?;
    writeln!(marker_file, "profile={profile_id}")?;
    writeln!(marker_file, "created={}", Utc::now().to_rfc3339())?;
    let mut selected = vec![format!("{profile_id}/backup-marker.txt")];
    for relative in [
        "runtime-upper",
        "prefix/drive_c/users/steamuser/Saved Games",
    ] {
        if profile.join(relative).exists() {
            selected.push(format!("{profile_id}/{relative}"));
        }
    }
    let mut command = Command::new("bsdtar");
    command
        .args(["-caf"])
        .arg(&destination)
        .args(["-C"])
        .arg(paths.profiles_dir())
        .args(&selected);
    let output = command.output().context("run bsdtar backup")?;
    ensure!(
        output.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    fs::remove_file(marker).ok();
    prune(&destination_dir, 10)?;
    Ok(destination)
}

pub fn create_save_set(paths: &AppPaths, save_set_id: &str, label: &str) -> Result<PathBuf> {
    let source = paths.save_set_dir(save_set_id);
    ensure!(
        source.is_dir(),
        "managed save set does not exist: {}",
        source.display()
    );
    let destination_dir = paths.backups_dir().join("save-sets").join(save_set_id);
    fs::create_dir_all(&destination_dir)?;
    let destination = destination_dir.join(format!(
        "{}-{}.tar.zst",
        Utc::now().format("%Y%m%d-%H%M%S%.3f"),
        safe_label(label)
    ));
    let output = Command::new("bsdtar")
        .args(["-caf"])
        .arg(&destination)
        .args(["-C"])
        .arg(paths.save_sets_dir())
        .arg(save_set_id)
        .output()
        .context("run shared-save backup")?;
    ensure!(
        output.status.success(),
        "shared-save backup failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    prune(&destination_dir, 20)?;
    Ok(destination)
}

pub fn create_steam_saves(paths: &AppPaths, source: &Path, label: &str) -> Result<PathBuf> {
    ensure!(
        source.is_dir(),
        "Steam save directory does not exist: {}",
        source.display()
    );
    let destination_dir = paths.backups_dir().join("steam-saves");
    fs::create_dir_all(&destination_dir)?;
    let destination = destination_dir.join(format!(
        "{}-{}.tar.zst",
        Utc::now().format("%Y%m%d-%H%M%S%.3f"),
        safe_label(label)
    ));
    let parent = source
        .parent()
        .context("Steam save directory has no parent")?;
    let name = source
        .file_name()
        .context("Steam save directory has no name")?;
    let output = Command::new("bsdtar")
        .args(["-caf"])
        .arg(&destination)
        .args(["-C"])
        .arg(parent)
        .arg(name)
        .output()
        .context("run Steam-save backup")?;
    ensure!(
        output.status.success(),
        "Steam-save backup failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    prune(&destination_dir, 20)?;
    Ok(destination)
}

pub fn list(paths: &AppPaths, profile_id: &str) -> Result<Vec<PathBuf>> {
    let directory = paths.backups_dir().join(profile_id);
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut result = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("zst"))
        .collect::<Vec<_>>();
    result.sort();
    result.reverse();
    Ok(result)
}

pub fn restore(paths: &AppPaths, profile_id: &str, archive: &Path) -> Result<()> {
    ensure!(archive.is_file(), "backup archive does not exist");
    let archive = fs::canonicalize(archive)?;
    let backup_root = fs::canonicalize(paths.backups_dir())?;
    ensure!(
        archive.starts_with(&backup_root),
        "refusing to restore an archive outside the managed backup directory"
    );
    validate_restore_archive(&archive, profile_id)?;
    let profile = paths.profile_dir(profile_id);
    if profile.exists() {
        create(paths, profile_id, "pre-restore")?;
    }
    let output = Command::new("bsdtar")
        .args(["-xf"])
        .arg(&archive)
        .args(["-C"])
        .arg(paths.profiles_dir())
        .output()?;
    ensure!(
        output.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn validate_restore_archive(archive: &Path, profile_id: &str) -> Result<()> {
    let output = Command::new("bsdtar").args(["-tf"]).arg(archive).output()?;
    ensure!(
        output.status.success(),
        "cannot inspect backup: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let listing = String::from_utf8(output.stdout)?;
    ensure!(!listing.is_empty(), "backup archive is empty");
    for entry in listing.lines() {
        let path = Path::new(entry);
        ensure!(!path.is_absolute(), "backup contains an absolute path");
        ensure!(
            !path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)),
            "backup contains path traversal"
        );
        ensure!(
            path.starts_with(profile_id),
            "backup contains state for a different profile"
        );
    }
    Ok(())
}

fn prune(directory: &Path, retain: usize) -> Result<()> {
    let backups = list_parent(directory)?;
    for old in backups.into_iter().skip(retain) {
        fs::remove_file(old)?;
    }
    Ok(())
}

fn list_parent(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut items = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    items.sort();
    items.reverse();
    Ok(items)
}

fn safe_label(label: &str) -> String {
    label
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}
