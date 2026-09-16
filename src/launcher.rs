use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};

use crate::{
    backup,
    db::Database,
    doctor,
    loadout::{self, MaterializedLoadout},
    models::{GameInstall, Profile},
    paths::AppPaths,
    saves,
};

#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub executable: String,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub mountpoint: PathBuf,
    pub redmod_command: Option<Vec<String>>,
    pub conflicts: usize,
}

pub fn plan(
    db: &Database,
    paths: &AppPaths,
    profile: &Profile,
    game: &GameInstall,
    force: bool,
) -> Result<(LaunchPlan, MaterializedLoadout)> {
    if profile.game_build_id != game.build_id
        && !force
        && !db.has_compatibility_override(&profile.id, &game.build_id)?
    {
        bail!(
            "game build changed from {} to {}; review framework compatibility or use --force",
            profile.game_build_id,
            game.build_id
        );
    }
    let loadout = loadout::materialize(db, paths, profile)?;
    let broken = loadout
        .entries
        .iter()
        .filter(|entry| entry.entry.requested_enabled && !entry.effective_enabled)
        .collect::<Vec<_>>();
    ensure!(
        broken.is_empty(),
        "loadout has disabled dependents: {}",
        broken
            .iter()
            .map(|entry| format!(
                "{} ({})",
                entry.entry.mod_id,
                entry.disabled_reason.as_deref().unwrap_or("unknown")
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !loadout.redmods.is_empty() {
        ensure!(
            game.redmod,
            "profile contains REDmods but Steam DLC 2060310 is not installed"
        );
    }

    let profile_dir = paths.profile_dir(&profile.id);
    let mountpoint = paths.runtime_dir.join(&profile.id).join("game");
    let prefix = profile_dir.join("prefix");
    let mut environment = profile.environment.clone();
    environment.insert("WINEPREFIX".into(), prefix.display().to_string());
    environment.insert("GAMEID".into(), "umu-1091500".into());
    environment.insert("STORE".into(), "steam".into());
    environment.insert("SteamAppId".into(), "1091500".into());
    environment.insert("SteamGameId".into(), "1091500".into());
    environment.insert("STEAM_COMPAT_APP_ID".into(), "1091500".into());
    environment.insert("WINEDLLOVERRIDES".into(), "winmm,version=n,b".into());
    if !profile.runner.is_empty() && profile.runner != "UMU-Proton" {
        environment.insert("PROTONPATH".into(), profile.runner.clone());
    }

    let mut args = vec![
        mountpoint
            .join("bin/x64/Cyberpunk2077.exe")
            .display()
            .to_string(),
    ];
    if !loadout.redmods.is_empty() {
        args.push("-modded".into());
    }
    args.extend(profile.launch_args.clone());
    let redmod_command = (!loadout.redmods.is_empty()).then(|| {
        vec![
            mountpoint
                .join("tools/redmod/bin/redMod.exe")
                .display()
                .to_string(),
            "deploy".into(),
            format!("-root={}", wine_path(&mountpoint)),
            format!("-mod={}", loadout.redmods.join(",")),
        ]
    });
    let conflicts = loadout.conflicts.len();
    Ok((
        LaunchPlan {
            executable: "umu-run".into(),
            args,
            environment,
            mountpoint,
            redmod_command,
            conflicts,
        },
        loadout,
    ))
}

pub fn prepare_prefix(paths: &AppPaths, profile: &Profile, redmod: bool) -> Result<()> {
    let profile_dir = paths.profile_dir(&profile.id);
    let prefix = profile_dir.join("prefix");
    fs::create_dir_all(&profile_dir)?;
    let mut env = BTreeMap::from([
        ("WINEPREFIX".to_string(), prefix.display().to_string()),
        ("GAMEID".to_string(), "umu-1091500".to_string()),
        ("STORE".to_string(), "steam".to_string()),
    ]);
    if !profile.runner.is_empty() && profile.runner != "UMU-Proton" {
        env.insert("PROTONPATH".into(), profile.runner.clone());
    }
    let verbs = prepare_winetricks_args(redmod);
    run_with_env("umu-run", &verbs, &env)?;
    fs::write(
        profile_dir.join("prepared.marker"),
        chrono::Utc::now().to_rfc3339(),
    )?;
    Ok(())
}

fn prepare_winetricks_args(redmod: bool) -> Vec<&'static str> {
    let mut verbs = vec!["winetricks", "-q", "vcrun2022", "d3dcompiler_47"];
    if redmod {
        verbs.push("dotnet6");
    }
    verbs
}

pub fn profile_is_prepared(paths: &AppPaths, profile: &Profile) -> bool {
    paths
        .profile_dir(&profile.id)
        .join("prepared.marker")
        .is_file()
}

pub fn launch(
    db: &Database,
    paths: &AppPaths,
    profile: &Profile,
    game: &GameInstall,
    force: bool,
) -> Result<ExitStatus> {
    let (launch, loadout) = plan(db, paths, profile, game, force)?;
    if force && profile.game_build_id != game.build_id {
        db.record_compatibility_override(&profile.id, &game.build_id)?;
    }
    require_command("umu-run")?;
    require_command("fuse-overlayfs")?;
    require_command("fusermount3")?;
    ensure!(
        profile_is_prepared(paths, profile),
        "profile prefix is not prepared; run `cp2077-mod-tui prepare {}` first",
        profile.name
    );
    let _lock = ProfileLock::acquire(paths, &profile.id)?;
    saves::ensure_link(db, paths, profile)?;
    let _backup = backup::create(paths, &profile.id, "pre-launch").ok();
    if let Some(save_set) = db.save_set_for_profile(&profile.id)? {
        backup::create_save_set(paths, &save_set.id, "pre-launch")?;
    }
    if !doctor::steam_running() {
        let _ = Command::new("steam")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    let mut mount = OverlayMount::mount(paths, profile, game, &loadout)?;
    if let Some(command) = &launch.redmod_command {
        run_with_env(
            &launch.executable,
            &command.iter().map(String::as_str).collect::<Vec<_>>(),
            &launch.environment,
        )?;
    }
    let run_id = db.start_run(
        &profile.id,
        &format!("{} {:?}", launch.executable, launch.args),
    )?;
    let mut child = match command_with_env(
        &launch.executable,
        &launch.args.iter().map(String::as_str).collect::<Vec<_>>(),
        &launch.environment,
    )
    .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            db.finish_run(&run_id, None)?;
            return Err(error).context("launch Cyberpunk through UMU");
        }
    };
    let status = child.wait()?;
    db.finish_run(&run_id, status.code())?;
    mount.unmount()?;
    Ok(status)
}

fn command_with_env(
    command: &str,
    args: &[&str],
    environment: &BTreeMap<String, String>,
) -> Command {
    let mut process = Command::new(command);
    process.args(args);
    for (key, value) in environment {
        process.env(key, value);
    }
    process
}

fn run_with_env(
    command: &str,
    args: &[&str],
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    require_command(command)?;
    let output = command_with_env(command, args, environment).output()?;
    ensure!(
        output.status.success(),
        "{} failed: {}",
        command,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn require_command(command: &str) -> Result<()> {
    let found = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(command))
        .any(|candidate| candidate.is_file());
    ensure!(found, "required command is missing: {command}");
    Ok(())
}

struct OverlayMount {
    child: Child,
    mountpoint: PathBuf,
}

impl OverlayMount {
    fn mount(
        paths: &AppPaths,
        profile: &Profile,
        game: &GameInstall,
        loadout: &MaterializedLoadout,
    ) -> Result<Self> {
        let profile_dir = paths.profile_dir(&profile.id);
        let upper = profile_dir.join("runtime-upper");
        let work = profile_dir.join("runtime-work");
        let mountpoint = paths.runtime_dir.join(&profile.id).join("game");
        fs::create_dir_all(&upper)?;
        fs::create_dir_all(&work)?;
        fs::create_dir_all(&mountpoint)?;
        reconcile_runtime_state(&profile_dir, &upper, &loadout.owners)?;
        let options = format!(
            "lowerdir={}:{},upperdir={},workdir={}",
            overlay_escape(&loadout.layer),
            overlay_escape(&game.root),
            overlay_escape(&upper),
            overlay_escape(&work)
        );
        let child = Command::new("fuse-overlayfs")
            .args(["-f", "-o", &options])
            .arg(&mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start fuse-overlayfs")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if mountpoint.join("bin/x64/Cyberpunk2077.exe").is_file() {
                return Ok(Self { child, mountpoint });
            }
            thread::sleep(Duration::from_millis(50));
        }
        bail!("overlay mount did not become ready within five seconds")
    }

    fn unmount(&mut self) -> Result<()> {
        let status = Command::new("fusermount3")
            .args(["-u"])
            .arg(&self.mountpoint)
            .status()?;
        if !status.success() {
            let _ = Command::new("fusermount3")
                .args(["-uz"])
                .arg(&self.mountpoint)
                .status();
        }
        let _ = self.child.wait();
        Ok(())
    }
}

fn reconcile_runtime_state(
    profile_dir: &Path,
    upper: &Path,
    current: &BTreeMap<PathBuf, String>,
) -> Result<()> {
    let manifest = profile_dir.join("runtime-ownership.json");
    let previous: BTreeMap<PathBuf, String> = fs::read(&manifest)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    let parked = profile_dir.join("parked-state");
    for (path, old_owner) in &previous {
        if current.get(path) != Some(old_owner) {
            let source = upper.join(path);
            if source.exists() {
                let destination = parked.join(old_owner).join(path);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(source, destination)?;
            }
        }
    }
    for (path, owner) in current {
        let source = parked.join(owner).join(path);
        let destination = upper.join(path);
        if source.exists() && !destination.exists() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(source, destination)?;
        }
    }
    fs::write(manifest, serde_json::to_vec_pretty(current)?)?;
    Ok(())
}

impl Drop for OverlayMount {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("fusermount3")
                .args(["-uz"])
                .arg(&self.mountpoint)
                .status();
            let _ = self.child.kill();
        }
    }
}

struct ProfileLock {
    path: PathBuf,
}

impl ProfileLock {
    fn acquire(paths: &AppPaths, profile_id: &str) -> Result<Self> {
        let path = paths.profile_dir(profile_id).join("run.lock");
        if path.exists() {
            let pid = fs::read_to_string(&path)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            if pid
                .map(|pid| Path::new("/proc").join(pid.to_string()).exists())
                .unwrap_or(false)
            {
                bail!("profile is already running");
            }
            fs::remove_file(&path)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self { path })
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn overlay_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace(',', "\\,")
}

fn wine_path(path: &Path) -> String {
    format!(
        "Z:\\{}",
        path.to_string_lossy()
            .trim_start_matches('/')
            .replace('/', "\\")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_linux_path_for_redmod() {
        assert_eq!(
            wine_path(Path::new("/tmp/Night City/game")),
            "Z:\\tmp\\Night City\\game"
        );
    }

    #[test]
    fn escapes_overlay_separators() {
        assert_eq!(overlay_escape(Path::new("/tmp/a:b,c")), "/tmp/a\\:b\\,c");
    }

    #[test]
    fn prepares_prefix_directly_with_winetricks() {
        assert_eq!(
            prepare_winetricks_args(false),
            ["winetricks", "-q", "vcrun2022", "d3dcompiler_47"]
        );
    }

    #[test]
    fn adds_dotnet_for_redmod_prefixes() {
        assert_eq!(
            prepare_winetricks_args(true),
            ["winetricks", "-q", "vcrun2022", "d3dcompiler_47", "dotnet6"]
        );
    }
}
