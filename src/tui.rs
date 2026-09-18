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
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Wrap},
};

use crate::{
    backup, catalog,
    db::Database,
    deps, doctor,
    import::{self, ImportOptions},
    inbox::{self, ImportCandidate},
    launcher, loadout,
    models::{Dependency, GameInstall, LoadoutEntry, ModRelease, Profile, ResolvedEntry, SaveSet},
    paths::AppPaths,
    profiles, saves,
    theme::{Theme, presets},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Loadout,
    Add,
    Runtime,
}

impl Mode {
    const ALL: [Mode; 3] = [Mode::Loadout, Mode::Add, Mode::Runtime];

    fn number(self) -> u8 {
        match self {
            Self::Loadout => 1,
            Self::Add => 2,
            Self::Runtime => 3,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Loadout => "Loadout",
            Self::Add => "Add",
            Self::Runtime => "Runtime",
        }
    }

    fn short_title(self) -> &'static str {
        match self {
            Self::Loadout => "Load",
            Self::Add => "Add",
            Self::Runtime => "Run",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Sidebar,
    Primary,
    Secondary,
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
    FrameworkPresent {
        framework_id: String,
        release_id: String,
        version: String,
        copies: usize,
    },
    DeleteMods {
        ids: Vec<String>,
        label: String,
        used_by: Vec<String>,
    },
    EnableMod {
        release_id: String,
        release_name: String,
        enable_deps: Vec<(String, String)>,
        missing_deps: Vec<String>,
        disable_dups: Vec<(String, String)>,
    },
}

struct App {
    mode: Mode,
    pane: Pane,
    loadout_index: usize,
    conflict_index: usize,
    show_all_conflicts: bool,
    core_index: usize,
    inbox_index: usize,
    profile_index: usize,
    profile_cursor: usize,
    backup_index: usize,
    status: String,
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
    health_checks: Vec<doctor::Check>,
    help_open: bool,
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
        mode: Mode::Loadout,
        pane: Pane::Primary,
        loadout_index: 0,
        conflict_index: 0,
        show_all_conflicts: false,
        core_index: 0,
        inbox_index: 0,
        profile_index: 0,
        profile_cursor: 0,
        backup_index: 0,
        status: "Ready. Press ? for keys.".into(),
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
        health_checks: doctor::run(games),
        help_open: false,
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
        if app.help_open {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc => app.help_open = false,
                KeyCode::Char('q') => return Ok(None),
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            KeyCode::Char('1') => switch_mode(app, paths, Mode::Loadout)?,
            KeyCode::Char('2') => switch_mode(app, paths, Mode::Add)?,
            KeyCode::Char('3') => switch_mode(app, paths, Mode::Runtime)?,
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                app.pane = focus_left(app.pane);
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                app.pane = focus_right(app.pane);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.pane == Pane::Sidebar {
                    shift_mode(app, paths, 1)?;
                } else {
                    move_selection(app, 1);
                    if app.mode == Mode::Runtime && app.pane == Pane::Primary {
                        reload_detail_backups(app, paths)?;
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if app.pane == Pane::Sidebar {
                    shift_mode(app, paths, -1)?;
                } else {
                    move_selection(app, -1);
                    if app.mode == Mode::Runtime && app.pane == Pane::Primary {
                        reload_detail_backups(app, paths)?;
                    }
                }
            }
            KeyCode::Char(' ') if app.mode == Mode::Loadout && app.pane == Pane::Primary => {
                request_toggle_selected(app, db, paths)?;
            }
            KeyCode::Char('+') | KeyCode::Char('=')
                if app.mode == Mode::Loadout && app.pane == Pane::Primary =>
            {
                adjust_selected_priority(app, db, 10)?;
                reload_profile_state(app, db, paths)?;
            }
            KeyCode::Char('-') if app.mode == Mode::Loadout && app.pane == Pane::Primary => {
                adjust_selected_priority(app, db, -10)?;
                reload_profile_state(app, db, paths)?;
            }
            KeyCode::Char('c') if app.mode == Mode::Loadout => {
                if app.pane == Pane::Secondary {
                    app.pane = Pane::Primary;
                    app.status = "Mods table.".into();
                } else {
                    app.pane = Pane::Secondary;
                    app.conflict_index = 0;
                    app.status = if app.show_all_conflicts {
                        "File conflicts (all). Press C to filter to the highlighted mod, c to return."
                            .into()
                    } else {
                        "File conflicts for the highlighted mod. Press C for all, c to return."
                            .into()
                    };
                }
            }
            KeyCode::Char('C') if app.mode == Mode::Loadout => {
                app.show_all_conflicts = !app.show_all_conflicts;
                app.pane = Pane::Secondary;
                app.conflict_index = 0;
                app.status = if app.show_all_conflicts {
                    "Showing all file conflicts.".into()
                } else {
                    "Showing conflicts for the highlighted mod.".into()
                };
            }
            KeyCode::Char('r') if app.mode == Mode::Add && app.pane == Pane::Secondary => {
                refresh_imports(app)?;
                app.status = format!("Rescanned {}.", app.import_root.display());
            }
            KeyCode::Char('a') if app.mode == Mode::Add => {
                app.import_path_input = Some(String::new());
                app.status = "Enter a mod directory or archive path; Esc cancels.".into();
            }
            KeyCode::Char('i') if app.mode == Mode::Add && app.pane == Pane::Secondary => {
                import_selected_candidate(terminal, app, db, paths, games)?;
            }
            KeyCode::Char('f') if app.mode == Mode::Add && app.pane == Pane::Primary => {
                request_framework_install(terminal, app, db, paths, games)?;
            }
            KeyCode::Char('d') if app.mode == Mode::Loadout && app.pane == Pane::Primary => {
                request_delete_selected_mod(app, db)?;
            }
            KeyCode::Char('d') if app.mode == Mode::Add && app.pane == Pane::Primary => {
                request_delete_selected_framework(app, db)?;
            }
            KeyCode::Enter if app.pane == Pane::Sidebar => {
                app.pane = Pane::Primary;
            }
            KeyCode::Enter if app.mode == Mode::Add && app.pane == Pane::Secondary => {
                import_selected_candidate(terminal, app, db, paths, games)?;
            }
            KeyCode::Enter if app.mode == Mode::Runtime && app.pane == Pane::Secondary => {
                if let Some(archive) = app.backups.get(app.backup_index).cloned() {
                    app.pending_action = Some(PendingAction::RestoreBackup(archive));
                    app.status =
                        "Restore selected backup? Press y to confirm or n to cancel.".into();
                }
            }
            KeyCode::Enter if app.mode == Mode::Runtime && app.pane == Pane::Primary => {
                app.profile_index = app.profile_cursor.min(app.profiles.len().saturating_sub(1));
                reload_profile_state(app, db, paths)?;
                if let Some(profile) = app.profiles.get(app.profile_index) {
                    app.status = format!("Selected profile {}.", profile.name);
                }
            }
            KeyCode::Char('n') if app.mode == Mode::Runtime => {
                app.profile_name_input = Some(String::new());
                app.status = "Enter a name for the new isolated profile; Esc cancels.".into();
            }
            KeyCode::Char('s') if app.mode == Mode::Runtime => {
                let source = app.profiles.get(app.profile_index);
                let target = app.profiles.get(app.profile_cursor);
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
            KeyCode::Char('u') if app.mode == Mode::Runtime => {
                if let Some(profile) = app.profiles.get(app.profile_cursor) {
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
            KeyCode::Char('i') if app.mode == Mode::Runtime => {
                if let Some(profile) = app.profiles.get(app.profile_cursor) {
                    app.pending_action = Some(PendingAction::ImportSteamSaves {
                        profile_id: profile.id.clone(),
                    });
                    app.status = format!(
                        "Import Steam saves into {}? Press y to confirm or n to cancel.",
                        profile.name
                    );
                }
            }
            KeyCode::Char('e') if app.mode == Mode::Runtime => {
                if let Some(profile) = app.profiles.get(app.profile_cursor) {
                    app.pending_action = Some(PendingAction::ExportSteamSaves {
                        profile_id: profile.id.clone(),
                    });
                    app.status = format!(
                        "Export {} saves to Steam? Press y to confirm or n to cancel.",
                        profile.name
                    );
                }
            }
            KeyCode::Char('p') if matches!(app.mode, Mode::Loadout | Mode::Runtime) => {
                prepare_selected_profile(terminal, app, db, paths, games)?;
            }
            KeyCode::Char('b') if app.mode == Mode::Runtime => {
                create_profile_backup(terminal, app, db, paths, games)?;
            }
            KeyCode::Char('x') | KeyCode::Char('v') if app.mode == Mode::Loadout => {
                app.pending_action = Some(PendingAction::DisableAllMods);
                app.status =
                    "Disable every mod in this profile? Press y to confirm or n to cancel.".into();
            }
            KeyCode::Char('L') => {
                if app.profiles.is_empty() {
                    app.status = "Create a profile before launching.".into();
                } else if !selected_profile_is_prepared(app) {
                    app.status = "Profile is not prepared. Press p to prepare it first.".into();
                } else {
                    return Ok(app
                        .profiles
                        .get(app.profile_index)
                        .map(|profile| profile.id.clone()));
                }
            }
            KeyCode::Char('t') => {
                let themes = presets();
                let current = themes
                    .iter()
                    .position(|theme| theme.name == app.theme.name)
                    .unwrap_or(0);
                let next = themes[(current + 1) % themes.len()].clone();
                app.theme = next;
                app.theme.save(&paths.config_path())?;
                app.status = format!("Theme changed to {}.", app.theme.name);
            }
            KeyCode::Char('?') => {
                app.help_open = true;
            }
            _ => {}
        }
    }
}

fn focus_left(pane: Pane) -> Pane {
    match pane {
        Pane::Secondary => Pane::Primary,
        Pane::Primary | Pane::Sidebar => Pane::Sidebar,
    }
}

fn focus_right(pane: Pane) -> Pane {
    match pane {
        Pane::Sidebar => Pane::Primary,
        Pane::Primary | Pane::Secondary => Pane::Secondary,
    }
}

fn switch_mode(app: &mut App, paths: &AppPaths, mode: Mode) -> Result<()> {
    app.mode = mode;
    app.pane = Pane::Primary;
    if mode == Mode::Runtime {
        app.profile_cursor = app.profile_index;
        reload_detail_backups(app, paths)?;
    }
    Ok(())
}

fn shift_mode(app: &mut App, paths: &AppPaths, delta: i8) -> Result<()> {
    let index = Mode::ALL
        .iter()
        .position(|mode| *mode == app.mode)
        .unwrap_or(0) as i8;
    let next = (index + delta).rem_euclid(Mode::ALL.len() as i8) as usize;
    app.mode = Mode::ALL[next];
    if app.mode == Mode::Runtime {
        app.profile_cursor = app.profile_index;
        reload_detail_backups(app, paths)?;
    }
    Ok(())
}

fn move_selection(app: &mut App, delta: i32) {
    if app.pane == Pane::Sidebar {
        return;
    }
    let count = pane_len(app);
    if count == 0 {
        return;
    }
    let index = current_index_mut(app);
    if delta > 0 {
        *index = (*index + 1).min(count.saturating_sub(1));
    } else {
        *index = index.saturating_sub(1);
    }
    if app.mode == Mode::Loadout && app.pane == Pane::Primary {
        app.conflict_index = 0;
    }
}

fn current_index_mut(app: &mut App) -> &mut usize {
    match (app.mode, app.pane) {
        (_, Pane::Sidebar) => &mut app.loadout_index,
        (Mode::Loadout, Pane::Primary) => &mut app.loadout_index,
        (Mode::Loadout, Pane::Secondary) => &mut app.conflict_index,
        (Mode::Add, Pane::Primary) => &mut app.core_index,
        (Mode::Add, Pane::Secondary) => &mut app.inbox_index,
        (Mode::Runtime, Pane::Primary) => &mut app.profile_cursor,
        (Mode::Runtime, Pane::Secondary) => &mut app.backup_index,
    }
}

fn pane_len(app: &App) -> usize {
    match (app.mode, app.pane) {
        (_, Pane::Sidebar) => 0,
        (Mode::Loadout, Pane::Primary) => app.mods.len(),
        (Mode::Loadout, Pane::Secondary) => visible_conflicts(app).len(),
        (Mode::Add, Pane::Primary) => catalog::FRAMEWORKS.len(),
        (Mode::Add, Pane::Secondary) => app.import_candidates.len(),
        (Mode::Runtime, Pane::Primary) => app.profiles.len(),
        (Mode::Runtime, Pane::Secondary) => app.backups.len(),
    }
}

fn clamp_all_selections(app: &mut App) {
    app.loadout_index = app.loadout_index.min(app.mods.len().saturating_sub(1));
    app.conflict_index = app
        .conflict_index
        .min(visible_conflicts(app).len().saturating_sub(1));
    app.core_index = app
        .core_index
        .min(catalog::FRAMEWORKS.len().saturating_sub(1));
    app.inbox_index = app
        .inbox_index
        .min(app.import_candidates.len().saturating_sub(1));
    app.profile_cursor = app.profile_cursor.min(app.profiles.len().saturating_sub(1));
    app.backup_index = app.backup_index.min(app.backups.len().saturating_sub(1));
}

fn refresh_imports(app: &mut App) -> Result<()> {
    app.import_candidates = inbox::scan(&app.import_root)?;
    app.inbox_index = app
        .inbox_index
        .min(app.import_candidates.len().saturating_sub(1));
    Ok(())
}

fn reload_detail_backups(app: &mut App, paths: &AppPaths) -> Result<()> {
    if let Some(profile) = app.profiles.get(app.profile_cursor) {
        app.backups = backup::list(paths, &profile.id)?;
    } else {
        app.backups.clear();
    }
    app.backup_index = app.backup_index.min(app.backups.len().saturating_sub(1));
    Ok(())
}

fn visible_conflicts(app: &App) -> Vec<(PathBuf, Vec<String>)> {
    if app.show_all_conflicts {
        return app.conflicts.clone();
    }
    let Some(mod_id) = app
        .mods
        .get(app.loadout_index)
        .map(|release| release.id.as_str())
    else {
        return app.conflicts.clone();
    };
    app.conflicts
        .iter()
        .filter(|(_, owners)| owners.iter().any(|owner| owner == mod_id))
        .cloned()
        .collect()
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
                    app.profile_cursor = app.profile_index;
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
    db: &mut Database,
    paths: &AppPaths,
    games: &[GameInstall],
    key: KeyCode,
) -> Result<()> {
    if matches!(&app.pending_action, Some(PendingAction::EnableMod { .. })) {
        return handle_enable_mod_prompt(app, db, paths, key);
    }
    if matches!(key, KeyCode::Char('n') | KeyCode::Esc) {
        app.pending_action = None;
        app.status = "Action cancelled.".into();
        return Ok(());
    }
    let replacing_framework = matches!(
        app.pending_action,
        Some(PendingAction::FrameworkPresent { .. })
    ) && key == KeyCode::Char('r');
    if key != KeyCode::Char('y') && !replacing_framework {
        return Ok(());
    }
    let Some(action) = app.pending_action.take() else {
        return Ok(());
    };
    if let PendingAction::DeleteMods { ids, label, .. } = action {
        return delete_mods(app, db, paths, ids, label);
    }
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
            let target = app
                .profiles
                .get(app.profile_cursor)
                .cloned()
                .unwrap_or(profile);
            app.status = format!("Restoring {}…", archive.display());
            terminal.draw(|frame| render(frame, app, games))?;
            match backup::restore(paths, &target.id, &archive) {
                Ok(()) => {
                    reload_profile_state(app, db, paths)?;
                    app.status = format!("Restored backup for {}.", target.name);
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
        PendingAction::FrameworkPresent {
            framework_id,
            release_id,
            ..
        } => {
            if replacing_framework {
                fetch_and_replace_framework(terminal, app, db, paths, games, &framework_id)?;
            } else {
                enable_release(db, &profile, &release_id)?;
                reload_profile_state(app, db, paths)?;
                app.status = format!(
                    "Enabled existing {} in {}.",
                    catalog::FRAMEWORKS
                        .iter()
                        .find(|item| item.id == framework_id)
                        .map(|item| item.name)
                        .unwrap_or(framework_id.as_str()),
                    profile.name
                );
            }
        }
        PendingAction::DeleteMods { .. } => {}
        PendingAction::EnableMod { .. } => {}
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
            app.health_checks = doctor::run(games);
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
    let Some(profile) = app.profiles.get(app.profile_cursor).cloned() else {
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
            app.backup_index = 0;
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
    let Some(candidate) = app.import_candidates.get(app.inbox_index).cloned() else {
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

fn request_toggle_selected(app: &mut App, db: &Database, paths: &AppPaths) -> Result<()> {
    let Some(profile) = app.profiles.get(app.profile_index).cloned() else {
        app.status = "Create a profile first.".into();
        return Ok(());
    };
    let Some(release) = app.mods.get(app.loadout_index).cloned() else {
        return Ok(());
    };
    let loadout = db.loadout(&profile.id)?;
    let currently_enabled = loadout
        .iter()
        .find(|entry| entry.mod_id == release.id)
        .map(|entry| entry.requested_enabled)
        .unwrap_or(false);
    if currently_enabled {
        set_requested(db, &profile, &release.id, false)?;
        db.revision(&profile.id, "disable mod")?;
        reload_profile_state(app, db, paths)?;
        app.status = format!("Disabled {}.", release.name);
        return Ok(());
    }
    let plan = enable_plan(&release, &app.mods, &loadout, &db.list_dependencies()?);
    if plan.needs_prompt() {
        app.status = format!(
            "Enable {} with fixes? y apply, n enable only, Esc cancel.",
            release.name
        );
        app.pending_action = Some(PendingAction::EnableMod {
            release_id: release.id,
            release_name: release.name,
            enable_deps: plan.enable_deps,
            missing_deps: plan.missing_deps,
            disable_dups: plan.disable_dups,
        });
        return Ok(());
    }
    apply_enable_plan(app, db, paths, &profile, &release.id, &plan, true)?;
    Ok(())
}

#[derive(Debug, Default)]
struct EnablePlan {
    enable_deps: Vec<(String, String)>,
    missing_deps: Vec<String>,
    disable_dups: Vec<(String, String)>,
}

impl EnablePlan {
    fn needs_prompt(&self) -> bool {
        !self.enable_deps.is_empty()
            || !self.missing_deps.is_empty()
            || !self.disable_dups.is_empty()
    }
}

fn release_satisfies(release: &ModRelease, requires_id: &str) -> bool {
    if release.id == requires_id {
        return true;
    }
    catalog::FRAMEWORKS.iter().any(|framework| {
        framework.id == requires_id
            && (release.id == framework.id || release.name.eq_ignore_ascii_case(framework.name))
    })
}

fn requirement_label(requires_id: &str) -> String {
    catalog::FRAMEWORKS
        .iter()
        .find(|framework| framework.id == requires_id)
        .map(|framework| framework.name.to_string())
        .unwrap_or_else(|| requires_id.to_string())
}

fn enable_plan(
    release: &ModRelease,
    mods: &[ModRelease],
    loadout: &[LoadoutEntry],
    deps: &[Dependency],
) -> EnablePlan {
    let requested: BTreeSet<&str> = loadout
        .iter()
        .filter(|entry| entry.requested_enabled)
        .map(|entry| entry.mod_id.as_str())
        .collect();
    let graph: BTreeMap<&str, Vec<&str>> = deps.iter().fold(BTreeMap::new(), |mut map, dep| {
        map.entry(dep.mod_id.as_str())
            .or_default()
            .push(dep.requires_id.as_str());
        map
    });
    let mut plan = EnablePlan::default();
    let mut stack = vec![release.id.as_str()];
    let mut seen = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        for requirement in graph.get(id).into_iter().flatten().copied() {
            if let Some(required) = mods
                .iter()
                .find(|candidate| release_satisfies(candidate, requirement))
            {
                stack.push(required.id.as_str());
                if required.id != release.id && !requested.contains(required.id.as_str()) {
                    plan.enable_deps
                        .push((required.id.clone(), required.name.clone()));
                }
            } else {
                plan.missing_deps.push(requirement_label(requirement));
            }
        }
    }
    plan.enable_deps.sort();
    plan.enable_deps.dedup();
    plan.missing_deps.sort();
    plan.missing_deps.dedup();
    for candidate in mods {
        if candidate.id != release.id
            && same_logical_mod(release, candidate)
            && requested.contains(candidate.id.as_str())
        {
            plan.disable_dups
                .push((candidate.id.clone(), candidate.name.clone()));
        }
    }
    plan
}

fn handle_enable_mod_prompt(
    app: &mut App,
    db: &Database,
    paths: &AppPaths,
    key: KeyCode,
) -> Result<()> {
    match key {
        KeyCode::Esc => {
            app.pending_action = None;
            app.status = "Enable cancelled.".into();
        }
        KeyCode::Char('n') | KeyCode::Char('y') => {
            let Some(PendingAction::EnableMod {
                release_id,
                enable_deps,
                missing_deps,
                disable_dups,
                ..
            }) = app.pending_action.take()
            else {
                return Ok(());
            };
            let Some(profile) = app.profiles.get(app.profile_index).cloned() else {
                app.status = "No profile is selected.".into();
                return Ok(());
            };
            let safe = key == KeyCode::Char('y');
            let plan = EnablePlan {
                enable_deps,
                missing_deps,
                disable_dups,
            };
            apply_enable_plan(app, db, paths, &profile, &release_id, &plan, safe)?;
        }
        _ => {}
    }
    Ok(())
}

fn apply_enable_plan(
    app: &mut App,
    db: &Database,
    paths: &AppPaths,
    profile: &Profile,
    release_id: &str,
    plan: &EnablePlan,
    safe: bool,
) -> Result<()> {
    set_requested(db, profile, release_id, true)?;
    let mut extra = Vec::new();
    if safe {
        for (id, name) in &plan.enable_deps {
            set_requested(db, profile, id, true)?;
            extra.push(format!("enabled {name}"));
        }
        for (id, name) in &plan.disable_dups {
            set_requested(db, profile, id, false)?;
            extra.push(format!("disabled duplicate {name}"));
        }
        if !plan.missing_deps.is_empty() {
            extra.push(format!(
                "still missing {} — fetch from Add (2, f)",
                plan.missing_deps.join(", ")
            ));
        }
    } else if !plan.missing_deps.is_empty() {
        extra.push(format!(
            "still missing {} — fetch from Add (2, f)",
            plan.missing_deps.join(", ")
        ));
    }
    db.revision(profile.id.as_str(), "enable mod from TUI")?;
    reload_profile_state(app, db, paths)?;
    let clashes = app
        .conflicts
        .iter()
        .filter(|(_, owners)| owners.iter().any(|owner| owner == release_id))
        .count();
    let release_name = app
        .mods
        .iter()
        .find(|release| release.id == release_id)
        .map(|release| release.name.as_str())
        .unwrap_or(release_id);
    let mut status = format!("Enabled {release_name}.");
    if !extra.is_empty() {
        status.push(' ');
        status.push_str(&extra.join("; "));
        status.push('.');
    }
    if clashes > 0 {
        status.push_str(&format!(
            " {clashes} file conflict(s). Press c. +/- sets the winner."
        ));
    }
    app.status = status;
    Ok(())
}

fn set_requested(db: &Database, profile: &Profile, mod_id: &str, enabled: bool) -> Result<()> {
    let current = db
        .loadout(&profile.id)?
        .into_iter()
        .find(|entry| entry.mod_id == mod_id);
    let entry = current.unwrap_or(LoadoutEntry {
        profile_id: profile.id.clone(),
        mod_id: mod_id.into(),
        priority: db.next_priority(&profile.id)?,
        requested_enabled: enabled,
    });
    db.set_loadout_entry(&LoadoutEntry {
        requested_enabled: enabled,
        ..entry
    })?;
    Ok(())
}

fn adjust_selected_priority(app: &mut App, db: &Database, delta: i64) -> Result<()> {
    let Some(profile) = app.profiles.get(app.profile_index) else {
        app.status = "Create a profile first.".into();
        return Ok(());
    };
    let Some(release) = app.mods.get(app.loadout_index) else {
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

fn request_framework_install(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &mut Database,
    paths: &AppPaths,
    games: &[GameInstall],
) -> Result<()> {
    let Some(framework) = catalog::FRAMEWORKS.get(app.core_index) else {
        return Ok(());
    };
    let matches = catalog::matching_releases(&app.mods, framework);
    if let Some(existing) = matches.first() {
        app.pending_action = Some(PendingAction::FrameworkPresent {
            framework_id: framework.id.into(),
            release_id: existing.id.clone(),
            version: existing.version.clone(),
            copies: matches.len(),
        });
        app.status = format!(
            "{} {} is already installed ({} cop{}). y enable existing, r replace with latest, n cancel.",
            framework.name,
            existing.version,
            matches.len(),
            if matches.len() == 1 { "y" } else { "ies" }
        );
        return Ok(());
    }
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
    Ok(())
}

fn fetch_and_replace_framework(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &mut Database,
    paths: &AppPaths,
    games: &[GameInstall],
    framework_id: &str,
) -> Result<()> {
    let Some(framework) = catalog::FRAMEWORKS
        .iter()
        .find(|item| item.id == framework_id)
        .cloned()
    else {
        app.status = "Unknown framework.".into();
        return Ok(());
    };
    if let Some(index) = catalog::FRAMEWORKS
        .iter()
        .position(|item| item.id == framework.id)
    {
        app.mode = Mode::Add;
        app.pane = Pane::Primary;
        app.core_index = index;
    }
    app.status = format!(
        "Replacing {} with its latest upstream release…",
        framework.name
    );
    terminal.draw(|frame| render(frame, app, games))?;
    let matches: Vec<ModRelease> = catalog::matching_releases(&app.mods, &framework)
        .into_iter()
        .cloned()
        .collect();
    for release in &matches {
        if let Err(error) = import::remove_release(db, release) {
            app.status = format!("Could not remove {}: {error:#}", release.name);
            return Ok(());
        }
    }
    app.mods = db.list_mods()?;
    match install_selected_framework(app, db, paths) {
        Ok(message) => {
            app.mods = db.list_mods()?;
            reload_profile_state(app, db, paths)?;
            app.status = message;
        }
        Err(error) => app.status = format!("Framework replace failed: {error:#}"),
    }
    Ok(())
}

fn request_delete_selected_mod(app: &mut App, db: &Database) -> Result<()> {
    let Some(release) = app.mods.get(app.loadout_index) else {
        app.status = "No mod is selected.".into();
        return Ok(());
    };
    let used_by = db.profiles_using_mod(&release.id)?;
    app.pending_action = Some(PendingAction::DeleteMods {
        ids: vec![release.id.clone()],
        label: format!("{} {}", release.name, release.version),
        used_by,
    });
    app.status = format!(
        "Delete {} {} from the library? Press y to confirm or n to cancel.",
        release.name, release.version
    );
    Ok(())
}

fn request_delete_selected_framework(app: &mut App, db: &Database) -> Result<()> {
    let Some(framework) = catalog::FRAMEWORKS.get(app.core_index) else {
        return Ok(());
    };
    let matches = catalog::matching_releases(&app.mods, framework);
    if matches.is_empty() {
        app.status = format!("{} is not installed.", framework.name);
        return Ok(());
    }
    let mut used_by = BTreeSet::new();
    for release in &matches {
        used_by.extend(db.profiles_using_mod(&release.id)?);
    }
    app.pending_action = Some(PendingAction::DeleteMods {
        ids: matches.iter().map(|release| release.id.clone()).collect(),
        label: format!(
            "{} ({} cop{})",
            framework.name,
            matches.len(),
            if matches.len() == 1 { "y" } else { "ies" }
        ),
        used_by: used_by.into_iter().collect(),
    });
    app.status = format!(
        "Delete {} from the library? Press y to confirm or n to cancel.",
        framework.name
    );
    Ok(())
}

fn delete_mods(
    app: &mut App,
    db: &Database,
    paths: &AppPaths,
    ids: Vec<String>,
    label: String,
) -> Result<()> {
    let mut removed = 0usize;
    for id in ids {
        let Some(release) = db.mod_release(&id)? else {
            continue;
        };
        import::remove_release(db, &release)?;
        removed += 1;
    }
    app.mods = db.list_mods()?;
    reload_profile_state(app, db, paths)?;
    clamp_all_selections(app);
    app.status = format!("Removed {removed} release(s): {label}.");
    Ok(())
}

fn install_selected_framework(app: &App, db: &mut Database, paths: &AppPaths) -> Result<String> {
    let framework = catalog::FRAMEWORKS
        .get(app.core_index)
        .context("framework selection disappeared")?;
    let profile = app
        .profiles
        .get(app.profile_index)
        .context("create a profile before enabling frameworks")?;
    let downloaded = catalog::fetch(paths, framework.id)?;
    let release = import::import(
        db,
        paths,
        &downloaded.path,
        ImportOptions {
            id: Some(framework.id.into()),
            name: Some(framework.name.into()),
            version: downloaded.version,
        },
    )?;
    enable_release(db, profile, &release.id)?;
    Ok(format!(
        "{} {} is installed and enabled in {}.",
        framework.name, release.version, profile.name
    ))
}

fn enabled_count(app: &App) -> usize {
    app.resolved
        .iter()
        .filter(|entry| entry.effective_enabled)
        .count()
}

fn blocked_count(app: &App) -> usize {
    app.resolved
        .iter()
        .filter(|entry| entry.entry.requested_enabled && !entry.effective_enabled)
        .count()
}

fn core_installed_count(app: &App) -> usize {
    catalog::FRAMEWORKS
        .iter()
        .filter(|framework| catalog::is_installed(&app.mods, framework))
        .count()
}

fn list_highlight(app: &App, pane: Pane) -> &'static str {
    if app.pane == pane { "▶ " } else { "  " }
}

fn clash_count(release: &ModRelease, app: &App) -> usize {
    app.conflicts
        .iter()
        .filter(|(_, owners)| owners.iter().any(|owner| owner == &release.id))
        .count()
}

fn issue_flags(release: &ModRelease, app: &App) -> String {
    let mut flags = Vec::new();
    let blocked = app.resolved.iter().any(|entry| {
        entry.entry.mod_id == release.id
            && entry.entry.requested_enabled
            && !entry.effective_enabled
    });
    if blocked {
        flags.push("dep");
    }
    if clash_count(release, app) > 0 {
        flags.push("clash");
    }
    if duplicate_reason(release, &app.mods).is_some() {
        flags.push("dup");
    }
    flags.join(" ")
}

fn selected_mod_detail(app: &App) -> String {
    let Some(release) = app.mods.get(app.loadout_index) else {
        return "No mod selected.".into();
    };
    let mut parts = Vec::new();
    if let Some(entry) = app
        .resolved
        .iter()
        .find(|entry| entry.entry.mod_id == release.id)
        && entry.entry.requested_enabled
        && !entry.effective_enabled
    {
        parts.push(
            entry
                .disabled_reason
                .clone()
                .unwrap_or_else(|| "dependency".into()),
        );
    }
    let clashes = clash_count(release, app);
    if clashes > 0 {
        parts.push(format!("{clashes} file conflict(s). Press c to inspect."));
    }
    if let Some(duplicate) = duplicate_reason(release, &app.mods) {
        parts.push(duplicate);
    }
    if parts.is_empty() {
        format!("{}: OK", release.name)
    } else {
        format!("{}: {}", release.name, parts.join("  //  "))
    }
}

fn duplicate_reason(release: &ModRelease, mods: &[ModRelease]) -> Option<String> {
    let other = mods
        .iter()
        .find(|candidate| candidate.id != release.id && same_logical_mod(release, candidate))?;
    Some(format!("duplicate of {}", other.name))
}

fn same_logical_mod(left: &ModRelease, right: &ModRelease) -> bool {
    if left.archive_sha256 == right.archive_sha256 {
        return true;
    }
    let catalog_id = |release: &ModRelease| {
        catalog::FRAMEWORKS.iter().find_map(|framework| {
            (release.id == framework.id || release.name.eq_ignore_ascii_case(framework.name))
                .then_some(framework.id)
        })
    };
    match (catalog_id(left), catalog_id(right)) {
        (Some(left_id), Some(right_id)) => left_id == right_id,
        _ => left.name.eq_ignore_ascii_case(&right.name) && left.version == right.version,
    }
}

fn warn_count(app: &App) -> usize {
    let mut count = app.health_checks.iter().filter(|check| !check.ok).count();
    if !app.profiles.is_empty() && !selected_profile_is_prepared(app) {
        count += 1;
    }
    if blocked_count(app) > 0 {
        count += 1;
    }
    count
}

fn page_badge(mode: Mode, app: &App) -> Option<String> {
    match mode {
        Mode::Loadout => {
            let count = blocked_count(app) + app.conflicts.len();
            (count > 0).then(|| count.to_string())
        }
        Mode::Add => None,
        Mode::Runtime => {
            if app.profiles.is_empty() || selected_profile_is_prepared(app) {
                None
            } else {
                Some("prep".into())
            }
        }
    }
}

fn next_action(app: &App, games: &[GameInstall]) -> String {
    if games.is_empty() {
        return "Install Cyberpunk 2077 in a discoverable Steam library.".into();
    }
    if let Some(check) = app.health_checks.iter().find(|check| !check.ok) {
        return check
            .remedy
            .clone()
            .unwrap_or_else(|| format!("Resolve {}.", check.name));
    }
    if core_installed_count(app) == 0 {
        return "Press 2, then f to fetch Core frameworks.".into();
    }
    if app.mods.is_empty() {
        return format!(
            "Press 2 to import from Add, or a on that mode to import a path. Inbox scans {}.",
            app.import_root.display()
        );
    }
    if !selected_profile_is_prepared(app) {
        return "Press p to prepare the isolated runtime.".into();
    }
    if blocked_count(app) > 0 {
        return "Blocked mods are in the loadout table. Space toggles, 2 opens Add for Core."
            .into();
    }
    if !app.conflicts.is_empty() {
        return "Press c to inspect file conflicts. C shows all collisions.".into();
    }
    "Shift-L launches the selected profile.".into()
}

fn sidebar_width(total_width: u16) -> u16 {
    if total_width < 100 { 16 } else { 22 }
}

fn render(frame: &mut Frame<'_>, app: &App, games: &[GameInstall]) {
    let theme = &app.theme;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());
    frame.render_widget(
        Block::default()
            .title(" CP2077 // MOD CONTROL ")
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme.primary.into())),
        vertical[0],
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width(vertical[1].width)),
            Constraint::Min(20),
        ])
        .split(vertical[1]);

    render_sidebar(frame, columns[0], app);
    match app.mode {
        Mode::Loadout => render_loadout(frame, columns[1], app, games),
        Mode::Add => render_add(frame, columns[1], app),
        Mode::Runtime => render_runtime(frame, columns[1], app),
    }
    let profile = app
        .profiles
        .get(app.profile_index)
        .map(|profile| profile.name.as_str())
        .unwrap_or("none");
    frame.render_widget(
        Paragraph::new(app.status.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" STATUS // {profile} ")),
            )
            .style(Style::default().fg(theme.accent.into())),
        vertical[2],
    );
    if let Some(input) = &app.import_path_input {
        render_import_path_prompt(frame, app, input);
    } else if let Some(input) = &app.profile_name_input {
        render_profile_name_prompt(frame, app, input);
    } else if let Some(action) = &app.pending_action {
        render_confirmation(frame, app, action);
    } else if app.help_open {
        render_help(frame, app);
    }
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let border = if app.pane == Pane::Sidebar {
        app.theme.accent
    } else {
        app.theme.primary
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border.into()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(7),
            Constraint::Length(3),
        ])
        .split(inner);

    let profile = app.profiles.get(app.profile_index);
    let header = vec![
        Line::from(Span::styled(
            format!(
                "● {}",
                profile
                    .map(|profile| profile.name.as_str())
                    .unwrap_or("none")
            ),
            Style::default()
                .fg(app.theme.accent.into())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    frame.render_widget(Paragraph::new(header), chunks[0]);

    let compact = chunks[1].width < 16;
    let nav = Mode::ALL
        .iter()
        .map(|mode| sidebar_nav_line(*mode, app, compact, chunks[1].width))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(nav), chunks[1]);

    let footer = if chunks[2].width < 16 {
        vec![
            Line::from(Span::styled(
                app.theme.name.as_str(),
                Style::default().fg(app.theme.accent.into()),
            )),
            Line::from("t theme"),
            Line::from("? help  L"),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                app.theme.name.as_str(),
                Style::default().fg(app.theme.accent.into()),
            )),
            Line::from("t theme  ? help"),
            Line::from("L launch"),
        ]
    };
    frame.render_widget(Paragraph::new(footer), chunks[2]);
}

fn sidebar_nav_line(mode: Mode, app: &App, compact: bool, width: u16) -> Line<'static> {
    let selected = app.mode == mode;
    let marker = if selected && app.pane == Pane::Sidebar {
        "▶"
    } else {
        " "
    };
    let name = if compact {
        mode.short_title()
    } else {
        mode.title()
    };
    let left = format!("{marker}{} {name}", mode.number());
    let style = if selected {
        Style::default()
            .fg(app.theme.accent.into())
            .add_modifier(Modifier::BOLD)
    } else {
        let index = mode.number().saturating_sub(1);
        Style::default().fg(app.theme.gradient(index, (Mode::ALL.len() - 1) as u8))
    };
    if let Some(badge) = page_badge(mode, app) {
        let gap = (width as usize)
            .saturating_sub(left.chars().count() + badge.chars().count())
            .max(1);
        Line::from(vec![
            Span::styled(left, style),
            Span::raw(" ".repeat(gap)),
            Span::styled(
                badge,
                Style::default()
                    .fg(app.theme.primary.into())
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(Span::styled(left, style))
    }
}

fn render_help(frame: &mut Frame<'_>, app: &App) {
    let outer = frame.area();
    let area = centered(outer, 78, 20);
    let lines = vec![
        Line::from("1 Loadout   2 Add   3 Runtime   q quit"),
        Line::from("h/l or arrows: sidebar ↔ list ↔ extra pane   j/k move in focus"),
        Line::from("p prepare   Shift-L launch   x/v vanilla loadout"),
        Line::from("Loadout: Space toggle (y fixes deps/dups)   +/- priority   d delete"),
        Line::from("c file conflicts   C all vs selected mod"),
        Line::from(
            "ISSUE flags: dep missing/disabled dependency   clash file conflict   dup duplicate",
        ),
        Line::from("Add Core: f fetch/enable   d delete   (r replaces if already installed)"),
        Line::from("Add Inbox: Enter/i import   a other path   r rescan"),
        Line::from("Runtime: Enter select   n new   s share   u private   i/e Steam"),
        Line::from("Runtime backups: b create   Enter restore"),
        Line::from("t cycle theme   ? or Esc close help"),
        Line::from(""),
        Line::from(format!("Active theme: {}", app.theme.name)),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" KEYS ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.accent.into())),
        ),
        area,
    );
}

fn centered(outer: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(outer.width.saturating_sub(2)).max(1);
    let height = height.min(outer.height.saturating_sub(2)).max(1);
    Rect::new(
        outer.x + outer.width.saturating_sub(width) / 2,
        outer.y + outer.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn render_loadout(frame: &mut Frame<'_>, area: Rect, app: &App, games: &[GameInstall]) {
    let warns = warn_count(app);
    let header_lines = vec![
        Line::from(format!(
            "LOADOUT  {} active / {} blocked / {} conflicts   CORE {}/{}   WARN {warns}",
            enabled_count(app),
            blocked_count(app),
            app.conflicts.len(),
            core_installed_count(app),
            catalog::FRAMEWORKS.len()
        )),
        Line::from(Span::styled(
            next_action(app, games),
            Style::default().fg(app.theme.accent.into()),
        )),
        Line::from(
            "Space toggle • +/- priority • d delete • c conflicts • C all clashes • p prepare • x vanilla • Shift-L launch",
        ),
    ];
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(6)])
        .split(area);
    frame.render_widget(
        Paragraph::new(header_lines)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .title(" LOADOUT // HEALTH ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if app.pane == Pane::Primary {
                        app.theme.accent.into()
                    } else {
                        app.theme.primary.into()
                    })),
            ),
        chunks[0],
    );
    let body = chunks[1];
    if app.pane == Pane::Secondary {
        render_conflicts(frame, body, app);
        return;
    }
    let table_and_detail = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(4)])
        .split(body);
    render_mods(frame, table_and_detail[0], app);
    frame.render_widget(
        Paragraph::new(selected_mod_detail(app))
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(app.theme.accent.into()))
            .block(
                Block::default()
                    .title(" SELECTED ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.theme.primary.into())),
            ),
        table_and_detail[1],
    );
}

fn render_add(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let core_height = 12.min(area.height.saturating_sub(8)).max(8);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(core_height), Constraint::Min(6)])
        .split(area);
    render_frameworks(frame, chunks[0], app);
    render_imports(frame, chunks[1], app);
}

fn render_runtime(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width >= 70 {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(area);
        render_profiles(frame, panes[0], app);
        render_runtime_detail(frame, panes[1], app);
    } else if app.pane == Pane::Secondary {
        render_runtime_detail(frame, area, app);
    } else {
        render_profiles(frame, area, app);
    }
}

fn render_runtime_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let profile = app.profiles.get(app.profile_cursor);
    let info = if let Some(profile) = profile {
        let prepared = app.prepared_profiles.contains(&profile.id);
        vec![
            Line::from(format!(
                "{}  [{}]",
                profile.name,
                if prepared { "PREPARED" } else { "NEEDS PREP" }
            )),
            Line::from(format!(
                "saves={}  runner={}  build={}",
                app.save_sets
                    .get(&profile.id)
                    .map(|save_set| save_set.name.as_str())
                    .unwrap_or("private"),
                profile.runner,
                profile.game_build_id
            )),
            Line::from("Enter select • n new • s share • u private • i/e Steam • p prepare"),
            Line::from("b create backup • Enter on list restores"),
        ]
    } else {
        vec![Line::from("No profile selected. Press n to create one.")]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(5)])
        .split(area);
    frame.render_widget(
        Paragraph::new(info).wrap(Wrap { trim: true }).block(
            Block::default()
                .title(" PROFILE DETAIL ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.primary.into())),
        ),
        chunks[0],
    );
    render_backups(frame, chunks[1], app);
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
    let mut state = ratatui::widgets::ListState::default().with_selected(Some(app.profile_cursor));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" PROFILES // ENTER SELECT // S SHARE // U PRIVATE // I IMPORT // E EXPORT // N NEW // P PREP ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if app.pane == Pane::Primary {
                        app.theme.accent.into()
                    } else {
                        app.theme.primary.into()
                    })),
            )
            .highlight_style(
                Style::default()
                    .bg(app.theme.surface.into())
                    .fg(app.theme.accent.into()),
            )
            .highlight_symbol(list_highlight(app, Pane::Primary)),
        area,
        &mut state,
    );
}

fn render_mods(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.mods.is_empty() {
        render_empty(
            frame,
            area,
            app,
            " MOD LOADOUT ",
            vec![
                Line::from("No mods are in this profile yet."),
                Line::from("Press 2, then f to fetch Core frameworks."),
                Line::from("Press 2 and Tab to import local archives from Inbox."),
            ],
        );
        return;
    }
    let state = app
        .resolved
        .iter()
        .map(|entry| (entry.entry.mod_id.as_str(), entry))
        .collect::<std::collections::BTreeMap<_, _>>();
    let rows = app.mods.iter().map(|release| {
        let status = match state.get(release.id.as_str()) {
            Some(entry) if entry.effective_enabled => "[ON]",
            Some(entry) if entry.entry.requested_enabled => "[--]",
            _ => "[  ]",
        };
        Row::new(vec![
            Cell::from(status),
            Cell::from(issue_flags(release, app)),
            Cell::from(release.name.clone()),
            Cell::from(release.version.clone()),
            Cell::from(release.kind.as_str()),
            Cell::from(
                state
                    .get(release.id.as_str())
                    .map(|entry| entry.entry.priority.to_string())
                    .unwrap_or_default(),
            ),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(13),
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(["STATE", "ISSUE", "MOD", "VERSION", "TYPE", "PRIORITY"]).style(
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
    .highlight_symbol(list_highlight(app, Pane::Primary))
    .block(
        Block::default()
            .title(format!(
                " MODS // {} CLASHES // SPACE TOGGLE // D DELETE // C CONFLICTS ",
                app.conflicts.len()
            ))
            .border_style(Style::default().fg(if app.pane == Pane::Primary {
                app.theme.accent.into()
            } else {
                app.theme.primary.into()
            }))
            .borders(Borders::ALL),
    );
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(app.loadout_index));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_imports(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.import_candidates.is_empty() {
        render_empty(
            frame,
            area,
            app,
            &format!(" INBOX {} ", app.import_root.display()),
            vec![
                Line::from("No archives or directories in the inbox."),
                Line::from("Drop a zip, 7z, rar, tar, or extracted mod folder there."),
                Line::from("Press a to import a path from anywhere, or r to rescan."),
                Line::from("Press f in the Core pane above to fetch frameworks first."),
            ],
        );
        return;
    }
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
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(app.inbox_index));
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
        .highlight_symbol(list_highlight(app, Pane::Secondary))
        .block(
            Block::default()
                .title(format!(
                    " INBOX {} // ENTER/I IMPORT // A OTHER PATH // R RESCAN ",
                    app.import_root.display()
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if app.pane == Pane::Secondary {
                    app.theme.accent.into()
                } else {
                    app.theme.primary.into()
                })),
        ),
        area,
        &mut state,
    );
}

fn render_empty(frame: &mut Frame<'_>, area: Rect, app: &App, title: &str, lines: Vec<Line<'_>>) {
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(app.theme.accent.into()))
            .block(Block::default().title(title).borders(Borders::ALL)),
        area,
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
        PendingAction::FrameworkPresent {
            framework_id,
            version,
            copies,
            ..
        } => {
            let name = catalog::FRAMEWORKS
                .iter()
                .find(|item| item.id == *framework_id)
                .map(|item| item.name)
                .unwrap_or(framework_id.as_str());
            format!(
                "{name} {version} is already in the library ({copies} cop{}).\n\nPress y to enable the existing copy in this profile, r to fetch the latest release and replace, or n to cancel.",
                if *copies == 1 { "y" } else { "ies" }
            )
        }
        PendingAction::DeleteMods { label, used_by, .. } => {
            let users = if used_by.is_empty() {
                "It is not in any profile loadout.".into()
            } else {
                format!("Used by: {}.", used_by.join(", "))
            };
            format!(
                "Delete {label} from the library?\n\n{users} Loadout entries are removed from every profile. The vanilla Steam tree is not touched."
            )
        }
        PendingAction::EnableMod {
            release_name,
            enable_deps,
            missing_deps,
            disable_dups,
            ..
        } => {
            let mut lines = vec![format!("Enable {release_name}?")];
            if !enable_deps.is_empty() {
                lines.push(format!(
                    "y also enables: {}.",
                    enable_deps
                        .iter()
                        .map(|(_, name)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !missing_deps.is_empty() {
                lines.push(format!(
                    "Not in the library (fetch from Add, then f): {}.",
                    missing_deps.join(", ")
                ));
            }
            if !disable_dups.is_empty() {
                lines.push(format!(
                    "y disables duplicate copy: {}.",
                    disable_dups
                        .iter()
                        .map(|(_, name)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            lines.push(
                "n enables only this row. Esc cancels.".into(),
            );
            lines.join("\n")
        }
    };
    let title = match action {
        PendingAction::FrameworkPresent { .. } => {
            " CONFIRM // Y ENABLE // R REPLACE // N OR ESC CANCELS "
        }
        PendingAction::EnableMod { .. } => " CONFIRM // Y FIX // N ENABLE ONLY // ESC CANCELS ",
        _ => " CONFIRM // Y YES // N OR ESC CANCELS ",
    };
    let outer = frame.area();
    let width = outer.width.saturating_sub(4).clamp(1, 86);
    let height = 10.min(outer.height);
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
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.primary.into())),
        ),
        area,
    );
}

fn render_conflicts(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let visible = visible_conflicts(app);
    if visible.is_empty() {
        let scope = if app.show_all_conflicts {
            "No exact-path collisions in this profile."
        } else {
            "No collisions for the highlighted mod. Press c to show all."
        };
        render_empty(
            frame,
            area,
            app,
            " FILE CONFLICTS ",
            vec![
                Line::from(scope),
                Line::from("When two mods ship the same game file, they appear here."),
                Line::from(
                    "Higher priority on the mods table wins. C toggles all vs selected. c returns.",
                ),
            ],
        );
        return;
    }
    let rows = visible.iter().map(|(path, owners)| {
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
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(app.conflict_index));
    let title = if app.show_all_conflicts {
        format!(
            " FILE CONFLICTS // {} ALL // SHIFT-C FILTER // C BACK ",
            visible.len()
        )
    } else {
        format!(
            " FILE CONFLICTS // {} FOR MOD // SHIFT-C ALL // C BACK ",
            visible.len()
        )
    };
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
        .highlight_symbol(list_highlight(app, Pane::Secondary))
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if app.pane == Pane::Secondary {
                    app.theme.accent.into()
                } else {
                    app.theme.primary.into()
                })),
        ),
        area,
        &mut state,
    );
}

fn render_backups(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.backups.is_empty() {
        render_empty(
            frame,
            area,
            app,
            " PROFILE BACKUPS // B CREATE // ENTER RESTORE ",
            vec![
                Line::from("No backups for the selected profile."),
                Line::from("Press b to snapshot isolated saves and runtime state."),
                Line::from("Restores never touch the vanilla Steam tree."),
            ],
        );
        return;
    }
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
    let mut state = ratatui::widgets::ListState::default().with_selected(Some(app.backup_index));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" PROFILE BACKUPS // B CREATE // ENTER RESTORE ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if app.pane == Pane::Secondary {
                        app.theme.accent.into()
                    } else {
                        app.theme.primary.into()
                    })),
            )
            .highlight_style(
                Style::default()
                    .bg(app.theme.surface.into())
                    .fg(app.theme.accent.into()),
            )
            .highlight_symbol(list_highlight(app, Pane::Secondary)),
        area,
        &mut state,
    );
}

#[allow(dead_code)]
fn render_checks(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut rows = app
        .health_checks
        .iter()
        .map(|check| {
            Row::new(vec![
                if check.ok {
                    "OK".to_string()
                } else {
                    "WARN".to_string()
                },
                check.name.clone(),
                check.detail.clone(),
                check.remedy.clone().unwrap_or_default(),
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
            "Press p on Loadout or Runtime".to_string()
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
            "Inspect Mods (3) and Core (6)".to_string()
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
            "Review Conflicts (4); priority determines the winner".to_string()
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
        .block(Block::default().title(" CHECKS ").borders(Borders::ALL)),
        area,
    );
}

fn render_frameworks(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = catalog::FRAMEWORKS.iter().map(|framework| {
        let installed = catalog::is_installed(&app.mods, framework);
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
    let mut state = ratatui::widgets::TableState::default().with_selected(Some(app.core_index));
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
        .highlight_symbol(list_highlight(app, Pane::Primary))
        .block(
            Block::default()
                .title(format!(
                    " CORE FRAMEWORKS // {} / {} INSTALLED // F FETCH // D DELETE ",
                    core_installed_count(app),
                    catalog::FRAMEWORKS.len()
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if app.pane == Pane::Primary {
                    app.theme.accent.into()
                } else {
                    app.theme.primary.into()
                })),
        ),
        area,
        &mut state,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn test_app() -> App {
        App {
            mode: Mode::Loadout,
            pane: Pane::Primary,
            loadout_index: 0,
            conflict_index: 0,
            show_all_conflicts: false,
            core_index: 0,
            inbox_index: 0,
            profile_index: 0,
            profile_cursor: 0,
            backup_index: 0,
            status: "Ready. Press ? for keys.".into(),
            mods: Vec::new(),
            profiles: Vec::new(),
            resolved: Vec::new(),
            import_root: PathBuf::from("/tmp/mods"),
            import_candidates: Vec::new(),
            import_path_input: None,
            profile_name_input: None,
            pending_action: None,
            prepared_profiles: BTreeSet::new(),
            save_sets: BTreeMap::new(),
            backups: Vec::new(),
            conflicts: vec![(PathBuf::from("archive/mod.archive"), vec!["a".into()])],
            health_checks: vec![doctor::Check {
                name: "Steam".into(),
                ok: false,
                detail: "missing".into(),
                remedy: Some("Install native Steam.".into()),
            }],
            help_open: false,
            theme: presets().remove(0),
        }
    }

    fn sample_game() -> GameInstall {
        GameInstall {
            id: "game".into(),
            library: PathBuf::from("/games"),
            root: PathBuf::from("/games/Cyberpunk 2077"),
            manifest: PathBuf::from("/games/appmanifest"),
            build_id: "20770477".into(),
            phantom_liberty: true,
            redmod: true,
            writable: true,
        }
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn three_named_modes() {
        assert_eq!(Mode::ALL.len(), 3);
        assert_eq!(Mode::Loadout.number(), 1);
        assert_eq!(Mode::Add.number(), 2);
        assert_eq!(Mode::Runtime.number(), 3);
        assert_eq!(Mode::Loadout.title(), "Loadout");
        assert_eq!(Mode::Add.title(), "Add");
        assert_eq!(Mode::Runtime.title(), "Runtime");
    }

    #[test]
    fn next_action_follows_launch_gate_order() {
        let mut app = test_app();
        assert_eq!(
            next_action(&app, &[]),
            "Install Cyberpunk 2077 in a discoverable Steam library."
        );
        let games = [sample_game()];
        assert_eq!(next_action(&app, &games), "Install native Steam.");
        app.health_checks[0].ok = true;
        assert_eq!(
            next_action(&app, &games),
            "Press 2, then f to fetch Core frameworks."
        );
    }

    #[test]
    fn badges_report_warns_and_conflicts() {
        let app = test_app();
        assert_eq!(page_badge(Mode::Loadout, &app).as_deref(), Some("1"));
        assert_eq!(page_badge(Mode::Add, &app), None);
        assert_eq!(page_badge(Mode::Runtime, &app), None);
    }

    #[test]
    fn sidebar_uses_full_names_on_a_wide_terminal() {
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        let app = test_app();
        terminal.draw(|frame| render(frame, &app, &[])).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Loadout"));
        assert!(text.contains("Add"));
        assert!(text.contains("Runtime"));
        assert!(text.contains("Night City"));
        assert!(text.contains("CP2077"));
        assert!(text.contains("// MOD CONTROL"));
        assert!(text.contains("● none"));
        assert!(!text.contains("PROFILE"));
        assert!(!text.contains("1DASH"));
        assert!(!text.contains("THEME"));
        assert!(!text.contains("6CLASH"));
        assert!(text.contains("SELECTED"));
        assert!(!text.contains("GAME PATH"));
    }

    #[test]
    fn enable_plan_collects_disabled_deps_and_dups() {
        let target = crate::models::ModRelease {
            id: "ui".into(),
            name: "Native Settings UI".into(),
            version: "1".into(),
            kind: crate::models::ModKind::Legacy,
            archive_sha256: "1".into(),
            layer_path: PathBuf::from("/tmp/ui"),
            source: "/tmp/ui".into(),
            installed_at: chrono::Utc::now(),
        };
        let redscript = crate::models::ModRelease {
            id: "redscript".into(),
            name: "redscript".into(),
            version: "1".into(),
            kind: crate::models::ModKind::Framework,
            archive_sha256: "2".into(),
            layer_path: PathBuf::from("/tmp/rs"),
            source: "/tmp/rs".into(),
            installed_at: chrono::Utc::now(),
        };
        let copy = crate::models::ModRelease {
            id: "ui-copy".into(),
            name: "Native Settings UI".into(),
            version: "1".into(),
            kind: crate::models::ModKind::Legacy,
            archive_sha256: "1".into(),
            layer_path: PathBuf::from("/tmp/ui2"),
            source: "/tmp/ui2".into(),
            installed_at: chrono::Utc::now(),
        };
        let mods = vec![target.clone(), redscript, copy];
        let loadout = vec![LoadoutEntry {
            profile_id: "p".into(),
            mod_id: "ui-copy".into(),
            priority: 10,
            requested_enabled: true,
        }];
        let deps = vec![Dependency {
            mod_id: "ui".into(),
            requires_id: "redscript".into(),
            inferred: true,
        }];
        let plan = enable_plan(&target, &mods, &loadout, &deps);
        assert_eq!(plan.enable_deps[0].0, "redscript");
        assert!(plan.missing_deps.is_empty());
        assert_eq!(plan.disable_dups[0].0, "ui-copy");
        assert!(plan.needs_prompt());
    }

    #[test]
    fn enable_plan_names_missing_core() {
        let target = crate::models::ModRelease {
            id: "ui".into(),
            name: "Native Settings UI".into(),
            version: "1".into(),
            kind: crate::models::ModKind::Legacy,
            archive_sha256: "1".into(),
            layer_path: PathBuf::from("/tmp/ui"),
            source: "/tmp/ui".into(),
            installed_at: chrono::Utc::now(),
        };
        let plan = enable_plan(
            &target,
            std::slice::from_ref(&target),
            &[],
            &[Dependency {
                mod_id: "ui".into(),
                requires_id: "redscript".into(),
                inferred: true,
            }],
        );
        assert_eq!(plan.missing_deps, vec!["redscript"]);
        assert!(plan.enable_deps.is_empty());
    }

    #[test]
    fn issue_flags_mark_file_clashes() {
        let mut app = test_app();
        app.mods.push(crate::models::ModRelease {
            id: "a".into(),
            name: "Alpha".into(),
            version: "1".into(),
            kind: crate::models::ModKind::Legacy,
            archive_sha256: "x".into(),
            layer_path: PathBuf::from("/tmp"),
            source: "/tmp".into(),
            installed_at: chrono::Utc::now(),
        });
        assert_eq!(issue_flags(&app.mods[0], &app), "clash");
        assert!(selected_mod_detail(&app).contains("file conflict"));
    }

    #[test]
    fn pane_chain_is_sidebar_primary_secondary() {
        assert_eq!(focus_left(Pane::Secondary), Pane::Primary);
        assert_eq!(focus_left(Pane::Primary), Pane::Sidebar);
        assert_eq!(focus_left(Pane::Sidebar), Pane::Sidebar);
        assert_eq!(focus_right(Pane::Sidebar), Pane::Primary);
        assert_eq!(focus_right(Pane::Primary), Pane::Secondary);
        assert_eq!(focus_right(Pane::Secondary), Pane::Secondary);
    }

    #[test]
    fn help_overlay_lists_three_modes() {
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        let mut app = test_app();
        app.help_open = true;
        terminal.draw(|frame| render(frame, &app, &[])).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("1 Loadout"));
        assert!(text.contains("ISSUE flags"));
        assert!(text.contains("t cycle theme"));
    }
}
