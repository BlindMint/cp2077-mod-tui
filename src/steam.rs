use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::{CYBERPUNK_APP_ID, models::GameInstall};

pub fn steam_roots() -> Vec<PathBuf> {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".var/app/com.valvesoftware.Steam/data/Steam"),
    ]
    .into_iter()
    .filter(|path| path.is_dir())
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect()
}

pub fn discover() -> Result<Vec<GameInstall>> {
    let mut libraries = BTreeSet::new();
    for root in steam_roots() {
        libraries.insert(root.clone());
        let library_file = root.join("steamapps/libraryfolders.vdf");
        if let Ok(contents) = fs::read_to_string(&library_file) {
            for path in vdf_values(&contents, "path") {
                libraries.insert(PathBuf::from(path.replace("\\\\", "\\")));
            }
        }
    }

    let mut found = Vec::new();
    for library in libraries {
        let manifest = library
            .join("steamapps")
            .join(format!("appmanifest_{CYBERPUNK_APP_ID}.acf"));
        if !manifest.is_file() {
            continue;
        }
        let contents = fs::read_to_string(&manifest)
            .with_context(|| format!("read Steam manifest {}", manifest.display()))?;
        let installdir =
            vdf_value(&contents, "installdir").unwrap_or_else(|| "Cyberpunk 2077".to_string());
        let root = library.join("steamapps/common").join(installdir);
        if !root.join("bin/x64/Cyberpunk2077.exe").is_file() {
            continue;
        }
        let canonical_root = fs::canonicalize(&root).unwrap_or(root);
        let id = hex::encode(&Sha256::digest(canonical_root.to_string_lossy().as_bytes())[..12]);
        let build_id = vdf_value(&contents, "buildid").unwrap_or_else(|| "unknown".into());
        let phantom_liberty =
            contents.contains("\"2138330\"") || canonical_root.join("archive/pc/ep1").is_dir();
        let redmod = canonical_root.join("tools/redmod/bin/redMod.exe").is_file();
        found.push(GameInstall {
            id,
            library,
            root: canonical_root.clone(),
            manifest,
            build_id,
            phantom_liberty,
            redmod,
            writable: is_writable(&canonical_root),
        });
    }
    found.sort_by(|a, b| a.root.cmp(&b.root));
    found.dedup_by(|a, b| a.root == b.root);
    Ok(found)
}

fn is_writable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| !metadata.permissions().readonly())
        .unwrap_or(false)
        && parent_mount_is_rw(path)
}

fn parent_mount_is_rw(path: &Path) -> bool {
    let Ok(mounts) = fs::read_to_string("/proc/self/mountinfo") else {
        return true;
    };
    let mut best: Option<(usize, bool)> = None;
    for line in mounts.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }
        let mountpoint = fields[4].replace("\\040", " ");
        let mountpoint_path = Path::new(&mountpoint);
        if path.starts_with(mountpoint_path) {
            let rw = fields[5].split(',').any(|option| option == "rw");
            let depth = mountpoint.len();
            if best.map(|(old, _)| depth > old).unwrap_or(true) {
                best = Some((depth, rw));
            }
        }
    }
    best.map(|(_, rw)| rw).unwrap_or(true)
}

pub fn vdf_value(contents: &str, key: &str) -> Option<String> {
    vdf_values(contents, key).into_iter().next()
}

pub fn vdf_values(contents: &str, key: &str) -> Vec<String> {
    contents
        .lines()
        .filter_map(|line| {
            let mut values = quoted_values(line);
            if values.len() >= 2 && values[0].eq_ignore_ascii_case(key) {
                Some(values.swap_remove(1))
            } else {
                None
            }
        })
        .collect()
}

fn quoted_values(line: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            if quoted {
                values.push(std::mem::take(&mut current));
            }
            quoted = !quoted;
        } else if quoted {
            current.push(ch);
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vdf_values_and_escapes() {
        let input = r#"
            "path" "/mnt/Games/SteamLibrary"
            "installdir" "Cyberpunk 2077"
            "path" "D:\\Steam"
        "#;
        assert_eq!(
            vdf_values(input, "path"),
            vec!["/mnt/Games/SteamLibrary", "D:\\Steam"]
        );
        assert_eq!(
            vdf_value(input, "installdir").as_deref(),
            Some("Cyberpunk 2077")
        );
    }
}
