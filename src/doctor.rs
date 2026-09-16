use std::{
    env,
    path::Path,
    process::{Command, Stdio},
};

use serde::Serialize;

use crate::models::GameInstall;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub remedy: Option<String>,
}

pub fn run(games: &[GameInstall]) -> Vec<Check> {
    let mut checks = vec![
        command_check(
            "Steam",
            "steam",
            "Install native Steam or expose the Steam library manually.",
        ),
        command_check(
            "UMU launcher",
            "umu-run",
            "Install with: sudo pacman -S umu-launcher",
        ),
        command_check(
            "FUSE overlay",
            "fuse-overlayfs",
            "Install with: sudo pacman -S fuse-overlayfs",
        ),
        command_check(
            "Winetricks",
            "winetricks",
            "Install with: sudo pacman -S winetricks",
        ),
        command_check(
            "Archive extractor",
            "bsdtar",
            "Install with: sudo pacman -S libarchive",
        ),
    ];
    checks.push(Check {
        name: "Wayland/Hyprland session".into(),
        ok: env::var("WAYLAND_DISPLAY").is_ok() || env::var("DISPLAY").is_ok(),
        detail: format!(
            "{} / {}",
            env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into()),
            env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown".into())
        ),
        remedy: None,
    });
    checks.push(Check {
        name: "Cyberpunk 2077".into(),
        ok: !games.is_empty(),
        detail: games
            .first()
            .map(|game| {
                format!(
                    "{} (build {}, {}, {})",
                    game.root.display(),
                    game.build_id,
                    if game.phantom_liberty {
                        "Phantom Liberty"
                    } else {
                        "base game"
                    },
                    if game.writable {
                        "writable"
                    } else {
                        "read-only"
                    }
                )
            })
            .unwrap_or_else(|| "Steam app 1091500 was not found".into()),
        remedy: games
            .is_empty()
            .then(|| "Install Cyberpunk 2077 in a discoverable Steam library.".into()),
    });
    if let Some(game) = games.first() {
        checks.push(Check {
            name: "REDmod DLC".into(),
            ok: game.redmod,
            detail: if game.redmod {
                "tools/redmod/bin/redMod.exe is present".into()
            } else {
                "Free Steam DLC 2060310 is not installed".into()
            },
            remedy: (!game.redmod).then(|| {
                "Install Cyberpunk 2077 REDmod from Steam; the Steam library must be writable."
                    .into()
            }),
        });
        checks.push(Check {
            name: "Vanilla tree".into(),
            ok: known_mod_artifacts(&game.root).is_empty(),
            detail: match known_mod_artifacts(&game.root).as_slice() {
                [] => "No managed-framework artifacts found in the vanilla tree".into(),
                paths => format!("Unmanaged artifacts: {}", paths.join(", ")),
            },
            remedy: (!known_mod_artifacts(&game.root).is_empty()).then(|| {
                "Import existing mods, remove them from the vanilla tree, then verify with Steam."
                    .into()
            }),
        });
    }
    checks
}

fn command_check(name: &str, command: &str, remedy: &str) -> Check {
    let path = find_command(command);
    Check {
        name: name.into(),
        ok: path.is_some(),
        detail: path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| format!("{command} not found in PATH")),
        remedy: (!command_exists(command)).then(|| remedy.into()),
    }
}

fn command_exists(command: &str) -> bool {
    find_command(command).is_some()
}

fn find_command(command: &str) -> Option<std::path::PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

fn known_mod_artifacts(root: &Path) -> Vec<String> {
    [
        "bin/x64/version.dll",
        "bin/x64/winmm.dll",
        "bin/x64/plugins/cyber_engine_tweaks",
        "red4ext/RED4ext.dll",
        "archive/pc/mod",
        "r6/scripts",
        "r6/tweaks",
    ]
    .into_iter()
    .filter(|relative| root.join(relative).exists())
    .map(str::to_string)
    .collect()
}

pub fn steam_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "steam"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
