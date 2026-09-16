use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{BufReader, Read},
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use uuid::Uuid;

use crate::{
    catalog::{FRAMEWORKS, infer_requirements},
    db::Database,
    models::{Dependency, FileOwner, ModKind, ModRelease},
    paths::AppPaths,
};

const GAME_ROOTS: &[&str] = &["archive", "bin", "engine", "r6", "red4ext", "mods", "tools"];

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub name: Option<String>,
    pub version: String,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            name: None,
            version: "unknown".into(),
        }
    }
}

pub fn import(
    db: &mut Database,
    paths: &AppPaths,
    source: &Path,
    options: ImportOptions,
) -> Result<ModRelease> {
    ensure!(
        source.exists(),
        "import source does not exist: {}",
        source.display()
    );
    paths.ensure()?;
    let stage = TempDir::new_in(&paths.cache_dir).context("create import staging directory")?;
    let raw = stage.path().join("raw");
    let layer = stage.path().join("layer");
    fs::create_dir_all(&raw)?;
    fs::create_dir_all(&layer)?;

    let archive_sha = if source.is_dir() {
        let snapshot = stage.path().join("source.tar.zst");
        create_directory_snapshot(source, &snapshot)?;
        let digest = hash_file(&snapshot)?;
        retain_archive(paths, &snapshot, &digest, "tar.zst")?;
        copy_tree(source, &raw)?;
        digest
    } else {
        validate_archive_listing(source)?;
        let digest = hash_file(source)?;
        retain_archive(
            paths,
            source,
            &digest,
            source
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or("archive"),
        )?;
        extract_archive(source, &raw)?;
        digest
    };
    validate_extracted_tree(&raw)?;
    normalize_into_layer(&raw, &layer)?;

    let relative_paths = collect_files(&layer)?
        .into_iter()
        .map(|path| {
            path.strip_prefix(&layer)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    ensure!(
        !relative_paths.is_empty(),
        "archive contains no installable files"
    );

    let detected_framework = detect_framework(&relative_paths);
    let name = options
        .name
        .or_else(|| detected_framework.map(|framework| framework.name.to_string()))
        .or_else(|| {
            source
                .file_stem()
                .map(|value| value.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Imported mod".into());
    let mod_id = detected_framework
        .map(|framework| framework.id.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if db.mod_release(&mod_id)?.is_some() {
        bail!(
            "mod id {mod_id} is already installed; remove or update it instead of importing twice"
        );
    }
    let release_dir = paths
        .releases_dir()
        .join(&mod_id)
        .join(safe_component(&options.version));
    ensure!(
        !release_dir.exists(),
        "release destination already exists: {}",
        release_dir.display()
    );
    fs::create_dir_all(release_dir.parent().unwrap())?;
    move_or_copy_tree(&layer, &release_dir)?;

    let kind = detect_kind(&relative_paths, detected_framework.is_some());
    let release = ModRelease {
        id: mod_id.clone(),
        name,
        version: options.version,
        kind,
        archive_sha256: archive_sha,
        layer_path: release_dir.clone(),
        source: source.display().to_string(),
        installed_at: Utc::now(),
    };
    let files = index_files(&mod_id, &release_dir)?;
    db.insert_mod(&release, &files)?;

    if let Some(framework) = detected_framework {
        for requirement in framework.requires {
            db.add_dependency(&Dependency {
                mod_id: mod_id.clone(),
                requires_id: (*requirement).into(),
                inferred: false,
            })?;
        }
    }
    for requirement in infer_requirements(&relative_paths) {
        if requirement != mod_id {
            db.add_dependency(&Dependency {
                mod_id: mod_id.clone(),
                requires_id: requirement,
                inferred: true,
            })?;
        }
    }
    Ok(release)
}

fn validate_archive_listing(source: &Path) -> Result<()> {
    let output = Command::new("bsdtar")
        .args(["-tf"])
        .arg(source)
        .output()
        .with_context(|| "run bsdtar; install the Arch libarchive package")?;
    ensure!(
        output.status.success(),
        "cannot list archive: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let listing = String::from_utf8(output.stdout).context("archive has non-UTF-8 entry names")?;
    let mut count = 0usize;
    for entry in listing.lines() {
        count += 1;
        ensure!(count <= 200_000, "archive has more than 200,000 entries");
        validate_relative_path(Path::new(entry))?;
    }
    ensure!(count > 0, "archive is empty");
    Ok(())
}

fn extract_archive(source: &Path, destination: &Path) -> Result<()> {
    let output = Command::new("bsdtar")
        .args(["-xf"])
        .arg(source)
        .args([
            "-C",
            destination.to_str().context("non-UTF-8 extraction path")?,
            "--no-same-owner",
            "--no-same-permissions",
            "--safe-writes",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "archive extraction failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<()> {
    ensure!(
        !path.is_absolute(),
        "archive contains absolute path {}",
        path.display()
    );
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("archive contains unsafe path {}", path.display()),
        }
    }
    Ok(())
}

fn validate_extracted_tree(root: &Path) -> Result<()> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    visit(root, &mut |path, metadata| {
        let relative = path.strip_prefix(root).unwrap();
        validate_relative_path(relative)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "archive contains a symbolic link: {}",
            relative.display()
        );
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "archive contains a special file: {}",
            relative.display()
        );
        if metadata.is_file() {
            files += 1;
            bytes = bytes.saturating_add(metadata.len());
            ensure!(files <= 200_000, "extracted archive has too many files");
            ensure!(
                bytes <= 100 * 1024 * 1024 * 1024,
                "extracted archive exceeds the 100 GiB safety limit"
            );
        }
        Ok(())
    })
}

fn normalize_into_layer(raw: &Path, layer: &Path) -> Result<()> {
    if raw.join("info.json").is_file() {
        let name = redmod_name(raw).unwrap_or_else(|| "ImportedREDmod".into());
        return copy_tree(raw, &layer.join("mods").join(safe_component(&name)));
    }
    let entries = fs::read_dir(raw)?.collect::<std::io::Result<Vec<_>>>()?;
    let redmod_dirs = entries
        .iter()
        .filter(|entry| entry.path().is_dir() && entry.path().join("info.json").is_file())
        .collect::<Vec<_>>();
    if !redmod_dirs.is_empty() && redmod_dirs.len() == entries.len() {
        for entry in redmod_dirs {
            copy_tree(&entry.path(), &layer.join("mods").join(entry.file_name()))?;
        }
        return Ok(());
    }

    let effective = find_effective_root(raw)?;
    copy_tree(&effective, layer)
}

fn find_effective_root(raw: &Path) -> Result<PathBuf> {
    let mut current = raw.to_path_buf();
    for _ in 0..6 {
        let entries = fs::read_dir(&current)?.collect::<std::io::Result<Vec<_>>>()?;
        if entries.iter().any(|entry| {
            GAME_ROOTS
                .iter()
                .any(|root| entry.file_name().eq_ignore_ascii_case(root))
        }) {
            return Ok(current);
        }
        let directories = entries
            .iter()
            .filter(|entry| entry.path().is_dir())
            .collect::<Vec<_>>();
        let files = entries.iter().any(|entry| entry.path().is_file());
        if !files && directories.len() == 1 {
            current = directories[0].path();
        } else {
            bail!(
                "ambiguous archive layout at {}; expected a game-root folder such as archive, bin, r6, red4ext, or mods",
                current.display()
            );
        }
    }
    bail!("archive has too many wrapper directories")
}

fn redmod_name(root: &Path) -> Option<String> {
    let data = fs::read_to_string(root.join("info.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&data).ok()?;
    json.get("name")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn detect_framework(paths: &[String]) -> Option<&'static crate::catalog::FrameworkDescriptor> {
    FRAMEWORKS.iter().find(|framework| {
        framework.signatures.iter().all(|signature| {
            let signature = signature.to_ascii_lowercase();
            paths.iter().any(|path| {
                let path = path.to_ascii_lowercase();
                path == signature || path.starts_with(&(signature.clone() + "/"))
            })
        })
    })
}

fn detect_kind(paths: &[String], framework: bool) -> ModKind {
    if framework {
        return ModKind::Framework;
    }
    let redmod = paths.iter().any(|path| path.starts_with("mods/"));
    let legacy = paths.iter().any(|path| {
        ["archive/", "bin/", "engine/", "r6/", "red4ext/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
    });
    match (redmod, legacy) {
        (true, true) => ModKind::Mixed,
        (true, false) => ModKind::Redmod,
        _ => ModKind::Legacy,
    }
}

fn index_files(mod_id: &str, layer: &Path) -> Result<Vec<FileOwner>> {
    collect_files(layer)?
        .into_iter()
        .map(|path| {
            Ok(FileOwner {
                mod_id: mod_id.into(),
                relative_path: path.strip_prefix(layer)?.to_path_buf(),
                sha256: hash_file(&path)?,
                size: fs::metadata(path)?.len(),
            })
        })
        .collect()
}

fn retain_archive(paths: &AppPaths, source: &Path, digest: &str, extension: &str) -> Result<()> {
    let destination = paths.archives_dir().join(format!("{digest}.{extension}"));
    if !destination.exists() {
        fs::copy(source, &destination).with_context(|| {
            format!(
                "retain source archive {} as {}",
                source.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn create_directory_snapshot(source: &Path, destination: &Path) -> Result<()> {
    let parent = source.parent().context("directory has no parent")?;
    let name = source.file_name().context("directory has no name")?;
    let output = Command::new("bsdtar")
        .args(["-caf"])
        .arg(destination)
        .args(["-C"])
        .arg(parent)
        .arg(name)
        .output()?;
    ensure!(
        output.status.success(),
        "failed to archive directory: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn move_or_copy_tree(source: &Path, destination: &Path) -> Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => copy_tree(source, destination),
    }
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "refusing to copy symlink {}",
            from.display()
        );
        if metadata.is_dir() {
            copy_tree(&from, &to)?;
        } else if metadata.is_file() {
            fs::copy(&from, &to)?;
            let mut permissions = fs::metadata(&to)?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&to, permissions)?;
        }
    }
    Ok(())
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    visit(root, &mut |path, metadata| {
        if metadata.is_file() {
            result.push(path.to_path_buf());
        }
        Ok(())
    })?;
    result.sort();
    Ok(result)
}

fn visit(root: &Path, callback: &mut impl FnMut(&Path, &fs::Metadata) -> Result<()>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        callback(&path, &metadata)?;
        if metadata.is_dir() {
            visit(&path, callback)?;
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 128];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn safe_component(value: &str) -> String {
    let result = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if result.is_empty() {
        "unknown".into()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        assert!(validate_relative_path(Path::new("../../etc/passwd")).is_err());
        assert!(validate_relative_path(Path::new("/etc/passwd")).is_err());
        assert!(validate_relative_path(Path::new("archive/pc/mod/a.archive")).is_ok());
    }

    #[test]
    fn finds_wrapped_game_root() {
        let temp = tempfile::tempdir().unwrap();
        let wrapped = temp.path().join("foo-1.0");
        fs::create_dir_all(wrapped.join("archive/pc/mod")).unwrap();
        assert_eq!(find_effective_root(temp.path()).unwrap(), wrapped);
    }

    #[test]
    fn classifies_redmod_and_legacy() {
        assert_eq!(
            detect_kind(&["mods/foo/info.json".into()], false),
            ModKind::Redmod
        );
        assert_eq!(
            detect_kind(
                &["mods/foo/info.json".into(), "r6/scripts/foo.reds".into()],
                false
            ),
            ModKind::Mixed
        );
    }

    #[test]
    fn imports_a_directory_into_managed_storage() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("r6/scripts")).unwrap();
        fs::write(source.join("r6/scripts/example.reds"), "module Example").unwrap();
        let paths = AppPaths {
            config_dir: temp.path().join("config"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            runtime_dir: temp.path().join("runtime"),
        };
        let mut db = Database::in_memory().unwrap();

        let release = import(
            &mut db,
            &paths,
            &source,
            ImportOptions {
                name: Some("Example".into()),
                version: "1.0".into(),
            },
        )
        .unwrap();

        assert!(release.layer_path.join("r6/scripts/example.reds").is_file());
        assert_eq!(db.files_for_mod(&release.id).unwrap().len(), 1);
        assert!(
            db.list_dependencies()
                .unwrap()
                .iter()
                .any(|dependency| dependency.requires_id == "redscript")
        );
        assert_eq!(fs::read_dir(paths.archives_dir()).unwrap().count(), 1);
    }
}
