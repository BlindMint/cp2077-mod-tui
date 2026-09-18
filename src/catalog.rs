use std::{collections::BTreeMap, fs, path::PathBuf, process::Command};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{models::ModRelease, paths::AppPaths};

#[derive(Debug, Clone, Serialize)]
pub struct FrameworkDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub repository: &'static str,
    pub requires: &'static [&'static str],
    pub signatures: &'static [&'static str],
}

pub const FRAMEWORKS: &[FrameworkDescriptor] = &[
    FrameworkDescriptor {
        id: "cyber-engine-tweaks",
        name: "Cyber Engine Tweaks",
        repository: "maximegmd/CyberEngineTweaks",
        requires: &[],
        signatures: &["bin/x64/version.dll", "bin/x64/plugins/cyber_engine_tweaks"],
    },
    FrameworkDescriptor {
        id: "redscript",
        name: "redscript",
        repository: "jac3km4/redscript",
        requires: &[],
        signatures: &["engine/tools/scc.exe", "engine/config/base/scripts.ini"],
    },
    FrameworkDescriptor {
        id: "red4ext",
        name: "RED4ext",
        repository: "wopss/RED4ext",
        requires: &[],
        signatures: &["red4ext/RED4ext.dll", "bin/x64/winmm.dll"],
    },
    FrameworkDescriptor {
        id: "archivexl",
        name: "ArchiveXL",
        repository: "psiberx/cp2077-archive-xl",
        requires: &["red4ext"],
        signatures: &["red4ext/plugins/ArchiveXL"],
    },
    FrameworkDescriptor {
        id: "tweakxl",
        name: "TweakXL",
        repository: "psiberx/cp2077-tweak-xl",
        requires: &["red4ext"],
        signatures: &["red4ext/plugins/TweakXL"],
    },
    FrameworkDescriptor {
        id: "codeware",
        name: "Codeware",
        repository: "psiberx/cp2077-codeware",
        requires: &["red4ext"],
        signatures: &["red4ext/plugins/Codeware"],
    },
];

pub fn matching_releases<'a>(
    mods: &'a [ModRelease],
    framework: &FrameworkDescriptor,
) -> Vec<&'a ModRelease> {
    mods.iter()
        .filter(|release| {
            release.id == framework.id || release.name.eq_ignore_ascii_case(framework.name)
        })
        .collect()
}

pub fn is_installed(mods: &[ModRelease], framework: &FrameworkDescriptor) -> bool {
    !matching_releases(mods, framework).is_empty()
}

pub fn dependencies() -> BTreeMap<String, Vec<String>> {
    FRAMEWORKS
        .iter()
        .map(|framework| {
            (
                framework.id.to_string(),
                framework
                    .requires
                    .iter()
                    .map(|value| value.to_string())
                    .collect(),
            )
        })
        .collect()
}

pub fn infer_requirements(paths: &[String]) -> Vec<String> {
    let mut required = Vec::new();
    let has = |suffix: &str| {
        paths
            .iter()
            .any(|path| path.to_ascii_lowercase().ends_with(suffix))
    };
    let under = |prefix: &str| {
        paths
            .iter()
            .any(|path| path.to_ascii_lowercase().starts_with(prefix))
    };
    if has(".lua") && under("bin/x64/plugins/cyber_engine_tweaks/") {
        required.push("cyber-engine-tweaks".into());
    }
    if has(".reds") && under("r6/scripts/") {
        required.push("redscript".into());
    }
    if has(".xl") || has(".archive.xl") {
        required.push("archivexl".into());
    }
    if has(".tweak") || (has(".yaml") && under("r6/tweaks/")) {
        required.push("tweakxl".into());
    }
    if has(".dll") && under("red4ext/plugins/") {
        required.push("red4ext".into());
    }
    required.sort();
    required.dedup();
    required
}

#[derive(Debug, Clone)]
pub struct DownloadedFramework {
    pub path: PathBuf,
    pub version: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

pub fn fetch(paths: &AppPaths, framework_id: &str) -> Result<DownloadedFramework> {
    let descriptor = FRAMEWORKS
        .iter()
        .find(|item| item.id == framework_id)
        .with_context(|| format!("unknown framework {framework_id}"))?;
    let download_dir = paths.cache_dir.join("downloads");
    fs::create_dir_all(&download_dir)?;
    let metadata = download_dir.join(format!("{framework_id}-latest.json"));
    curl_to(
        &format!(
            "https://api.github.com/repos/{}/releases/latest",
            descriptor.repository
        ),
        &metadata,
    )?;
    let release: GithubRelease =
        serde_json::from_slice(&fs::read(&metadata)?).context("parse GitHub release metadata")?;
    let mut candidates = release
        .assets
        .into_iter()
        .filter(|asset| asset.name.to_ascii_lowercase().ends_with(".zip"))
        .filter(|asset| {
            let name = asset.name.to_ascii_lowercase();
            !["source", "debug", "symbols", "pdb", "dev"]
                .iter()
                .any(|word| name.contains(word))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|asset| {
        let name = asset.name.to_ascii_lowercase();
        let preferred = match framework_id {
            "redscript" => name.contains("redscript-mod"),
            "cyber-engine-tweaks" => name.contains("cet") || name.contains("cyber"),
            _ => true,
        };
        !preferred
    });
    let asset = candidates.into_iter().next().with_context(|| {
        format!(
            "{} latest release has no installable zip asset",
            descriptor.name
        )
    })?;
    let destination = download_dir.join(&asset.name);
    curl_to(&asset.browser_download_url, &destination)?;
    if let Some(expected) = asset
        .digest
        .and_then(|value| value.strip_prefix("sha256:").map(str::to_string))
    {
        let actual = sha256_file(&destination)?;
        ensure!(
            actual.eq_ignore_ascii_case(&expected),
            "GitHub digest mismatch for {}",
            asset.name
        );
    }
    Ok(DownloadedFramework {
        path: destination,
        version: release.tag_name.trim_start_matches('v').to_string(),
    })
}

fn curl_to(url: &str, destination: &std::path::Path) -> Result<()> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--retry",
            "2",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            "-H",
            "User-Agent: cp2077-mod-tui",
        ])
        .arg(url)
        .args(["-o"])
        .arg(destination)
        .output()
        .context("run curl; install the curl package")?;
    ensure!(
        output.status.success(),
        "download failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn sha256_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_high_confidence_dependencies() {
        let result = infer_requirements(&[
            "bin/x64/plugins/cyber_engine_tweaks/mods/foo/init.lua".into(),
            "r6/scripts/foo.reds".into(),
            "archive/pc/mod/foo.archive.xl".into(),
        ]);
        assert_eq!(
            result,
            vec!["archivexl", "cyber-engine-tweaks", "redscript"]
        );
    }

    #[test]
    fn matches_frameworks_by_stable_id_or_name() {
        let uuid_copy = ModRelease {
            id: "not-redscript".into(),
            name: "redscript".into(),
            version: "1.0".into(),
            kind: crate::models::ModKind::Framework,
            archive_sha256: "abc".into(),
            layer_path: PathBuf::from("/tmp/layer"),
            source: "/tmp/src".into(),
            installed_at: chrono::Utc::now(),
        };
        let framework = FRAMEWORKS
            .iter()
            .find(|item| item.id == "redscript")
            .unwrap();
        assert!(is_installed(std::slice::from_ref(&uuid_copy), framework));
        assert_eq!(matching_releases(&[uuid_copy], framework).len(), 1);
    }
}
