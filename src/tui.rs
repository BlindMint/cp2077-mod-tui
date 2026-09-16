use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Tabs, Wrap},
};

use crate::{
    backup, catalog,
    db::Database,
    deps, doctor,
    import::{self, ImportOptions},
    inbox::{self, ImportCandidate},
    launcher, loadout,
    models::{GameInstall, LoadoutEntry, ModRelease, Profile, ResolvedEntry, SaveSet},
    paths::AppPaths,
    profiles, saves,
    theme::{Theme, presets},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Dashboard,
    Profiles,
    Mods,
    Imports,
    Frameworks,
    Conflicts,
    Backups,
    Health,
    Themes,
}

impl Page {
    const ALL: [Page; 9] = [
        Page::Dashboard,
        Page::Profiles,
        Page::Mods,
        Page::Imports,
        Page::Frameworks,
        Page::Conflicts,
        Page::Backups,
        Page::Health,
        Page::Themes,
    ];
    fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "1DASH",
            Self::Profiles => "2PROF",
            Self::Mods => "3MOD",
            Self::Imports => "4IMP",
            Self::Frameworks => "5CORE",
            Self::Conflicts => "6CLASH",
            Self::Backups => "7BAK",
            Self::Health => "8HLTH",
            Self::Themes => "9THEME",
        }
    }
}

#[derive(Debug, Clone)]
enum PendingAction {
    DisableAllMods,
    RestoreBackup(PathBuf),
    ShareSaves {
        source_id: String,
        target_id: String,
    },
    MakeSavesPrivate {
        profile_id: String,
    },
    ImportSteamSaves {
        profile_id: String,
    },
    ExportSteamSaves {
        profile_id: String,
    },
}

struct App {
    page: Page,
    selected: usize,
    profile_index: usize,
    status: String,
    pending_launch: bool,
    mods: Vec<ModRelease>,
    profiles: Vec<Profile>,
    resolved: Vec<ResolvedEntry>,
    import_root: PathBuf,
    import_candidates: Vec<ImportCandidate>,
    import_path_input: Option<String>,
    profile_name_input: Option<String>,
    pending_action: Option<PendingAction>,
    prepared_profiles: BTreeSet<String>,
    save_sets: BTreeMap<String, SaveSet>,
    backups: Vec<PathBuf>,
    conflicts: Vec<(PathBuf, Vec<String>)>,
    theme: Theme,
}

pub fn run(db: &mut Database, paths: &AppPaths, games: &[GameInstall]) -> Result<Option<String>> {
    let theme = Theme::load_or_default(&paths.config_path())?;
    let import_root = inbox::default_directory()?;
    let import_candidates = inbox::scan(&import_root)?;
    let profiles = db.list_profiles()?;
    let prepared_profiles = profiles
        .iter()
        .filter(|profile| launcher::profile_is_prepared(paths, profile))
        .map(|profile| profile.id.clone())
        .collect();
    let save_sets = db.profile_save_sets()?.into_iter().collect();
    let mut app = App {
        page: Page::Dashboard,
        selected: 0,
        profile_index: 0,
        status: "Ready. Press ? for key hints.".into(),
        pending_launch: false,
        mods: db.list_mods()?,
        profiles,
        resolved: Vec::new(),
        import_root,
        import_candidates,
        import_path_input: None,
        profile_name_input: None,
        pending_action: None,
        prepared_profiles,
        save_sets,
        backups: Vec::new(),
        conflicts: Vec::new(),
        theme,
    };
    reload_profile_state(&mut app, db, paths)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, &mut app, db, paths, games);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &mut Database,
    paths: &AppPaths,
    games: &[GameInstall],
) -> Result<Option<String>> {
    loop {
        terminal.draw(|frame| render(frame, app, games))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if app.import_path_input.is_some() {
            handle_import_path_key(terminal, app, db, paths, games, key.code)?;
            continue;
        }
        if app.profile_name_input.is_some() {
            handle_profile_name_key(app, db, paths, games, key.code)?;
            continue;
        }
        if app.pending_action.is_some() {
            handle_pending_action(terminal, app, db, paths, games, key.code)?;
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            KeyCode::Char('1') => switch_page(app, Page::Dashboard),
            KeyCode::Char('2') => switch_page(app, Page::Profiles),
            KeyCode::Char('3') => switch_page(app, Page::Mods),
            KeyCode::Char('4') => switch_page(app, Page::Imports),
            KeyCode::Char('5') => switch_page(app, Page::Frameworks),
            KeyCode::Char('6') => switch_page(app, Page::Conflicts),
            KeyCode::Char('7') => switch_page(app, Page::Backups),
            KeyCode::Char('8') => switch_page(app, Page::Health),
            KeyCode::Char('9') => switch_page(app, Page::Themes),
            KeyCode::Down | KeyCode::Char('j') => {
                app.selected = app.selected.saturating_add(1);
                clamp_selection(app);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.selected = app.selected.saturating_sub(1);
            }
            KeyCode::Char(' ') if app.page == Page::Mods => {
                toggle_selected(app, db)?;
                reload_profile_state(app, db, paths)?;
            }
            KeyCode::Char('+') | KeyCode::Char('=') if app.page == Page::Mods => {
                adjust_selected_priority(app, db, 10)?;
                reload_profile_state(app, db, paths)?;
            }
            KeyCode::Char('-') if app.page == Page::Mods => {
                adjust_selected_priority(app, db, -10)?;
                reload_profile_state(app, db, paths)?;
            }
            KeyCode::Char('r') if app.page == Page::Imports => {
                refresh_imports(app)?;
                app.status = format!("Rescanned {}.", app.import_root.display());
            }
            KeyCode::Char('a') if app.page == Page::Imports => {
                app.import_path_input = Some(String::new());
                app.status = "Enter a mod directory or archive path; Esc cancels.".into();
            }
            KeyCode::Char('i') if app.page == Page::Imports => {
                import_selected_candidate(terminal, app, db, paths, games)?;
            }
            KeyCode::Char('f') if app.page == Page::Frameworks => {
                if let Some(framework) = catalog::FRAMEWORKS.get(app.selected) {
                    app.status = format!("Fetching {} from its upstream release…", framework.name);
                    terminal.draw(|frame| render(frame, app, games))?;
                    match install_selected_framework(app, db, paths) {
                        Ok(message) => {
                            app.mods = db.list_mods()?;
                            reload_profile_state(app, db, paths)?;
                            app.status = message;
                        }
                        Err(error) => app.status = format!("Framework install failed: {error:#}"),
                    }
                }
            }
            KeyCode::Enter if app.pending_launch => {
                return Ok(app
                    .profiles
                    .get(app.profile_index)
                    .map(|profile| profile.id.clone()));
            }
            KeyCode::Enter if app.page == Page::Imports => {
                import_selected_candidate(terminal, app, db, paths, games)?;
            }
            KeyCode::Enter if app.page == Page::Backups => {
                if let Some(archive) = app.backups.get(app.selected).cloned() {
                    app.pending_action = Some(PendingAction::RestoreBackup(archive));
                    app.status =
                        "Restore selected backup? Press y to confirm or n to cancel.".into();
                }
            }
            KeyCode::Enter if app.page == Page::Profiles => {
                app.profile_index = app.selected.min(app.profiles.len().saturating_sub(1));
                reload_profile_state(app, db, paths)?;
                if let Some(profile) = app.profiles.get(app.profile_index) {
                    app.status = format!("Selected profile {}.", profile.name);
                }
            }
            KeyCode::Char('n') if app.page == Page::Profiles => {
                app.profile_name_input = Some(String::new());
                app.status = "Enter a name for the new isolated profile; Esc cancels.".into();
            }
            KeyCode::Char('s') if app.page == Page::Profiles => {
                let source = app.profiles.get(app.profile_index);
                let target = app.profiles.get(app.selected);
                match (source, target) {
                    (Some(source), Some(target)) if source.id != target.id => {
                        app.pending_action = Some(PendingAction::ShareSaves {
                            source_id: source.id.clone(),
                            target_id: target.id.clone(),
                        });
                        app.status = format!(
                            "Share {} saves with {}? Press y to confirm or n to cancel.",
                            source.name, target.name
                        );
                    }
                    (Some(_), Some(_)) => {
                        app.status =
                            "Move the cursor to another profile without pressing Enter, then press s."
                                .into();
                    }
                    _ => app.status = "Two profiles are required for save sharing.".into(),
                }
            }
            KeyCode::Char('u') if app.page == Page::Profiles => {
                if let Some(profile) = app.profiles.get(app.selected) {
                    if app.save_sets.contains_key(&profile.id) {
                        app.pending_action = Some(PendingAction::MakeSavesPrivate {
                            profile_id: profile.id.clone(),
                        });
                        app.status = format!(
                            "Make {} saves private? Press y to confirm or n to cancel.",
                            profile.name
                        );
                    } else {
                        app.status = format!("{} already uses private saves.", profile.name);
                    }
                }
            }
            KeyCode::Char('i') if app.page == Page::Profiles => {
                if let Some(profile) = app.profiles.get(app.selected) {
                    app.pending_action = Some(PendingAction::ImportSteamSaves {
                        profile_id: profile.id.clone(),
                    });
                    app.status = format!(
                        "Import Steam saves into {}? Press y to confirm or n to cancel.",
                        profile.name
                    );
                }
            }
            KeyCode::Char('e') if app.page == Page::Profiles => {
                if let Some(profile) = app.profiles.get(app.selected) {
                    app.pending_action = Some(PendingAction::ExportSteamSaves {
                        profile_id: profile.id.clone(),
                    });
                    app.status = format!(
                        "Export {} saves to Steam? Press y to confirm or n to cancel.",
                        profile.name
                    );
                }
            }
            KeyCode::Char('p') if matches!(app.page, Page::Dashboard | Page::Profiles) => {
                prepare_selected_profile(terminal, app, db, paths, games)?;
            }
            KeyCode::Char('b') if app.page == Page::Backups => {
                create_profile_backup(terminal, app, db, paths, games)?;
            }
            KeyCode::Char('x') | KeyCode::Char('v')
                if matches!(app.page, Page::Dashboard | Page::Mods) =>
            {
                app.pending_action = Some(PendingAction::DisableAllMods);
                app.status =
                    "Disable every mod in this profile? Press y to confirm or n to cancel.".into();
            }
            KeyCode::Char('L') => {
                if app.profiles.is_empty() {
                    app.status = "Create a profile before launching.".into();
                } else if !selected_profile_is_prepared(app) {
                    app.status =
                        "Profile is not prepared. Press p on Dashboard or Profiles first.".into();
                } else {
                    app.pending_launch = true;
                    app.status =
                        "Launch armed. Press Enter to exit the TUI and start the selected profile."
                            .into();
                }
            }
            KeyCode::Char('t') if app.page == Page::Themes => {
                let themes = presets();
                if let Some(theme) = themes.get(app.selected) {
                    app.theme = theme.clone();
                    app.theme.save(&paths.config_path())?;
                    app.status = format!("Theme changed to {}.", theme.name);
                }
            }
            KeyCode::Char('?') => {
                app.status =
                    "1-9 pages • n new • s share saves • u private • i import Steam • e export Steam • p prepare • Space toggle • x vanilla • b backup • Shift-L launch"
                        .into();
            }
            _ => {
                app.pending_launch = false;
            }
        }
    }
}

fn switch_page(app: &mut App, page: Page) {
    app.page = page;
    app.selected = if page == Page::Profiles {
        app.profile_index
    } else {
        0
    };
    app.pending_launch = false;
}

fn clamp_selection(app: &mut App) {
    let count = match app.page {
        Page::Profiles => app.profiles.len(),
        Page::Mods => app.mods.len(),
        Page::Imports => app.import_candidates.len(),
        Page::Frameworks => catalog::FRAMEWORKS.len(),
        Page::Conflicts => app.conflicts.len(),
        Page::Backups => app.backups.len(),
        Page::Themes => presets().len(),
        _ => 1,
    };
    app.selected = app.selected.min(count.saturating_sub(1));
}

fn refresh_imports(app: &mut App) -> Result<()> {
    app.import_candidates = inbox::scan(&app.import_root)?;
    app.selected = app
        .selected
        .min(app.import_candidates.len().saturating_sub(1));
    Ok(())
}

fn selected_profile_is_prepared(app: &App) -> bool {
    app.profiles
        .get(app.profile_index)
        .map(|profile| app.prepared_profiles.contains(&profile.id))
        .unwrap_or(false)
}

fn reload_profile_state(app: &mut App, db: &Database, paths: &AppPaths) -> Result<()> {
    app.save_sets = db.profile_save_sets()?.into_iter().collect();
    if let Some(profile) = app.profiles.get(app.profile_index) {
        app.resolved = deps::resolve(&db.loadout(&profile.id)?, &db.list_dependencies()?);
        app.conflicts = loadout::conflicts_for_profile(db, &profile.id)?;
        app.backups = backup::list(paths, &profile.id)?;
    } else {
        app.resolved.clear();
        app.conflicts.clear();
        app.backups.clear();
    }
    Ok(())
}

fn handle_profile_name_key(
    app: &mut App,
    db: &Database,
    paths: &AppPaths,
    games: &[GameInstall],
    key: KeyCode,
) -> Result<()> {
    match key {
        KeyCode::Esc => {
            app.profile_name_input = None;
            app.status = "Profile creation cancelled.".into();
        }
        KeyCode::Backspace => {
            if let Some(input) = &mut app.profile_name_input {
                input.pop();
            }
        }
        KeyCode::Char(character) => {
            if let Some(input) = &mut app.profile_name_input {
                input.push(character);
            }
        }
        KeyCode::Enter => {
            let name = app.profile_name_input.take().unwrap_or_default();
            let result = games
                .first()
                .context("Cyberpunk is not installed")
                .and_then(|game| profiles::create(db, paths, game, &name, "UMU-Proton"));
            match result {
                Ok(profile) => {
                    app.profiles = db.list_profiles()?;
                    app.profile_index = app
                        .profiles
                        .iter()
                        .position(|item| item.id == profile.id)
                        .unwrap_or(0);
                    app.selected = app.profile_index;
                    reload_profile_state(app, db, paths)?;
                    app.status = format!(
                        "Created and selected {}. Press p to prepare it.",
                        profile.name
                    );
                }
                Err(error) => app.status = format!("Profile creation failed: {error:#}"),
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_pending_action(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &Database,
    paths: &AppPaths,
    games: &[GameInstall],
    key: KeyCode,
) -> Result<()> {
    if matches!(key, KeyCode::Char('n') | KeyCode::Esc) {
        app.pending_action = None;
        app.status = "Action cancelled.".into();
        return Ok(());
    }
    if key != KeyCode::Char('y') {
        return Ok(());
    }
    let Some(action) = app.pending_action.take() else {
        return Ok(());
    };
    let Some(profile) = app.profiles.get(app.profile_index).cloned() else {
        app.status = "No profile is selected.".into();
        return Ok(());
    };
    match action {
        PendingAction::DisableAllMods => {
            let changed = db.disable_all_mods(&profile.id)?;
            db.revision(&profile.id, "disable all mods for vanilla loadout from TUI")?;
            reload_profile_state(app, db, paths)?;
            app.status = format!(
                "Vanilla loadout selected for {}: disabled {changed} mod(s). Normal Steam was already untouched.",
                profile.name
            );
        }
        PendingAction::RestoreBackup(archive) => {
            app.status = format!("Restoring {}…", archive.display());
            terminal.draw(|frame| render(frame, app, games))?;
            match backup::restore(paths, &profile.id, &archive) {
                Ok(()) => {
                    reload_profile_state(app, db, paths)?;
                    app.status = format!("Restored backup for {}.", profile.name);
                }
                Err(error) => app.status = format!("Restore failed: {error:#}"),
            }
        }
        PendingAction::ShareSaves {
            source_id,
            target_id,
        } => {
            let source = db
                .profile_by_name_or_id(&source_id)?
                .context("source profile disappeared")?;
            let target = db
                .profile_by_name_or_id(&target_id)?
                .context("target profile disappeared")?;
            app.status = format!("Sharing {} saves with {}…", source.name, target.name);
            terminal.draw(|frame| render(frame, app, games))?;
            match saves::share(db, paths, &source, &target) {
                Ok(result) => {
                    reload_profile_state(app, db, paths)?;
                    app.status = if let Some(detached) = result.detached_target {
                        format!(
                            "{} and {} now use '{}'. Previous target state: {}",
                            source.name,
                            target.name,
                            result.save_set.name,
                            detached.display()
                        )
                    } else {
                        format!(
                            "{} and {} now use '{}'.",
                            source.name, target.name, result.save_set.name
                        )
                    };
                }
                Err(error) => app.status = format!("Save sharing failed: {error:#}"),
            }
        }
        PendingAction::MakeSavesPrivate { profile_id } => {
            let selected = db
                .profile_by_name_or_id(&profile_id)?
                .context("profile disappeared")?;
            app.status = format!(
                "Copying shared saves into {} private storage…",
                selected.name
            );
            terminal.draw(|frame| render(frame, app, games))?;
            match saves::make_private(db, paths, &selected) {
                Ok(true) => {
                    reload_profile_state(app, db, paths)?;
                    app.status = format!("{} now has a private save copy.", selected.name);
                }
                Ok(false) => app.status = format!("{} already uses private saves.", selected.name),
                Err(error) => app.status = format!("Save detach failed: {error:#}"),
            }
        }
        PendingAction::ImportSteamSaves { profile_id } => {
            let selected = db
                .profile_by_name_or_id(&profile_id)?
                .context("profile disappeared")?;
            let game = games
                .iter()
                .find(|game| game.id == selected.game_install_id)
                .context("profile game install disappeared")?;
            app.status = format!("Importing Steam saves into {}…", selected.name);
            terminal.draw(|frame| render(frame, app, games))?;
            match saves::import_from_steam(db, paths, &selected, game) {
                Ok(result) => {
                    app.status = format!(
                        "Imported {} Steam save slot(s) into {}. Backup: {}",
                        result.save_slots,
                        selected.name,
                        result
                            .backup
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "not needed".into())
                    );
                }
                Err(error) => app.status = format!("Steam save import failed: {error:#}"),
            }
        }
        PendingAction::ExportSteamSaves { profile_id } => {
            let selected = db
                .profile_by_name_or_id(&profile_id)?
                .context("profile disappeared")?;
            let game = games
                .iter()
                .find(|game| game.id == selected.game_install_id)
                .context("profile game install disappeared")?;
            app.status = format!("Exporting {} saves to Steam…", selected.name);
            terminal.draw(|frame| render(frame, app, games))?;
            match saves::export_to_steam(db, paths, &selected, game) {
                Ok(result) => {
                    app.status = format!(
                        "Exported {} save slot(s) from {} to Steam. Backup: {}",
                        result.save_slots,
                        selected.name,
                        result
                            .backup
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "Steam had no previous local saves".into())
                    );
                }
                Err(error) => app.status = format!("Steam save export failed: {error:#}"),
            }
        }
    }
    Ok(())
}

fn prepare_selected_profile(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &Database,
    paths: &AppPaths,
    games: &[GameInstall],
) -> Result<()> {
    let Some(profile) = app.profiles.get(app.profile_index).cloned() else {
        app.status = "Create a profile before preparing it.".into();
        return Ok(());
    };
    app.status = format!("Preparing {}. This can take several minutes…", profile.name);
    terminal.draw(|frame| render(frame, app, games))?;
    let result = loadout::materialize(db, paths, &profile).and_then(|materialized| {
        launcher::prepare_prefix(paths, &profile, !materialized.redmods.is_empty())
    });
    match result {
        Ok(()) => {
            app.prepared_profiles.insert(profile.id);
            app.status = format!("Prepared isolated runtime for {}.", profile.name);
        }
        Err(error) => app.status = format!("Profile preparation failed: {error:#}"),
    }
    Ok(())
}

fn create_profile_backup(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &Database,
    paths: &AppPaths,
    games: &[GameInstall],
) -> Result<()> {
    let Some(profile) = app.profiles.get(app.profile_index).cloned() else {
        app.status = "Create a profile before backing it up.".into();
        return Ok(());
    };
    app.status = format!("Backing up isolated state for {}…", profile.name);
    terminal.draw(|frame| render(frame, app, games))?;
    match backup::create(paths, &profile.id, "tui-manual") {
        Ok(archive) => {
            let shared = db
                .save_set_for_profile(&profile.id)?
                .map(|save_set| backup::create_save_set(paths, &save_set.id, "tui-manual"))
                .transpose();
            if let Err(error) = shared {
                app.status =
                    format!("Profile backup created, but shared-save backup failed: {error:#}");
                return Ok(());
            }
            app.backups = backup::list(paths, &profile.id)?;
            app.selected = 0;
            app.status = format!("Created {}.", archive.display());
        }
        Err(error) => app.status = format!("Backup failed: {error:#}"),
    }
    Ok(())
}

fn handle_import_path_key(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &mut Database,
    paths: &AppPaths,
    games: &[GameInstall],
    key: KeyCode,
) -> Result<()> {
    match key {
        KeyCode::Esc => {
            app.import_path_input = None;
            app.status = "Path import cancelled.".into();
        }
        KeyCode::Backspace => {
            if let Some(input) = &mut app.import_path_input {
                input.pop();
            }
        }
        KeyCode::Char(character) => {
            if let Some(input) = &mut app.import_path_input {
                input.push(character);
            }
        }
        KeyCode::Enter => {
            let input = app.import_path_input.take().unwrap_or_default();
            if input.trim().is_empty() {
                app.status = "No import path was entered.".into();
                return Ok(());
            }
            let source = resolve_user_path(&input)?;
            app.status = format!("Importing {}…", source.display());
            terminal.draw(|frame| render(frame, app, games))?;
            match import_and_enable(app, db, paths, &source) {
                Ok(message) => app.status = message,
                Err(error) => app.status = format!("Import failed: {error:#}"),
            }
            app.mods = db.list_mods()?;
            reload_profile_state(app, db, paths)?;
            refresh_imports(app)?;
        }
        _ => {}
    }
    Ok(())
}

fn import_selected_candidate(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &mut Database,
    paths: &AppPaths,
    games: &[GameInstall],
) -> Result<()> {
    let Some(candidate) = app.import_candidates.get(app.selected).cloned() else {
        app.status = format!(
            "No mods found in {}. Add one there or press a to enter another path.",
            app.import_root.display()
        );
        return Ok(());
    };
    app.status = format!("Importing {}…", candidate.path.display());
    terminal.draw(|frame| render(frame, app, games))?;
    match import_and_enable(app, db, paths, &candidate.path) {
        Ok(message) => app.status = message,
        Err(error) => app.status = format!("Import failed: {error:#}"),
    }
    app.mods = db.list_mods()?;
    reload_profile_state(app, db, paths)?;
    refresh_imports(app)?;
    Ok(())
}

fn import_and_enable(
    app: &App,
    db: &mut Database,
    paths: &AppPaths,
    source: &Path,
) -> Result<String> {
    let profile = app
        .profiles
        .get(app.profile_index)
        .context("create a profile before importing mods")?;
    let source = source
        .canonicalize()
        .with_context(|| format!("resolve import path {}", source.display()))?;
    let existing = db
        .list_mods()?
        .into_iter()
        .find(|release| paths_equal(Path::new(&release.source), &source));
    let (release_id, name, newly_imported) = if let Some(release) = existing {
        (release.id, release.name, false)
    } else {
        let release = import::import(db, paths, &source, ImportOptions::default())?;
        (release.id, release.name, true)
    };
    enable_release(db, profile, &release_id)?;
    Ok(format!(
        "{} {} and enabled it in {}.",
        if newly_imported {
            "Imported"
        } else {
            "Found existing"
        },
        name,
        profile.name
    ))
}

fn enable_release(db: &Database, profile: &Profile, release_id: &str) -> Result<()> {
    let current = db
        .loadout(&profile.id)?
        .into_iter()
        .find(|entry| entry.mod_id == release_id);
    db.set_loadout_entry(&LoadoutEntry {
        profile_id: profile.id.clone(),
        mod_id: release_id.into(),
        priority: current
            .as_ref()
            .map(|entry| entry.priority)
            .unwrap_or(db.next_priority(&profile.id)?),
        requested_enabled: true,
    })?;
    db.revision(&profile.id, "import and enable mod from TUI")?;
    Ok(())
}

fn resolve_user_path(input: &str) -> Result<PathBuf> {
    let trimmed = input
        .trim()
        .trim_matches(|character| character == '\'' || character == '"');
    let path = if trimmed == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?
    } else if let Some(relative) = trimmed.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?
            .join(relative)
    } else {
        PathBuf::from(trimmed)
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    left.canonicalize().unwrap_or_else(|_| left.to_path_buf())
        == right.canonicalize().unwrap_or_else(|_| right.to_path_buf())
}

fn toggle_selected(app: &mut App, db: &Database) -> Result<()> {
    let Some(profile) = app.profiles.get(app.profile_index) else {
        app.status = "Create a profile first.".into();
        return Ok(());
    };
    let Some(release) = app.mods.get(app.selected) else {
        return Ok(());
    };
    let current = db
        .loadout(&profile.id)?
        .into_iter()
        .find(|entry| entry.mod_id == release.id);
    let entry = current.unwrap_or(LoadoutEntry {
        profile_id: profile.id.clone(),
        mod_id: release.id.clone(),
        priority: db.next_priority(&profile.id)?,
        requested_enabled: false,
    });
    let enabled = !entry.requested_enabled;
    db.set_loadout_entry(&LoadoutEntry {
        requested_enabled: enabled,
        ..entry
    })?;
    db.revision(
        &profile.id,
        if enabled { "enable mod" } else { "disable mod" },
    )?;
    app.status = format!(
        "{} {}.",
        if enabled { "Enabled" } else { "Disabled" },
        release.name
    );
    Ok(())
}

fn adjust_selected_priority(app: &mut App, db: &Database, delta: i64) -> Result<()> {
    let Some(profile) = app.profiles.get(app.profile_index) else {
        app.status = "Create a profile first.".into();
        return Ok(());
    };
    let Some(release) = app.mods.get(app.selected) else {
        return Ok(());
    };
    let Some(mut entry) = db
        .loadout(&profile.id)?
        .into_iter()
        .find(|entry| entry.mod_id == release.id)
    else {
        app.status = "Toggle this mod once before changing its priority.".into();
        return Ok(());
    };
    entry.priority = entry.priority.saturating_add(delta);
    db.set_loadout_entry(&entry)?;
    db.revision(&profile.id, "change mod priority")?;
    app.status = format!("{} priority is now {}.", release.name, entry.priority);
    Ok(())
}

fn install_selected_framework(app: &App, db: &mut Database, paths: &AppPaths) -> Result<String> {
    let framework = catalog::FRAMEWORKS
        .get(app.selected)
        .context("framework selection disappeared")?;
    let profile = app
        .profiles
        .get(app.profile_index)
        .context("create a profile before enabling frameworks")?;
    let release_id = if db.mod_release(framework.id)?.is_some() {
        framework.id.to_string()
    } else {
        let downloaded = catalog::fetch(paths, framework.id)?;
        import::import(
            db,
            paths,
            &downloaded.path,
            ImportOptions {
                name: Some(framework.name.into()),
                version: downloaded.version,
            },
        )?
        .id
    };
    let current = db
        .loadout(&profile.id)?
        .into_iter()
        .find(|entry| entry.mod_id == release_id);
    db.set_loadout_entry(&LoadoutEntry {
        profile_id: profile.id.clone(),
        mod_id: release_id,
        priority: current
            .as_ref()
            .map(|entry| entry.priority)
            .unwrap_or(db.next_priority(&profile.id)?),
        requested_enabled: true,
    })?;
    db.revision(&profile.id, "install or enable framework from TUI")?;
    Ok(format!(
        "{} is installed and enabled in {}.",
        framework.name, profile.name
    ))
}

fn render(frame: &mut Frame<'_>, app: &App, games: &[GameInstall]) {
    let theme = &app.theme;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let titles = Page::ALL
        .iter()
        .enumerate()
        .map(|(index, page)| {
            Line::from(Span::styled(
                format!("{} ", page.title()),
                Style::default().fg(theme.gradient(index as u8, (Page::ALL.len() - 1) as u8)),
            ))
        })
        .collect::<Vec<_>>();
    let selected = Page::ALL
        .iter()
        .position(|page| *page == app.page)
        .unwrap_or(0);
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .block(
                Block::default()
                    .title(" CP2077 MOD CONTROL // NIGHT CITY ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.primary.into())),
            )
            .highlight_style(
                Style::default()
                    .fg(theme.accent.into())
                    .add_modifier(Modifier::BOLD),
            ),
        chunks[0],
    );

    match app.page {
        Page::Dashboard => render_dashboard(frame, chunks[1], app, games),
        Page::Profiles => render_profiles(frame, chunks[1], app),
        Page::Mods => render_mods(frame, chunks[1], app),
        Page::Imports => render_imports(frame, chunks[1], app),
        Page::Frameworks => render_frameworks(frame, chunks[1], app),
        Page::Conflicts => render_conflicts(frame, chunks[1], app),
        Page::Backups => render_backups(frame, chunks[1], app),
        Page::Health => render_health(frame, chunks[1], app, games),
        Page::Themes => render_themes(frame, chunks[1], app),
    }
    frame.render_widget(
        Paragraph::new(app.status.as_str())
            .block(Block::default().borders(Borders::ALL).title(" STATUS "))
            .style(Style::default().fg(theme.accent.into())),
        chunks[2],
    );
    if let Some(input) = &app.import_path_input {
        render_import_path_prompt(frame, app, input);
    } else if let Some(input) = &app.profile_name_input {
        render_profile_name_prompt(frame, app, input);
    } else if let Some(action) = &app.pending_action {
        render_confirmation(frame, app, action);
    }
}

fn render_dashboard(frame: &mut Frame<'_>, area: Rect, app: &App, games: &[GameInstall]) {
    let game = games.first();
    let profile = app.profiles.get(app.profile_index);
    let enabled = app
        .resolved
        .iter()
        .filter(|entry| entry.effective_enabled)
        .count();
    let broken = app
        .resolved
        .iter()
        .filter(|entry| entry.entry.requested_enabled && !entry.effective_enabled)
        .count();
    let lines = vec![
        Line::from(vec![
            Span::styled("GAME       ", Style::default().fg(app.theme.primary.into())),
            Span::raw(
                game.map(|game| game.root.display().to_string())
                    .unwrap_or_else(|| "Not detected".into()),
            ),
        ]),
        Line::from(format!(
            "BUILD      {}",
            game.map(|game| game.build_id.as_str()).unwrap_or("unknown")
        )),
        Line::from(format!(
            "PROFILE    {}",
            profile
                .map(|profile| profile.name.as_str())
                .unwrap_or("none")
        )),
        Line::from(format!("LOADOUT    {enabled} active / {broken} blocked")),
        Line::from(format!(
            "RUNTIME    {}",
            if selected_profile_is_prepared(app) {
                "prepared"
            } else {
                "not prepared"
            }
        )),
        Line::from(format!(
            "REDMOD     {}",
            if game.map(|game| game.redmod).unwrap_or(false) {
                "installed"
            } else {
                "missing"
            }
        )),
        Line::from(""),
        Line::from(
            "Normal Steam launch remains untouched. Modded sessions use UMU and a private overlay.",
        ),
        Line::from("p prepares • x selects a vanilla loadout • Shift-L then Enter launches"),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .title(" SYSTEM OVERVIEW ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_profiles(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = app
        .profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let marker = if index == app.profile_index {
                "●"
            } else {
                "○"
            };
            ListItem::new(format!(
                "{marker} {}  [{}]  saves={}  build={}  runner={}",
                profile.name,
                if app.prepared_profiles.contains(&profile.id) {
                    "PREPARED"
                } else {
                    "NEEDS PREP"
                },
                app.save_sets
                    .get(&profile.id)
                    .map(|save_set| save_set.name.as_str())
                    .unwrap_or("private"),
                profile.game_build_id,
                profile.runner
            ))
        })
        .collect::<Vec<_>>();
    let mut state = ratatui::widgets::ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" PROFILES // ENTER SELECT // S SHARE // U PRIVATE // I IMPORT // E EXPORT // N NEW // P PREP ")
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .bg(app.theme.surface.into())
                    .fg(app.theme.accent.into()),
            )
            .highlight_symbol("▶ "),
        area,
        &mut state,
    );
}

fn render_mods(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let state = app
        .resolved
        .iter()
        .map(|entry| (entry.entry.mod_id.as_str(), entry))
        .collect::<std::collections::BTreeMap<_, _>>();
    let rows = app.mods.iter().map(|release| {
        let (status, reason) = match state.get(release.id.as_str()) {
            Some(entry) if entry.effective_enabled => ("[ON]", ""),
            Some(entry) if entry.entry.requested_enabled => (
                "[--]",
                entry.disabled_reason.as_deref().unwrap_or("dependency"),
            ),
            _ => ("[  ]", ""),
        };
        Row::new(vec![
            Cell::from(status),
            Cell::from(release.name.clone()),
            Cell::from(release.version.clone()),
            Cell::from(release.kind.as_str()),
            Cell::from(
                state
                    .get(release.id.as_str())
                    .map(|entry| entry.entry.priority.to_string())
                    .unwrap_or_default(),
            ),
            Cell::from(reason),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Percentage(35),
            Constraint::Length(12),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Min(15),
        ],
    )
    .header(
        Row::new(["STATE", "MOD", "VERSION", "TYPE", "PRIORITY", "HEALTH"]).style(
            Style::default()
                .fg(app.theme.accent.into())
                .add_modifier(Modifier::BOLD),
        ),
    )
    .row_highlight_style(
        Style::default()
            .bg(app.theme.surface.into())
            .fg(app.theme.accent.into()),
    )
    .highlight_symbol("▶ ")
    .block(
        Block::default()
            .title(" MOD LOADOUT // SPACE TO TOGGLE ")
            .borders(Borders::ALL),
    );
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_imports(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = app.import_candidates.iter().map(|candidate| {
        let imported = app
            .mods
            .iter()
            .any(|release| paths_equal(Path::new(&release.source), &candidate.path));
        Row::new(vec![
            if imported {
                "IMPORTED".to_string()
            } else {
                "NEW".to_string()
            },
            candidate
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| candidate.path.display().to_string()),
            candidate.kind.as_str().to_string(),
            candidate.path.display().to_string(),
        ])
    });
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Percentage(28),
                Constraint::Length(11),
                Constraint::Min(24),
            ],
        )
        .header(
            Row::new(["STATE", "ITEM", "TYPE", "SOURCE"]).style(
                Style::default()
                    .fg(app.theme.accent.into())
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .row_highlight_style(
            Style::default()
                .bg(app.theme.surface.into())
                .fg(app.theme.accent.into()),
        )
        .highlight_symbol("▶ ")
        .block(
            Block::default()
                .title(format!(
                    " MOD INBOX {} // ENTER/I IMPORT // A OTHER PATH // R RESCAN ",
                    app.import_root.display()
                ))
                .borders(Borders::ALL),
        ),
        area,
        &mut state,
    );
}

fn render_import_path_prompt(frame: &mut Frame<'_>, app: &App, input: &str) {
    render_text_prompt(
        frame,
        app,
        input,
        " IMPORT DIRECTORY OR ARCHIVE // ENTER CONFIRMS // ESC CANCELS ",
    );
}

fn render_profile_name_prompt(frame: &mut Frame<'_>, app: &App, input: &str) {
    render_text_prompt(
        frame,
        app,
        input,
        " NEW PROFILE NAME // ENTER CREATES // ESC CANCELS ",
    );
}

fn render_text_prompt(frame: &mut Frame<'_>, app: &App, input: &str, title: &str) {
    let outer = frame.area();
    let width = outer.width.saturating_sub(4).clamp(1, 96);
    let height = 5.min(outer.height);
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(input)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.theme.primary.into())),
            )
            .style(Style::default().fg(app.theme.accent.into())),
        area,
    );
    let cursor_offset = input.chars().count().min(width.saturating_sub(3) as usize) as u16;
    frame.set_cursor_position((area.x + 1 + cursor_offset, area.y + 1));
}

fn render_confirmation(frame: &mut Frame<'_>, app: &App, action: &PendingAction) {
    let message = match action {
        PendingAction::DisableAllMods => {
            "Disable all mods in the selected profile?\n\nThis creates a reversible vanilla loadout revision. The Steam game remains untouched."
                .to_string()
        }
        PendingAction::RestoreBackup(path) => format!(
            "Restore {}?\n\nA pre-restore backup will be created first.",
            path.file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default()
        ),
        PendingAction::ShareSaves {
            source_id,
            target_id,
        } => {
            let source = app
                .profiles
                .iter()
                .find(|profile| profile.id == *source_id)
                .map(|profile| profile.name.as_str())
                .unwrap_or("source profile");
            let target = app
                .profiles
                .iter()
                .find(|profile| profile.id == *target_id)
                .map(|profile| profile.name.as_str())
                .unwrap_or("target profile");
            format!(
                "Share {source}'s modded saves with {target}?\n\nBoth private states are backed up first. Steam's vanilla saves are never accessed."
            )
        }
        PendingAction::MakeSavesPrivate { profile_id } => {
            let profile = app
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id)
                .map(|profile| profile.name.as_str())
                .unwrap_or("profile");
            format!(
                "Detach {profile} from its shared save set?\n\nThe current shared saves are copied into private profile storage."
            )
        }
        PendingAction::ImportSteamSaves { profile_id } => {
            let profile = app
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id)
                .map(|profile| profile.name.as_str())
                .unwrap_or("profile");
            format!(
                "Replace {profile}'s modded saves with the current local Steam saves?\n\nThe modded destination is backed up first. Steam must be fully closed. Steam files are only read."
            )
        }
        PendingAction::ExportSteamSaves { profile_id } => {
            let profile = app
                .profiles
                .iter()
                .find(|profile| profile.id == *profile_id)
                .map(|profile| profile.name.as_str())
                .unwrap_or("profile");
            format!(
                "Replace the local vanilla Steam saves with {profile}'s modded saves?\n\nSteam's destination is backed up first. Steam must be fully closed. Steam Cloud may present a conflict afterward."
            )
        }
    };
    let outer = frame.area();
    let width = outer.width.saturating_sub(4).clamp(1, 86);
    let height = 8.min(outer.height);
    let area = Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(message).wrap(Wrap { trim: true }).block(
            Block::default()
                .title(" CONFIRM // Y YES // N OR ESC CANCELS ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.primary.into())),
        ),
        area,
    );
}

fn render_conflicts(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = app.conflicts.iter().map(|(path, owners)| {
        let names = owners
            .iter()
            .map(|owner| {
                app.mods
                    .iter()
                    .find(|release| release.id == *owner)
                    .map(|release| release.name.as_str())
                    .unwrap_or(owner)
            })
            .collect::<Vec<_>>()
            .join(" > ");
        Row::new(vec![path.display().to_string(), names])
    });
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(
        Table::new(
            rows,
            [Constraint::Percentage(55), Constraint::Percentage(45)],
        )
        .header(
            Row::new(["GAME PATH", "OWNERS (HIGHER PRIORITY WINS)"])
                .style(Style::default().fg(app.theme.accent.into())),
        )
        .row_highlight_style(
            Style::default()
                .bg(app.theme.surface.into())
                .fg(app.theme.accent.into()),
        )
        .highlight_symbol("▶ ")
        .block(
            Block::default()
                .title(format!(" FILE CONFLICTS // {} FOUND ", app.conflicts.len()))
                .borders(Borders::ALL),
        ),
        area,
        &mut state,
    );
}

fn render_backups(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = app
        .backups
        .iter()
        .map(|path| {
            ListItem::new(
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
            )
        })
        .collect::<Vec<_>>();
    let mut state = ratatui::widgets::ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" PROFILE BACKUPS // B CREATE // ENTER RESTORE ")
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .bg(app.theme.surface.into())
                    .fg(app.theme.accent.into()),
            )
            .highlight_symbol("▶ "),
        area,
        &mut state,
    );
}

fn render_health(frame: &mut Frame<'_>, area: Rect, app: &App, games: &[GameInstall]) {
    let mut rows = doctor::run(games)
        .into_iter()
        .map(|check| {
            Row::new(vec![
                if check.ok {
                    "OK".to_string()
                } else {
                    "WARN".to_string()
                },
                check.name,
                check.detail,
                check.remedy.unwrap_or_default(),
            ])
        })
        .collect::<Vec<_>>();
    rows.push(Row::new(vec![
        if selected_profile_is_prepared(app) {
            "OK".to_string()
        } else {
            "WARN".to_string()
        },
        "Selected profile".to_string(),
        if selected_profile_is_prepared(app) {
            "Isolated runtime is prepared".to_string()
        } else {
            "Isolated runtime needs preparation".to_string()
        },
        if selected_profile_is_prepared(app) {
            String::new()
        } else {
            "Press p on Dashboard or Profiles".to_string()
        },
    ]));
    let broken = app
        .resolved
        .iter()
        .filter(|entry| entry.entry.requested_enabled && !entry.effective_enabled)
        .count();
    rows.push(Row::new(vec![
        if broken == 0 { "OK" } else { "WARN" }.to_string(),
        "Dependencies".to_string(),
        if broken == 0 {
            "All requested mods can be enabled".to_string()
        } else {
            format!("{broken} requested mod(s) are blocked")
        },
        if broken == 0 {
            String::new()
        } else {
            "Inspect the Mods and Core pages".to_string()
        },
    ]));
    rows.push(Row::new(vec![
        if app.conflicts.is_empty() {
            "OK"
        } else {
            "INFO"
        }
        .to_string(),
        "File conflicts".to_string(),
        format!("{} exact-path conflict(s)", app.conflicts.len()),
        if app.conflicts.is_empty() {
            String::new()
        } else {
            "Review page 6; priority determines the winner".to_string()
        },
    ]));
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Length(22),
                Constraint::Percentage(38),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(["STATE", "CHECK", "DETAIL", "REMEDY"])
                .style(Style::default().fg(app.theme.accent.into())),
        )
        .block(
            Block::default()
                .title(" LOADOUT HEALTH ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_frameworks(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = catalog::FRAMEWORKS.iter().map(|framework| {
        let installed = app.mods.iter().any(|release| release.id == framework.id);
        Row::new(vec![
            if installed {
                "INSTALLED".to_string()
            } else {
                "AVAILABLE".to_string()
            },
            framework.name.to_string(),
            if framework.requires.is_empty() {
                "—".to_string()
            } else {
                framework.requires.join(", ")
            },
            framework.repository.to_string(),
        ])
    });
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(11),
                Constraint::Length(23),
                Constraint::Length(20),
                Constraint::Min(20),
            ],
        )
        .header(
            Row::new(["STATE", "FRAMEWORK", "REQUIRES", "UPSTREAM"]).style(
                Style::default()
                    .fg(app.theme.accent.into())
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .row_highlight_style(
            Style::default()
                .bg(app.theme.surface.into())
                .fg(app.theme.accent.into()),
        )
        .highlight_symbol("▶ ")
        .block(
            Block::default()
                .title(" CORE FRAMEWORKS // F TO FETCH + ENABLE ")
                .borders(Borders::ALL),
        ),
        area,
        &mut state,
    );
}

fn render_themes(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = presets()
        .into_iter()
        .map(|theme| {
            ListItem::new(Line::from(vec![
                Span::styled("██", Style::default().fg(theme.primary.into())),
                Span::styled("██", Style::default().fg(theme.accent.into())),
                Span::raw(format!("  {}", theme.name)),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ratatui::widgets::ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" THEME PRESETS // T TO APPLY ")
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .bg(app.theme.surface.into())
                    .fg(app.theme.accent.into()),
            )
            .highlight_symbol("▶ "),
        area,
        &mut state,
    );
}
