use std::{path::PathBuf, process::ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use cp2077_mod_tui::{
    backup, catalog,
    db::Database,
    deps, doctor,
    import::{self, ImportOptions},
    launcher, loadout,
    models::LoadoutEntry,
    paths::AppPaths,
    profiles, saves, steam, theme, tui,
};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Doctor {
        #[arg(long)]
        json: bool,
    },
    Import(ImportArgs),
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    Mod {
        #[command(subcommand)]
        command: ModCommand,
    },
    Framework {
        #[command(subcommand)]
        command: FrameworkCommand,
    },
    Conflicts {
        profile: String,
    },
    Prepare {
        profile: String,
    },
    Launch {
        profile: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
    Backup {
        profile: String,
        #[arg(default_value = "manual")]
        label: String,
    },
    Restore {
        profile: String,
        archive: PathBuf,
    },
    Theme {
        #[command(subcommand)]
        command: ThemeCommand,
    },
}

#[derive(Args)]
struct ImportArgs {
    path: PathBuf,
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value = "unknown")]
    version: String,
    #[arg(long)]
    profile: Option<String>,
}

#[derive(Subcommand)]
enum ProfileCommand {
    List,
    Create {
        name: String,
        #[arg(long, default_value = "UMU-Proton")]
        runner: String,
    },
    Show {
        profile: String,
    },
    ShareSaves {
        source: String,
        target: String,
    },
    PrivateSaves {
        profile: String,
    },
    ImportSteamSaves {
        profile: String,
    },
    ExportSteamSaves {
        profile: String,
    },
}

#[derive(Subcommand)]
enum ModCommand {
    List,
    Enable {
        profile: String,
        mod_id: String,
    },
    Disable {
        profile: String,
        mod_id: String,
    },
    DisableAll {
        profile: String,
    },
    Priority {
        profile: String,
        mod_id: String,
        priority: i64,
    },
}

#[derive(Subcommand)]
enum FrameworkCommand {
    List,
    Fetch {
        framework: String,
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum ThemeCommand {
    List,
    Set { name: String },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover()?;
    paths.ensure()?;
    let mut db = Database::open(&paths.db_path())?;
    let games = steam::discover()?;
    for game in &games {
        db.upsert_game_install(game)?;
    }
    ensure_default_profile(&db, &paths, &games)?;

    match cli.command {
        None => {
            if let Some(profile_id) = tui::run(&mut db, &paths, &games)? {
                let profile = db
                    .profile_by_name_or_id(&profile_id)?
                    .context("selected profile disappeared")?;
                let game = db
                    .game_install(&profile.game_install_id)?
                    .context("profile game disappeared")?;
                let status = launcher::launch(&db, &paths, &profile, &game, false)?;
                println!("Cyberpunk exited with {status}");
            }
        }
        Some(Command::Doctor { json }) => {
            let checks = doctor::run(&games);
            if json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                for check in checks {
                    println!(
                        "{:4} {:22} {}",
                        if check.ok { "OK" } else { "WARN" },
                        check.name,
                        check.detail
                    );
                    if let Some(remedy) = check.remedy {
                        println!("     remedy: {remedy}");
                    }
                }
            }
        }
        Some(Command::Import(args)) => {
            let release = import::import(
                &mut db,
                &paths,
                &args.path,
                ImportOptions {
                    name: args.name,
                    version: args.version,
                },
            )?;
            println!(
                "Imported {} {} as {} ({} files)",
                release.name,
                release.version,
                release.id,
                db.files_for_mod(&release.id)?.len()
            );
            if let Some(profile_value) = args.profile {
                set_enabled(&db, &profile_value, &release.id, true)?;
            }
        }
        Some(Command::Profile { command }) => match command {
            ProfileCommand::List => {
                for profile in db.list_profiles()? {
                    println!(
                        "{}\t{}\tbuild={}\trunner={}",
                        profile.id, profile.name, profile.game_build_id, profile.runner
                    );
                }
            }
            ProfileCommand::Create { name, runner } => {
                let game = games.first().context("Cyberpunk is not installed")?;
                let profile = profiles::create(&db, &paths, game, &name, &runner)?;
                println!("Created profile {} ({})", profile.name, profile.id);
            }
            ProfileCommand::Show { profile } => {
                let profile = db
                    .profile_by_name_or_id(&profile)?
                    .context("profile not found")?;
                println!("{}", serde_json::to_string_pretty(&profile)?);
                let resolved = deps::resolve(&db.loadout(&profile.id)?, &db.list_dependencies()?);
                println!("{}", serde_json::to_string_pretty(&resolved)?);
            }
            ProfileCommand::ShareSaves { source, target } => {
                let source = db
                    .profile_by_name_or_id(&source)?
                    .context("source profile not found")?;
                let target = db
                    .profile_by_name_or_id(&target)?
                    .context("target profile not found")?;
                let result = saves::share(&db, &paths, &source, &target)?;
                println!(
                    "{} and {} now share '{}'",
                    source.name, target.name, result.save_set.name
                );
                if let Some(detached) = result.detached_target {
                    println!("Previous target saves preserved at {}", detached.display());
                }
            }
            ProfileCommand::PrivateSaves { profile } => {
                let profile = db
                    .profile_by_name_or_id(&profile)?
                    .context("profile not found")?;
                if saves::make_private(&db, &paths, &profile)? {
                    println!("{} now uses a private save copy", profile.name);
                } else {
                    println!("{} already uses private saves", profile.name);
                }
            }
            ProfileCommand::ImportSteamSaves { profile } => {
                let profile = db
                    .profile_by_name_or_id(&profile)?
                    .context("profile not found")?;
                let game = db
                    .game_install(&profile.game_install_id)?
                    .context("game install not found")?;
                let result = saves::import_from_steam(&db, &paths, &profile, &game)?;
                println!(
                    "Imported {} Steam save slot(s) into {}",
                    result.save_slots, profile.name
                );
                if let Some(backup) = result.backup {
                    println!("Previous modded saves backed up to {}", backup.display());
                }
            }
            ProfileCommand::ExportSteamSaves { profile } => {
                let profile = db
                    .profile_by_name_or_id(&profile)?
                    .context("profile not found")?;
                let game = db
                    .game_install(&profile.game_install_id)?
                    .context("game install not found")?;
                let result = saves::export_to_steam(&db, &paths, &profile, &game)?;
                println!(
                    "Exported {} save slot(s) from {} to Steam",
                    result.save_slots, profile.name
                );
                if let Some(backup) = result.backup {
                    println!("Previous Steam saves backed up to {}", backup.display());
                }
            }
        },
        Some(Command::Mod { command }) => match command {
            ModCommand::List => {
                for item in db.list_mods()? {
                    println!(
                        "{}\t{}\t{}\t{}",
                        item.id,
                        item.name,
                        item.version,
                        item.kind.as_str()
                    );
                }
            }
            ModCommand::Enable { profile, mod_id } => {
                set_enabled(&db, &profile, &mod_id, true)?;
            }
            ModCommand::Disable { profile, mod_id } => {
                set_enabled(&db, &profile, &mod_id, false)?;
            }
            ModCommand::DisableAll { profile } => {
                let profile = db
                    .profile_by_name_or_id(&profile)?
                    .context("profile not found")?;
                let changed = db.disable_all_mods(&profile.id)?;
                let revision =
                    db.revision(&profile.id, "disable all mods for vanilla loadout from CLI")?;
                println!(
                    "Disabled {changed} mod(s) in {} (revision {revision})",
                    profile.name
                );
            }
            ModCommand::Priority {
                profile,
                mod_id,
                priority,
            } => {
                set_priority(&db, &profile, &mod_id, priority)?;
            }
        },
        Some(Command::Framework { command }) => match command {
            FrameworkCommand::List => {
                let installed = db
                    .list_mods()?
                    .into_iter()
                    .map(|item| item.id)
                    .collect::<Vec<_>>();
                for framework in catalog::FRAMEWORKS {
                    println!(
                        "{:9} {:22} {}",
                        if installed.contains(&framework.id.to_string()) {
                            "installed"
                        } else {
                            "available"
                        },
                        framework.name,
                        framework.repository
                    );
                }
            }
            FrameworkCommand::Fetch { framework, profile } => {
                let downloaded = catalog::fetch(&paths, &framework)?;
                let descriptor = catalog::FRAMEWORKS
                    .iter()
                    .find(|item| item.id == framework)
                    .context("unknown framework")?;
                let release = import::import(
                    &mut db,
                    &paths,
                    &downloaded.path,
                    ImportOptions {
                        name: Some(descriptor.name.into()),
                        version: downloaded.version,
                    },
                )?;
                println!("Installed {} {}", release.name, release.version);
                if let Some(profile) = profile {
                    set_enabled(&db, &profile, &release.id, true)?;
                }
            }
        },
        Some(Command::Conflicts { profile }) => {
            let profile = db
                .profile_by_name_or_id(&profile)?
                .context("profile not found")?;
            for (path, owners) in loadout::conflicts_for_profile(&db, &profile.id)? {
                println!("{}\t{}", path.display(), owners.join(" > "));
            }
        }
        Some(Command::Prepare { profile }) => {
            let profile = db
                .profile_by_name_or_id(&profile)?
                .context("profile not found")?;
            let loadout = loadout::materialize(&db, &paths, &profile)?;
            launcher::prepare_prefix(&paths, &profile, !loadout.redmods.is_empty())?;
            println!("Prepared isolated prefix for {}", profile.name);
        }
        Some(Command::Launch {
            profile,
            dry_run,
            force,
        }) => {
            let profile = db
                .profile_by_name_or_id(&profile)?
                .context("profile not found")?;
            let game = db
                .game_install(&profile.game_install_id)?
                .context("game install not found")?;
            if dry_run {
                let (plan, loadout) = launcher::plan(&db, &paths, &profile, &game, force)?;
                println!("mount: {}", plan.mountpoint.display());
                println!("command: {} {}", plan.executable, shell_display(&plan.args));
                println!("loadout revision: {}", loadout.revision);
                println!("conflicts: {}", plan.conflicts);
                if let Some(redmod) = plan.redmod_command {
                    println!("redmod: umu-run {}", shell_display(&redmod));
                }
            } else {
                let status = launcher::launch(&db, &paths, &profile, &game, force)?;
                if !status.success() {
                    bail!("Cyberpunk exited with {status}");
                }
            }
        }
        Some(Command::Backup { profile, label }) => {
            let profile = db
                .profile_by_name_or_id(&profile)?
                .context("profile not found")?;
            println!("{}", backup::create(&paths, &profile.id, &label)?.display());
            if let Some(save_set) = db.save_set_for_profile(&profile.id)? {
                println!(
                    "{}",
                    backup::create_save_set(&paths, &save_set.id, &label)?.display()
                );
            }
        }
        Some(Command::Restore { profile, archive }) => {
            let profile = db
                .profile_by_name_or_id(&profile)?
                .context("profile not found")?;
            backup::restore(&paths, &profile.id, &archive)?;
            println!("Restored {}", archive.display());
        }
        Some(Command::Theme { command }) => match command {
            ThemeCommand::List => {
                for theme in theme::presets() {
                    println!("{}", theme.name);
                }
            }
            ThemeCommand::Set { name } => {
                let selected = theme::presets()
                    .into_iter()
                    .find(|theme| theme.name.eq_ignore_ascii_case(&name))
                    .context("unknown theme")?;
                selected.save(&paths.config_path())?;
                println!("Theme set to {}", selected.name);
            }
        },
    }
    Ok(())
}

fn ensure_default_profile(
    db: &Database,
    paths: &AppPaths,
    games: &[cp2077_mod_tui::models::GameInstall],
) -> Result<()> {
    if db.list_profiles()?.is_empty()
        && let Some(game) = games.first()
    {
        profiles::create(db, paths, game, "Default", "UMU-Proton")?;
    }
    Ok(())
}

fn set_enabled(db: &Database, profile_value: &str, mod_id: &str, enabled: bool) -> Result<()> {
    let profile = db
        .profile_by_name_or_id(profile_value)?
        .context("profile not found")?;
    db.mod_release(mod_id)?.context("mod not found")?;
    let existing = db
        .loadout(&profile.id)?
        .into_iter()
        .find(|entry| entry.mod_id == mod_id);
    db.set_loadout_entry(&LoadoutEntry {
        profile_id: profile.id.clone(),
        mod_id: mod_id.into(),
        priority: existing
            .as_ref()
            .map(|entry| entry.priority)
            .unwrap_or(db.next_priority(&profile.id)?),
        requested_enabled: enabled,
    })?;
    let revision = db.revision(
        &profile.id,
        if enabled {
            "enable mod from CLI"
        } else {
            "disable mod from CLI"
        },
    )?;
    println!(
        "{} {mod_id} in {} (revision {revision})",
        if enabled { "Enabled" } else { "Disabled" },
        profile.name
    );
    Ok(())
}

fn set_priority(db: &Database, profile_value: &str, mod_id: &str, priority: i64) -> Result<()> {
    let profile = db
        .profile_by_name_or_id(profile_value)?
        .context("profile not found")?;
    db.mod_release(mod_id)?.context("mod not found")?;
    let mut entry = db
        .loadout(&profile.id)?
        .into_iter()
        .find(|entry| entry.mod_id == mod_id)
        .context("mod is not part of this profile; enable or disable it first")?;
    entry.priority = priority;
    db.set_loadout_entry(&entry)?;
    let revision = db.revision(&profile.id, "change mod priority from CLI")?;
    println!(
        "Set {mod_id} priority to {priority} in {} (revision {revision})",
        profile.name
    );
    Ok(())
}

fn shell_display(args: &[String]) -> String {
    args.iter()
        .map(|arg| format!("{arg:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}
