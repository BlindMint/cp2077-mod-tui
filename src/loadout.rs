use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::{
    db::Database,
    deps,
    models::{FileOwner, ModRelease, Profile, ResolvedEntry},
    paths::AppPaths,
};

#[derive(Debug, Clone)]
pub struct Conflict {
    pub path: PathBuf,
    pub winner: String,
    pub shadowed: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MaterializedLoadout {
    pub revision: String,
    pub layer: PathBuf,
    pub entries: Vec<ResolvedEntry>,
    pub conflicts: Vec<Conflict>,
    pub redmods: Vec<String>,
    pub owners: BTreeMap<PathBuf, String>,
}

pub fn materialize(
    db: &Database,
    paths: &AppPaths,
    profile: &Profile,
) -> Result<MaterializedLoadout> {
    let entries = deps::resolve(&db.loadout(&profile.id)?, &db.list_dependencies()?);
    let enabled = entries
        .iter()
        .filter(|entry| entry.effective_enabled)
        .collect::<Vec<_>>();
    let mods = db
        .list_mods()?
        .into_iter()
        .map(|release| (release.id.clone(), release))
        .collect::<BTreeMap<_, _>>();
    let mut winners: BTreeMap<PathBuf, (i64, String, PathBuf)> = BTreeMap::new();
    let mut owners: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();

    for resolved in &enabled {
        let release = mods
            .get(&resolved.entry.mod_id)
            .with_context(|| format!("missing release {}", resolved.entry.mod_id))?;
        for file in db.files_for_mod(&release.id)? {
            let source = release.layer_path.join(&file.relative_path);
            owners
                .entry(file.relative_path.clone())
                .or_default()
                .push(release.id.clone());
            let candidate = (resolved.entry.priority, release.id.clone(), source);
            let replace = winners
                .get(&file.relative_path)
                .map(|current| candidate.0 > current.0)
                .unwrap_or(true);
            if replace {
                winners.insert(file.relative_path, candidate);
            }
        }
    }

    let mut digest = Sha256::new();
    for resolved in &enabled {
        digest.update(resolved.entry.mod_id.as_bytes());
        digest.update(resolved.entry.priority.to_le_bytes());
        if let Some(release) = mods.get(&resolved.entry.mod_id) {
            digest.update(release.archive_sha256.as_bytes());
        }
    }
    let revision = hex::encode(&digest.finalize()[..12]);
    let revision_root = paths
        .profile_dir(&profile.id)
        .join("loadouts")
        .join(&revision);
    let layer = revision_root.join("layer");
    if !layer.exists() {
        fs::create_dir_all(&layer)?;
        for (relative, (_, _, source)) in &winners {
            let destination = layer.join(relative);
            fs::create_dir_all(destination.parent().unwrap())?;
            link_or_copy(source, &destination)?;
        }
        fs::write(
            revision_root.join("manifest.json"),
            serde_json::to_vec_pretty(&entries)?,
        )?;
    }

    let conflicts = owners
        .into_iter()
        .filter_map(|(path, mut ids)| {
            if ids.len() < 2 {
                return None;
            }
            let winner = winners.get(&path).unwrap().1.clone();
            ids.retain(|id| id != &winner);
            Some(Conflict {
                path,
                winner,
                shadowed: ids,
            })
        })
        .collect();
    let mut redmods = Vec::new();
    for resolved in &enabled {
        let release = mods.get(&resolved.entry.mod_id).unwrap();
        for file in db.files_for_mod(&release.id)? {
            let components = file
                .relative_path
                .components()
                .map(|value| value.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if components.len() == 3
                && components[0].eq_ignore_ascii_case("mods")
                && components[2].eq_ignore_ascii_case("info.json")
                && !redmods.contains(&components[1])
            {
                redmods.push(components[1].clone());
            }
        }
    }
    let owners_map = winners
        .iter()
        .map(|(path, (_, owner, _))| (path.clone(), owner.clone()))
        .collect();
    Ok(MaterializedLoadout {
        revision,
        layer,
        entries,
        conflicts,
        redmods,
        owners: owners_map,
    })
}

fn link_or_copy(source: &Path, destination: &Path) -> Result<()> {
    match fs::hard_link(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(source, destination)?;
            let mut permissions = fs::metadata(destination)?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(destination, permissions)?;
            Ok(())
        }
    }
}

pub fn conflicts_for_profile(
    db: &Database,
    profile_id: &str,
) -> Result<Vec<(PathBuf, Vec<String>)>> {
    let entries = deps::resolve(&db.loadout(profile_id)?, &db.list_dependencies()?);
    let mut paths: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for entry in entries.into_iter().filter(|entry| entry.effective_enabled) {
        for file in db.files_for_mod(&entry.entry.mod_id)? {
            paths
                .entry(file.relative_path)
                .or_default()
                .push(entry.entry.mod_id.clone());
        }
    }
    Ok(paths
        .into_iter()
        .filter(|(_, owners)| owners.len() > 1)
        .collect())
}

pub fn verify_index(db: &Database, release: &ModRelease) -> Result<()> {
    for FileOwner { relative_path, .. } in db.files_for_mod(&release.id)? {
        ensure!(
            release.layer_path.join(&relative_path).is_file(),
            "missing indexed file {}",
            relative_path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardlink_falls_back_or_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("a");
        let destination = temp.path().join("b");
        fs::write(&source, b"night city").unwrap();
        link_or_copy(&source, &destination).unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"night city");
    }
}
