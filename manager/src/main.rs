#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{bail, Context, Result};
use reshade_manager::{
    artwork,
    catalog::{self, Cache, Package, Selection},
    discovery::{self, Game},
    install::{self, Operation, Plan},
    pe::{self, Api, Target},
    storage::{self, Settings},
};
use slint::{ComponentHandle, Image, ModelRc, VecModel};
use std::{
    cell::RefCell, collections::BTreeSet, fs, path::{Path, PathBuf},
    rc::Rc, sync::mpsc, thread, time::Duration,
};

slint::include_modules!();

const CATEGORIES: [&str; 10] = [
    "All categories", "Lighting & ambient occlusion", "Color correction & grading",
    "HDR & tone mapping", "Sharpening", "Anti-aliasing", "Depth effects", "CRT & retro",
    "Film & post-processing", "Utility & miscellaneous",
];

struct ViewGame {
    game: Game,
    summary: install::Status,
    artwork: Option<Image>,
}

struct Session {
    key: String,
    target: Result<Target>,
    status: install::Status,
    operation: Operation,
    api: Option<Api>,
    installed_api: Option<Api>,
    addon: bool,
    acknowledged: bool,
    shaders: Vec<Package>,
    addons: Vec<Package>,
    selection: Selection,
    expanded: BTreeSet<String>,
    query: String,
    category: usize,
    had_selection: bool,
    manages_content: bool,
    runtime_only: bool,
}

struct State {
    data: PathBuf,
    settings: Settings,
    games: Vec<ViewGame>,
    query: String,
    filter: i32,
    session: Option<Session>,
    events: mpsc::Sender<Event>,
    artwork_generation: u64,
    checking_compatibility: bool,
}

enum Event {
    Scanned(Vec<(Game, install::Status)>),
    Compatibility(Result<()>),
    Artwork(u64, String, PathBuf),
    Catalog(Result<(Vec<Package>, Vec<Package>, String)>),
    Stage(String),
    Finished(Result<()>),
    Cache(Result<()>),
    Legacy(Result<()>),
    CacheUpdates(Result<String>),
}

type SharedState = Rc<RefCell<State>>;

fn rows<T: 'static + Clone>(items: Vec<T>) -> ModelRc<T> { ModelRc::new(VecModel::from(items)) }

fn status_text(status: &install::Status) -> String {
    if status.conflict { return "Wrapper conflict".into(); }
    if status.missing_game { return "Game file missing".into(); }
    if status.needs_repair { return "Repair needed".into(); }
    if status.wrapper.is_some() || status.vulkan {
        return status.version.as_ref().map_or_else(|| "Installed".into(), |version| format!("Installed · {version}"));
    }
    if status.configuration { "Configuration found".into() } else { "Not installed".into() }
}

fn game_row(entry: &ViewGame) -> GameRow {
    GameRow {
        key: entry.game.key().into(), title: entry.game.title.clone().into(),
        source: entry.game.source.label().into(), status: status_text(&entry.summary).into(),
        installed: entry.summary.wrapper.is_some() || entry.summary.vulkan,
        art: entry.artwork.clone().unwrap_or_default(),
        selected: false,
    }
}

fn render_games(ui: &ManagerWindow, state: &State) {
    let selected = ui.get_game().key;
    ui.set_library_count(state.games.len() as i32);
    ui.set_games(rows(state.games.iter().filter(|entry| {
        let matches = entry.game.title.to_lowercase().contains(&state.query) || entry.game.source.label().to_lowercase().contains(&state.query);
        let installed = entry.summary.wrapper.is_some() || entry.summary.vulkan;
        matches && (state.filter == 0 || (state.filter == 1 && installed) || (state.filter == 2 && !installed))
    }).map(|entry| {
        let mut row = game_row(entry);
        row.selected = row.key == selected;
        row
    }).collect()));
}

fn cache(state: &State) -> Cache {
    Cache { root: state.settings.package_directory.clone().unwrap_or_else(|| state.data.join("download-cache")) }
}

fn render_settings(ui: &ManagerWindow, state: &State) {
    let settings = &state.settings;
    ui.set_directories(rows(settings.scan_directories.iter().map(|p| p.to_string_lossy().as_ref().into()).collect()));
    ui.set_scan_on_launch(settings.scan_on_launch);
    ui.global::<Appearance>().set_high_contrast(settings.high_contrast);
    ui.set_classic(settings.classic);
    ui.set_shared(settings.shared_shaders);
    ui.set_backups(settings.keep_backups);
    ui.set_package_limit(settings.package_limit_mb.to_string().into());
    ui.set_artwork_limit(settings.artwork_limit_mb.to_string().into());
    let cache = cache(state);
    let (count, size) = cache.stats();
    ui.set_cache_info(format!("Downloads · {count} packages · {:.1} MB\n{}\nShared live storage · {}\nArtwork · {}",
        size as f64 / 1_048_576.0, cache.root.display(), state.data.join("shared-shaders").display(), state.data.join("artwork").display()).into());
}

fn diagnostics(data: &Path, line: &str) {
    use std::io::Write;
    let _ = fs::create_dir_all(data);
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(data.join("manager.log")) {
        let _ = writeln!(file, "{line}");
    }
}

fn save(ui: &ManagerWindow, state: &State) {
    if let Err(error) = storage::write_json(&state.data.join("settings.json"), &state.settings) {
        ui.set_stage(format!("Could not save settings · {error:#}").into());
        diagnostics(&state.data, &format!("{error:#}"));
    }
    render_settings(ui, state);
}

fn compatibility(data: &Path) -> storage::Ini {
    storage::parse_ini(&fs::read_to_string(data.join("metadata/Compatibility.ini")).unwrap_or_default())
}

fn inspect(game: &Game, metadata: &storage::Ini) -> install::Status {
    if !game.executable.is_file() {
        return install::Status { missing_game: true, ..install::Status::default() };
    }
    let Some(root) = game.executable.parent() else { return install::Status::default(); };
    let key = game.executable.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    let base = metadata.get(&key).and_then(|v| v.get("installtarget"))
        .and_then(|path| storage::safe_child(root, Path::new(path)).ok());
    install::status(&game.executable, base.as_deref().unwrap_or(root))
}

fn scan(ui: &ManagerWindow, state: &mut State) {
    if ui.get_busy() || ui.get_scanning() { return; }
    ui.set_scanning(true);
    let events = state.events.clone();
    let settings = state.settings.clone();
    let data = state.data.clone();
    let refresh_compatibility = !state.checking_compatibility;
    if refresh_compatibility {
        state.checking_compatibility = true;
        ui.set_checking_compatibility(true);
    }
    thread::spawn(move || {
        let metadata = compatibility(&data);
        let games = discovery::discover(&discovery::system_locations(), &settings)
            .into_iter().map(|game| {
                let summary = inspect(&game, &metadata);
                (game, summary)
            }).collect();
        let _ = events.send(Event::Scanned(games));
        if refresh_compatibility {
            let result = catalog::client().and_then(|client| {
                catalog::metadata(&client, &data.join("metadata"), "Compatibility.ini").map(|_| ())
            });
            let _ = events.send(Event::Compatibility(result));
        }
    });
}

fn same_executable(a: &Path, b: &Path) -> bool {
    let a = a.canonicalize().unwrap_or_else(|_| a.to_owned());
    let b = b.canonicalize().unwrap_or_else(|_| b.to_owned());
    install::same_path(&a, &b)
}

fn artwork_jobs(state: &mut State, only: Option<(String, bool)>) {
    state.artwork_generation = state.artwork_generation.wrapping_add(1);
    let generation = state.artwork_generation;
    let games = state.games.iter().filter(|entry| {
        only.as_ref().map_or(true, |(key, _)| entry.game.key() == *key)
    }).map(|entry| entry.game.clone()).collect::<Vec<_>>();
    let directory = state.data.join("artwork");
    let settings = state.settings.clone();
    let refresh = only.is_some_and(|(_, refresh)| refresh);
    // Four independent workers bound network and decode contention.
    for offset in 0..games.len().min(4) {
        let work = games.iter().skip(offset).step_by(4).cloned().collect::<Vec<_>>();
        let events = state.events.clone();
        let directory = directory.clone();
        let custom = settings.artwork.clone();
        let limit = settings.artwork_limit_mb.saturating_mul(1_048_576);
        thread::spawn(move || {
            for game in work {
                let key = game.key();
                if let Ok(path) = artwork::load(&game, custom.get(&key).map(PathBuf::as_path), &directory, limit, refresh) {
                    let _ = events.send(Event::Artwork(generation, key, path));
                }
            }
        });
    }
}

fn receipt_for_entry(target: &Result<Target>) -> Option<install::Receipt> {
    let receipt = target.as_ref().ok().and_then(|target| install::receipt(&target.base).ok())?;
    receipt.game.is_some().then_some(receipt)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GameAction {
    Install, Import, Repair, Update, Shaders, Legacy, CheckCompatibility, Unavailable,
}

fn next_action(target: Option<&Target>, status: &install::Status, receipt: &install::Receipt, compatible: bool) -> GameAction {
    use GameAction::*;
    let Some(target) = target else { return Unavailable; };
    if target.banned || status.conflict || status.receipt_conflict { return Unavailable; }
    if !compatible { return CheckCompatibility; }
    if status.vulkan || (status.wrapper.is_none() && receipt.game.is_none() && target.api == Some(Api::Vulkan)) {
        return Legacy;
    }
    if status.wrapper.is_none() {
        return if receipt.game.is_some() { Repair } else { Install };
    }
    if receipt.game.is_none() { return Import; }
    if status.version.as_deref().is_some_and(install::is_older_than_distribution) {
        Update
    } else {
        Shaders
    }
}

fn details(ui: &ManagerWindow, state: &mut State, key: &str) {
    let Some(entry) = state.games.iter_mut().find(|entry| entry.game.key() == key) else { return; };
    let target = pe::analyze(&entry.game.executable, &compatibility(&state.data));
    let compatible = state.data.join("metadata/Compatibility.ini").is_file();
    let receipt = receipt_for_entry(&target).unwrap_or_default();
    let api = target.as_ref().ok().and_then(|t| t.api);
    let mut guidance = String::new();
    if let Ok(t) = &target {
        entry.summary = install::status(&t.executable, &t.base);
        if entry.summary.receipt_conflict {
            guidance = "This folder is managed for a different executable. Select that game or open the game folder to check the installation.".into();
        } else if entry.summary.conflict {
            guidance = "More than one ReShade wrapper was found. Open the game folder to inspect them before making changes.".into();
        } else if t.banned {
            guidance = "The compatibility catalog marks this game as blocked for ReShade.".into();
        } else if entry.summary.needs_repair {
            guidance = "The ReShade runtime is missing. Repair can restore it using the recorded settings.".into();
        }
        if !t.notes.is_empty() {
            if !guidance.is_empty() { guidance.push_str("\n"); }
            guidance.push_str(&t.notes.join("\n"));
        }
    } else if !entry.game.executable.is_file() {
        guidance = "The game executable may have moved or the drive may be disconnected. Reconnect the drive, or add the executable at its new location.".into();
    } else if let Err(error) = &target {
        guidance = format!("Could not read this executable. {error:#}");
    }
    let installed = entry.summary.wrapper.is_some() || entry.summary.vulkan || receipt.game.is_some();
    let action = next_action(target.as_ref().ok(), &entry.summary, &receipt, compatible);
    let (primary, message) = match action {
        GameAction::Install => ("Set up ReShade", ""),
        GameAction::Import => ("Import installation", "Confirm the edition once to manage this installation."),
        GameAction::Repair => ("Repair installation", ""),
        GameAction::Update => ("Review update", ""),
        GameAction::Shaders => ("Manage shaders", ""),
        GameAction::Legacy => ("Open official setup", "Vulkan setup uses the official ReShade installer."),
        GameAction::CheckCompatibility => ("Check compatibility", "Connect to the internet to download compatibility information."),
        GameAction::Unavailable => ("", ""),
    };
    if !message.is_empty() {
        if !guidance.is_empty() { guidance.push_str("\n"); }
        guidance.push_str(message);
    }
    let existing_api = entry.summary.wrapper.as_ref()
        .and_then(|path| Api::ALL.into_iter().find(|api| path.file_stem().is_some_and(|name| name.eq_ignore_ascii_case(api.name()))));
    let renderer = if entry.summary.vulkan { Some(Api::Vulkan) } else { existing_api.or(receipt.api).or(api) };
    let facts = target.as_ref().ok().map(|t| format!("{}  ·  {}", renderer.map(Api::label).unwrap_or("Renderer unknown"), t.arch.label())).unwrap_or_default();
    ui.set_game(game_row(entry));
    ui.set_details(guidance.into());
    ui.set_game_facts(facts.into());
    ui.set_game_path(entry.game.executable.to_string_lossy().as_ref().into());
    ui.set_primary_action(primary.into());
    ui.set_can_addon(matches!(action, GameAction::Install | GameAction::Legacy) && !installed);
    ui.set_can_maintain(installed && receipt.game.is_some() && !entry.summary.vulkan && !entry.summary.conflict && !entry.summary.receipt_conflict && compatible && target.as_ref().is_ok_and(|t| !t.banned));
    ui.set_can_remove(installed && !entry.summary.vulkan && !entry.summary.conflict && !entry.summary.receipt_conflict && compatible && target.is_ok());
    ui.set_compatibility_action(action == GameAction::CheckCompatibility);
    ui.set_can_official(target.as_ref().is_ok_and(|t| !t.banned) && compatible && !entry.summary.receipt_conflict);
    ui.set_can_forget(state.settings.executables.iter().any(|path| same_executable(path, &entry.game.executable)));
    ui.set_missing_game(entry.summary.missing_game || !entry.game.executable.is_file());
    ui.set_show_options(false);
    ui.set_import_picker(false);
    ui.set_stage("".into());
    ui.set_page(2);
    state.session = Some(Session {
        key: key.to_owned(), target, status: entry.summary.clone(), operation: Operation::Install,
        api: renderer, installed_api: existing_api.or(receipt.api), addon: receipt.addon, acknowledged: false,
        shaders: vec![], addons: vec![], selection: receipt.selection, expanded: BTreeSet::new(),
        query: String::new(), category: 0, had_selection: receipt.game.is_some(), manages_content: receipt.selection_managed, runtime_only: false,
    });
    render_games(ui, state);
}

fn review(ui: &ManagerWindow, state: &State) {
    let Some(session) = &state.session else { return; };
    let Ok(target) = &session.target else { return; };
    let Some(api) = session.api else { return; };
    let display = |packages: &[Package], selected: &dyn Fn(&Package) -> bool| {
        let cache = cache(state);
        let rows: Vec<_> = packages.iter().filter(|p| selected(p)).map(|p| {
            let source = cache.status(&p.url).map(|c| format!("cached · {}", c.version.chars().take(12).collect::<String>()))
                .unwrap_or_else(|| "download required".into());
            format!("  {} · {source}", p.name)
        }).collect();
        if rows.is_empty() { "  None selected for this operation".into() } else { rows.join("\n") }
    };
    let runtime = if session.operation == Operation::Uninstall {
        "Remove the verified ReShade runtime and unchanged files tracked by the manager. Keep presets, configuration and custom files.".into()
    } else if session.operation == Operation::Modify {
        "Keep the existing ReShade runtime.".into()
    } else {
        let url = install::runtime_url(session.addon);
        format!("{} ReShade {} · {}", if session.addon { "Full add-on support" } else { "Standard" }, install::RUNTIME_VERSION,
            if cache(state).contains(&url) { "cached" } else { "download required" })
    };
    let mut summary = format!("{}\n{} · {}\n\n{}\n\nDestination\n{}\n", ui.get_game().title, api.label(), target.arch.label(), runtime, target.base.display());
    if matches!(session.operation, Operation::Install | Operation::Modify) || (session.operation == Operation::Repair && session.manages_content) {
        summary.push_str(&format!("\nShader packages\n{}\n\nAdd-ons\n{}",
            display(&session.shaders, &|p| session.selection.shaders.contains_key(&p.id)),
            display(&session.addons, &|p| session.selection.addons.contains(&p.id))));
    }
    ui.set_can_edit_renderer(session.operation == Operation::Install && !target.forced);
    ui.set_can_edit_content(wants_content(session) && !session.runtime_only);
    ui.set_summary(summary.into());
    ui.set_stage("".into());
    ui.set_page(7);
}

fn render_catalog(ui: &ManagerWindow, state: &State) {
    let Some(session) = &state.session else { return; };
    let cache = cache(state);
    let mut result = Vec::new();
    let matches = |text: &str| text.to_lowercase().contains(&session.query);
    if ui.get_page() == 6 {
        for package in &session.addons {
            let category = catalog::addon_category(&package.name);
            if !(matches(&package.name) || matches(&package.description) || matches(&package.repository) || matches(category)) { continue; }
            let official = package.repository.starts_with("https://github.com/crosire/reshade/") && package.url.starts_with("https://github.com/crosire/");
            let source = if official { "Official example" } else { "Community · upstream curated" };
            let tag = package.url.split("/download/").nth(1).and_then(|s| s.split_once('/')).map(|(tag, _)| tag).unwrap_or("Latest release");
            let cached = if cache.contains(&package.url) { "Cached" } else { "Download required" };
            let arch = session.target.as_ref().map(|t| t.arch.label()).unwrap_or("Unknown");
            result.push(CatalogRow { key: package.id.clone().into(), title: package.name.clone().into(),
                subtitle: format!("{category} · {source} · {tag} · {arch} · {cached}\n{}\n{}", package.description, package.repository).into(),
                kind: 2, selected: session.selection.addons.contains(&package.id), required: false });
        }
        ui.set_selection_info(format!("{} add-ons selected", session.selection.addons.len()).into());
    } else {
        let selected_category = (!state.settings.classic && session.category > 0).then(|| CATEGORIES[session.category]);
        for package in &session.shaders {
            let visible_effects: Vec<_> = package.effects.iter().filter(|effect| {
                (matches(effect) || matches(&package.name) || matches(&package.description))
                    && selected_category.is_none_or(|category| category == catalog::category(effect))
            }).collect();
            if (visible_effects.is_empty() && !matches(&package.name) && !matches(&package.description)) ||
                (selected_category.is_some() && visible_effects.is_empty()) { continue; }
            let open = if !state.settings.classic || !session.query.is_empty() {
                !session.expanded.contains(&package.id)
            } else { session.expanded.contains(&package.id) };
            let count = package.effects.iter().filter(|effect| session.selection.effect_selected(package, effect)).count();
            let chosen = session.selection.shaders.get(&package.id).is_some_and(|choice| choice.all);
            let count = if package.required { format!("{count} required") } else if chosen { format!("Entire package · {count} listed") } else { format!("{count} of {} listed", package.effects.len()) };
            result.push(CatalogRow { key: package.id.clone().into(), title: package.name.clone().into(),
                subtitle: format!("{count} · {}\n{}", package.description, package.repository).into(),
                kind: 0, selected: open, required: package.required });
            if open {
                for effect in visible_effects {
                    result.push(CatalogRow { key: format!("{}|{effect}", package.id).into(), title: effect.as_str().into(),
                        subtitle: if state.settings.classic { "Effect".into() } else { format!("{} · No upstream description", catalog::category(effect)).into() },
                        kind: 1, selected: session.selection.effect_selected(package, effect), required: package.required });
                }
            }
        }
        ui.set_selection_info(format!("{} packages selected", session.selection.shaders.len()).into());
    }
    ui.set_catalog(rows(result));
}

fn wants_content(session: &Session) -> bool {
    matches!(session.operation, Operation::Install | Operation::Modify)
        || (session.operation == Operation::Repair && session.manages_content)
}

fn prepare_review(ui: &ManagerWindow, state: &State) {
    let Some(session) = &state.session else { return; };
    if session.api == Some(Api::Vulkan) || session.status.vulkan {
        legacy_setup(ui, state);
    } else if session.api.is_none() {
        ui.set_page(4);
    } else if wants_content(session) && !session.runtime_only && session.shaders.is_empty() {
        load_catalog(ui, state);
    } else {
        review(ui, state);
    }
}

fn load_catalog(ui: &ManagerWindow, state: &State) {
    let Some(session) = &state.session else { return; };
    let Ok(target) = &session.target else { return; };
    let arch = target.arch;
    let want_addons = session.addon;
    let location = state.data.join("metadata");
    let events = state.events.clone();
    ui.set_busy(true);
    ui.set_catalog_failed(false);
    ui.set_can_use_cached_runtime(false);
    ui.set_page(7);
    ui.set_stage("Loading shader packages…".into());
    thread::spawn(move || {
        let result = catalog::client().and_then(|client| {
            let effects = catalog::packages(catalog::metadata(&client, &location, "EffectPackages.ini")?, None);
            if effects.is_empty() { bail!("The shader catalog is empty."); }
            let (addons, note) = if want_addons {
                match catalog::metadata(&client, &location, "Addons.ini") {
                    Ok(ini) => (catalog::packages(ini, Some(arch)), String::new()),
                    Err(error) => (vec![], format!("Add-ons are unavailable. {error:#}")),
                }
            } else { (vec![], String::new()) };
            Ok((effects, addons, note))
        });
        let _ = events.send(Event::Catalog(result));
    });
}

fn show_content(ui: &ManagerWindow, state: &mut State, page: i32) {
    if let Some(session) = state.session.as_mut() {
        session.query.clear();
        session.category = 0;
        ui.set_selected_category(0);
        ui.set_page(page);
        render_catalog(ui, state);
    }
}

fn run_install(ui: &ManagerWindow, state: &State) {
    let Some(session) = &state.session else { return; };
    let Some(api) = session.api else { return; };
    let Ok(target) = &session.target else { return; };
    let Some(game) = state.games.iter().find(|entry| entry.game.key() == session.key) else {
        ui.set_stage("This game is no longer in the library. Rescan and select it again.".into());
        return;
    };
    let exe = game.game.executable.clone();
    let expected_executable = target.executable.clone();
    let expected_base = target.base.clone();
    let expected_arch = target.arch;
    let (addon, acknowledged, operation) = (session.addon, session.acknowledged, session.operation);
    let shaders = session.shaders.clone();
    let addons = session.addons.clone();
    let selection = session.selection.clone();
    let data = state.data.clone();
    let shared = state.settings.shared_shaders.then(|| data.join("shared-shaders"));
    let backups = state.settings.keep_backups.then(|| data.join("backups").join(storage::sha256(session.key.as_bytes())));
    let cache = cache(state);
    let events = state.events.clone();
    let limit = state.settings.package_limit_mb.saturating_mul(1_048_576);
    ui.set_busy(true);
    ui.set_page(8);
    ui.set_progress_phase(0);
    ui.set_result_failed(false);
    ui.set_stage("Preparing installation…".into());
    thread::spawn(move || {
        let result = pe::analyze(&exe, &compatibility(&data)).and_then(|target| {
            if target.executable != expected_executable || target.base != expected_base || target.arch != expected_arch {
                bail!("The installation target changed after review. Open the game again to check the new target.");
            }
            install::execute(&cache, Plan { target: &target, api, addon, acknowledged, operation, selection: &selection,
                shaders: &shaders, addons: &addons, shared: shared.as_deref(), backups: backups.as_deref() },
                |phase| { let _ = events.send(Event::Stage(phase.to_owned())); })
        });
        if result.is_ok() {
            if let Err(error) = cache.trim(limit) { diagnostics(&data, &format!("Cache cleanup · {error:#}")); }
        }
        let _ = events.send(Event::Finished(result));
    });
}

fn handle(ui: &ManagerWindow, state: &mut State, event: Event) {
    match event {
        Event::Scanned(games) => {
            let mut previous: std::collections::BTreeMap<_, _> = state.games.drain(..)
                .map(|entry| (entry.game.key(), entry.artwork)).collect();
            state.games = games.into_iter().map(|(game, summary)| {
                let artwork = previous.remove(&game.key()).flatten();
                ViewGame { game, summary, artwork }
            }).collect();
            let items = state.games.iter().map(|entry| &entry.game).collect::<Vec<_>>();
            if let Err(error) = storage::write_json(&state.data.join("library.json"), &items) {
                diagnostics(&state.data, &format!("{error:#}"));
            }
            ui.set_scanning(false);
            let key = ui.get_game().key;
            if ui.get_page() == 2 && !ui.get_busy() {
                if state.games.iter().any(|entry| entry.game.key() == key.as_str()) {
                    details(ui, state, key.as_str());
                } else {
                    ui.set_page(0);
                    ui.set_game(GameRow::default());
                    state.session = None;
                }
            }
            render_games(ui, state);
            artwork_jobs(state, None);
        }
        Event::Compatibility(result) => {
            state.checking_compatibility = false;
            ui.set_checking_compatibility(false);
            if ui.get_busy() || ui.get_page() != 2 { return; }
            if let Err(error) = &result { diagnostics(&state.data, &format!("{error:#}")); }
            let key = ui.get_game().key;
            if result.is_ok() {
                details(ui, state, key.as_str());
            } else if !state.data.join("metadata/Compatibility.ini").is_file() {
                ui.set_stage("Could not check compatibility. Check your connection and try again.".into());
            }
        }
        Event::Artwork(generation, key, path) if generation == state.artwork_generation => {
            if let Some(entry) = state.games.iter_mut().find(|entry| entry.game.key() == key) {
                entry.artwork = Image::load_from_path(&path).ok();
                if ui.get_game().key.as_str() == key { ui.set_game(game_row(entry)); }
            }
            render_games(ui, state);
        }
        Event::Artwork(..) => {}
        Event::Catalog(result) => {
            ui.set_busy(false);
            match result {
                Ok((shaders, addons, note)) => {
                    if let Some(session) = state.session.as_mut() {
                        if !session.had_selection && session.operation != Operation::Modify {
                            session.selection = Selection::defaults(&shaders);
                        }
                        for package in shaders.iter().filter(|p| p.required) { session.selection.set_all(package, true); }
                        session.shaders = shaders;
                        session.addons = addons;
                        session.expanded.clear();
                        review(ui, state);
                        ui.set_stage(note.into());
                    }
                }
                Err(error) => {
                    diagnostics(&state.data, &format!("{error:#}"));
                    ui.set_catalog_failed(true);
                    ui.set_can_use_cached_runtime(state.session.as_ref().is_some_and(|s|
                        s.operation == Operation::Install && !s.addon && !s.had_selection
                        && cache(state).contains(&install::runtime_url(false))
                    ));
                    ui.set_stage("Shader packages could not be loaded. Check your connection and retry.".into());
                }
            }
        }
        Event::Stage(stage) => {
            let phase = if stage.starts_with("Downloading") { 1 }
                else if stage.starts_with("Validating") || stage.starts_with("Extracting") { 2 }
                else if stage.starts_with("Configuring") { 3 }
                else if stage.starts_with("Installing") || stage.starts_with("Removing") { 4 }
                else if stage.starts_with("Final") { 5 } else { 0 };
            ui.set_progress_phase(ui.get_progress_phase().max(phase));
            ui.set_stage(stage.into());
        }
        Event::Finished(result) => {
            ui.set_busy(false);
            match result {
                Ok(()) => {
                    if let Some(Ok(target)) = state.session.as_ref().map(|s| &s.target) {
                        if let Some(entry) = state.games.iter_mut().find(|entry| entry.game.key() == ui.get_game().key.as_str()) {
                            entry.summary = install::status(&target.executable, &target.base);
                        }
                    }
                    ui.set_progress_phase(5);
                    ui.set_stage("Files verified. You can return to the game or open its folder.".into());
                    render_games(ui, state);
                }
                Err(error) => {
                    diagnostics(&state.data, &format!("{error:#}"));
                    ui.set_result_failed(true);
                    ui.set_stage(format!("{error:#}").into());
                }
            }
        }
        Event::Legacy(result) => {
            ui.set_busy(false);
            match result {
                Ok(()) => {
                    details(ui, state, ui.get_game().key.as_str());
                    ui.set_stage("Official setup closed. The game’s status has been checked.".into());
                }
                Err(error) => report(ui, state, Err(error)),
            }
        }
        Event::CacheUpdates(result) => {
            ui.set_busy(false);
            ui.set_stage(match result { Ok(message) => message.into(), Err(error) => format!("{error:#}").into() });
        }
        Event::Cache(result) => {
            ui.set_busy(false);
            ui.set_stage(match result { Ok(()) => "Cache updated.".into(), Err(e) => format!("Cache error · {e:#}").into() });
            render_settings(ui, state);
        }
    }
}

fn begin(ui: &ManagerWindow, state: &mut State, operation: &str, addon: bool) {
    let Some(session) = state.session.as_mut() else { return; };
    let Ok(target) = &session.target else { return; };
    session.operation = match operation {
        "Update" => Operation::Update, "Repair" => Operation::Repair, "Modify" => Operation::Modify,
        "Uninstall" => Operation::Uninstall, _ => Operation::Install,
    };
    if session.operation != Operation::Uninstall && target.banned { return; }
    if session.operation == Operation::Install {
        session.addon = addon;
        session.shaders.clear();
        session.addons.clear();
        session.selection = Selection::default();
        session.runtime_only = false;
    }
    session.api = if session.status.vulkan { Some(Api::Vulkan) } else { session.installed_api.or(target.api) };
    if target.forced && session.operation == Operation::Install { session.api = target.api; }
    session.acknowledged = false;
    ui.set_addon(session.addon && session.operation != Operation::Uninstall);
    ui.set_operation(operation.into());
    ui.set_renderer_handoff(session.api == Some(Api::Vulkan));
    ui.set_selected_api(session.api.and_then(|api| Api::ALL.iter().position(|a| *a == api)).map(|i| i as i32).unwrap_or(-1));
    ui.set_catalog_failed(false);
    ui.set_stage("".into());
    if session.addon && session.operation != Operation::Uninstall { ui.set_page(3); }
    else { prepare_review(ui, state); }
}

fn bind(ui: &ManagerWindow, state: &SharedState) {
    macro_rules! ui_callback {
        ($name:ident, |$win:ident, $app:ident $(, $arg:ident)*| $body:block) => {{
            let weak = ui.as_weak();
            let model = state.clone();
            ui.$name(move |$($arg),*| {
                if let Some($win) = weak.upgrade() {
                    let Ok(mut borrowed) = model.try_borrow_mut() else { return; };
                    let $app = &mut *borrowed;
                    $body
                }
            });
        }};
    }
    ui_callback!(on_navigate, |ui, state, page| {
        if page == 2 {
            let key = ui.get_game().key;
            if state.games.iter().any(|entry| entry.game.key() == key.as_str()) {
                details(&ui, state, key.as_str());
                return;
            }
            ui.set_game(GameRow::default());
            state.session = None;
            ui.set_page(0);
        } else {
            ui.set_page(page);
        }
        ui.set_stage("".into());
        if page == 1 { render_settings(&ui, state); } else { render_games(&ui, state); }
    });
    ui_callback!(on_search, |ui, state, query| { state.query = query.trim().to_lowercase(); ui.set_library_query(query); render_games(&ui, state); });
    ui_callback!(on_filter, |ui, state, filter| { state.filter = filter; ui.set_filter_index(filter); render_games(&ui, state); });
    ui_callback!(on_rescan, |ui, state| { scan(&ui, state); });
    ui_callback!(on_clear_filters, |ui, state| {
        state.query.clear();
        state.filter = 0;
        ui.set_library_query("".into());
        ui.set_filter_index(0);
        render_games(&ui, state);
    });
    ui_callback!(on_open_game, |ui, state, key| { details(&ui, state, key.as_str()); });
    ui_callback!(on_primary, |ui, state| {
        let Some(session) = &state.session else { return; };
        let receipt = receipt_for_entry(&session.target).unwrap_or_default();
        match next_action(session.target.as_ref().ok(), &session.status, &receipt, state.data.join("metadata/Compatibility.ini").is_file()) {
            GameAction::Install => begin(&ui, state, "Install", false),
            GameAction::Repair => begin(&ui, state, "Repair", false),
            GameAction::Update => begin(&ui, state, "Update", false),
            GameAction::Shaders => begin(&ui, state, "Modify", false),
            GameAction::Legacy => begin(&ui, state, "Install", false),
            GameAction::Import => ui.set_import_picker(!ui.get_import_picker()),
            GameAction::CheckCompatibility => scan(&ui, state),
            GameAction::Unavailable => {}
        }
    });
    ui_callback!(on_add_location, |ui, state| {
        if let Some(path) = pick_folder() {
            if path.parent().is_none() {
                ui.set_stage("Choose a game or game library folder inside the drive.".into());
            } else if !state.settings.scan_directories.contains(&path) {
                state.settings.scan_directories.push(path); save(&ui, state); scan(&ui, state);
            }
        }
    });
    ui_callback!(on_remove_location, |ui, state, index| {
        if (index as usize) < state.settings.scan_directories.len() {
            state.settings.scan_directories.remove(index as usize); save(&ui, state); scan(&ui, state);
        }
    });
    ui_callback!(on_add_executable, |ui, state| {
        if let Some(path) = pick_file("Game executable", &["exe"]) {
            if discovery::executable(&path) {
                if !state.settings.executables.iter().any(|entry| same_executable(entry, &path)) { state.settings.executables.push(path.canonicalize().unwrap_or(path)); save(&ui, state); }
                scan(&ui, state);
            } else { ui.set_stage("Choose an x86 or x64 Windows executable.".into()); }
        }
    });
    ui_callback!(on_contrast_setting, |ui, state, enabled| { state.settings.high_contrast = enabled; save(&ui, state); });
    ui_callback!(on_scan_setting, |ui, state, enabled| { state.settings.scan_on_launch = enabled; save(&ui, state); });
    ui_callback!(on_shared_setting, |ui, state, enabled| { state.settings.shared_shaders = enabled; save(&ui, state); });
    ui_callback!(on_backup_setting, |ui, state, enabled| { state.settings.keep_backups = enabled; save(&ui, state); });
    ui_callback!(on_choose_cache, |ui, state| {
        if let Some(path) = pick_folder() { state.settings.package_directory = Some(path.join("ReShade Downloads")); save(&ui, state); }
    });
    ui_callback!(on_check_updates, |ui, state| {
        let cache = cache(state);
        let catalog_path = state.data.join("metadata/EffectPackages.ini");
        let events = state.events.clone();
        ui.set_busy(true);
        ui.set_stage("Checking cached package revisions…".into());
        thread::spawn(move || {
            let result = (|| -> Result<String> {
                let packages = catalog::packages(storage::parse_ini(&fs::read_to_string(catalog_path).context("Open an installation's shader catalog first")?), None);
                let client = catalog::client_with_timeout(Duration::from_secs(5))?;
                let (updates, errors) = catalog::available_updates(&client, &cache, &packages)?;
                let mut text = if updates.is_empty() { "No updates found for cached shader packages.".to_owned() }
                    else { format!("{} updates · {}", updates.len(), updates.join(", ")) };
                if errors > 0 { text.push_str(&format!(" · {errors} repositories unavailable")); }
                Ok(text)
            })();
            let _ = events.send(Event::CacheUpdates(result));
        });
    });
    ui_callback!(on_cache_action, |ui, state, action| { clean_cache(&ui, state, Some(action)); });
    ui_callback!(on_set_limits, |ui, state, packages, artwork| {
        match (packages.parse::<u64>(), artwork.parse::<u64>()) {
            (Ok(packages), Ok(artwork)) if (1..=102_400).contains(&packages) && (1..=4096).contains(&artwork) => {
                state.settings.package_limit_mb = packages; state.settings.artwork_limit_mb = artwork;
                save(&ui, state);
                clean_cache(&ui, state, None);
            }
            _ => ui.set_stage("Enter package and artwork limits in whole MB.".into()),
        }
    });
    ui_callback!(on_set_classic, |ui, state, enabled| {
        state.settings.classic = enabled; save(&ui, state);
        if let Some(session) = state.session.as_mut() { session.expanded.clear(); }
        render_catalog(&ui, state);
    });
    ui_callback!(on_artwork_action, |ui, state, refresh| {
        let Some(key) = state.session.as_ref().map(|s| s.key.clone()) else { return; };
        if !refresh {
            let Some(path) = pick_file("Game artwork", &["jpg", "jpeg", "png", "webp", "ico"]) else { return; };
            state.settings.artwork.insert(key.clone(), path);
            save(&ui, state);
        }
        artwork_jobs(state, Some((key, refresh)));
    });
    ui_callback!(on_diagnostics, |ui, state| { report(&ui, state, open_path(&state.data)); });
    ui_callback!(on_presets, |ui, state| {
        let Some(Ok(target)) = state.session.as_ref().map(|s| &s.target) else { return; };
        let result = manage_preset(target);
        report(&ui, state, result);
    });
    ui_callback!(on_import_existing, |ui, state, addon| {
        let Some(session) = state.session.as_ref() else { return; };
        let key = session.key.clone();
        let Ok(target) = &session.target else { return; };
        match install::adopt(target, addon) {
            Ok(()) => {
                details(&ui, state, &key);
                ui.set_stage("Installation imported. Existing files and presets were preserved.".into());
            }
            Err(error) => report(&ui, state, Err(error)),
        }
    });
    ui_callback!(on_legacy, |ui, state| { legacy_setup(&ui, state); });
    ui_callback!(on_game_folder, |ui, state| {
        if let Some(session) = &state.session {
            if let Ok(target) = &session.target {
                report(&ui, state, open_path(&target.base));
            }
        }
    });
    ui_callback!(on_forget_game, |ui, state| {
        let key = ui.get_game().key;
        if let Some(entry) = state.games.iter().find(|entry| entry.game.key() == key.as_str()) {
            let path = entry.game.executable.clone();
            state.settings.executables.retain(|entry| !same_executable(entry, &path));
            ui.set_page(0);
            ui.set_game(GameRow::default());
            state.session = None;
            save(&ui, state);
            scan(&ui, state);
        }
    });
    ui_callback!(on_begin, |ui, state, operation, addon| {
        begin(&ui, state, operation.as_str(), addon);
    });
    ui_callback!(on_accept_warning, |ui, state| {
        if let Some(session) = state.session.as_mut() { session.acknowledged = true; }
        prepare_review(&ui, state);
    });
    ui_callback!(on_set_api, |ui, _state, index| {
        ui.set_selected_api(index);
        ui.set_renderer_handoff(Api::ALL.get(index as usize) == Some(&Api::Vulkan));
    });
    ui_callback!(on_load_catalog, |ui, state| {
        if ui.get_page() == 4 {
            if let Some(session) = state.session.as_mut() {
                session.api = Api::ALL.get(ui.get_selected_api() as usize).copied();
            }
        }
        prepare_review(&ui, state);
    });
    ui_callback!(on_use_cached_runtime, |ui, state| {
        if !ui.get_can_use_cached_runtime() { return; }
        if let Some(session) = state.session.as_mut() { session.runtime_only = true; }
        ui.set_catalog_failed(false);
        review(&ui, state);
    });
    ui_callback!(on_edit_renderer, |ui, state| {
        if let Some(session) = &state.session {
            ui.set_selected_api(session.api.and_then(|api| Api::ALL.iter().position(|a| *a == api)).map(|i| i as i32).unwrap_or(-1));
            ui.set_renderer_handoff(session.api == Some(Api::Vulkan));
            ui.set_page(4);
        }
    });
    ui_callback!(on_edit_content, |ui, state, page| { show_content(&ui, state, page); });
    ui_callback!(on_catalog_search, |ui, state, query| {
        if let Some(session) = state.session.as_mut() { session.query = query.to_lowercase(); render_catalog(&ui, state); }
    });
    ui_callback!(on_category, |ui, state, index| {
        if let Some(session) = state.session.as_mut() {
            session.category = (index as usize).min(CATEGORIES.len() - 1);
            ui.set_selected_category(session.category as i32);
            render_catalog(&ui, state);
        }
    });
    ui_callback!(on_catalog_action, |ui, state, key, action| {
        if let Some(session) = state.session.as_mut() {
            if ui.get_page() == 6 {
                if action == 1 { session.selection.addons.insert(key.to_string()); } else { session.selection.addons.remove(key.as_str()); }
            } else {
                let (id, effect) = key.split_once('|').map_or((key.as_str(), None), |(id, effect)| (id, Some(effect)));
                if let Some(package) = session.shaders.iter().find(|p| p.id == id) {
                    match (action, effect) {
                        (0, _) => { if !session.expanded.remove(id) { session.expanded.insert(id.to_owned()); } }
                        (_, Some(effect)) => session.selection.set_effect(package, effect, action == 1),
                        (_, None) => session.selection.set_all(package, action == 1),
                    }
                }
            }
            render_catalog(&ui, state);
        }
    });
    ui_callback!(on_review, |ui, state| { review(&ui, state); });
    ui_callback!(on_step_back, |ui, state| {
        let page = ui.get_page();
        if matches!(page, 5 | 6 | 8) || (page == 4 && state.session.as_ref().is_some_and(|s| !s.shaders.is_empty() && s.api.is_some())) {
            review(&ui, state);
        } else {
            details(&ui, state, ui.get_game().key.as_str());
        }
    });
    ui_callback!(on_execute, |ui, state| { run_install(&ui, state); });
}

fn report(ui: &ManagerWindow, state: &State, result: Result<()>) {
    if let Err(error) = result {
        diagnostics(&state.data, &format!("{error:#}"));
        ui.set_stage(format!("{error:#}").into());
    }
}

fn clean_cache(ui: &ManagerWindow, state: &State, action: Option<i32>) {
    let cache = cache(state);
    let artwork_folder = state.data.join("artwork");
    let package_limit = state.settings.package_limit_mb * 1_048_576;
    let artwork_limit = state.settings.artwork_limit_mb * 1_048_576;
    let events = state.events.clone();
    ui.set_busy(true);
    thread::spawn(move || {
        let result = match action {
            Some(0) => cache.trim(0),
            Some(1) => { artwork::trim(&artwork_folder, 0, None); Ok(()) }
            _ => { artwork::trim(&artwork_folder, artwork_limit, None); cache.trim(package_limit) }
        };
        let _ = events.send(Event::Cache(result));
    });
}

#[cfg(windows)]
fn pick_folder() -> Option<PathBuf> { rfd::FileDialog::new().pick_folder() }
#[cfg(not(windows))]
fn pick_folder() -> Option<PathBuf> { None }
#[cfg(windows)]
fn pick_file(label: &str, extensions: &[&str]) -> Option<PathBuf> {
    rfd::FileDialog::new().add_filter(label, extensions).pick_file()
}
#[cfg(not(windows))]
fn pick_file(_: &str, _: &[&str]) -> Option<PathBuf> { None }

#[cfg(windows)]
fn open_path(path: &Path) -> Result<()> {
    std::process::Command::new("explorer.exe").arg(path).spawn().context("Open folder in Explorer")?;
    Ok(())
}
#[cfg(not(windows))]
fn open_path(_: &Path) -> Result<()> { bail!("This action is available on Windows.") }

fn manage_preset(target: &Target) -> Result<()> {
    let config = storage::safe_child(&target.base, Path::new("ReShade.ini"))?;
    let Some(source) = pick_file("ReShade preset", &["ini"]) else { return Ok(()); };
    let text = fs::read_to_string(&config).context("The game has no readable ReShade configuration. Repair the installation before choosing a preset")?;
    let directory = storage::safe_child(&target.base, Path::new(".reshade-manager/presets"))?;
    let bytes = fs::read(&source)?;
    if bytes.len() > 8 * 1024 * 1024 { bail!("Preset exceeds size limit."); }
    let name = format!("{}.ini", storage::sha256(&bytes));
    let destination = storage::safe_child(&directory, Path::new(&name))?;
    storage::atomic_write(&destination, &bytes)?;
    let text = storage::set_ini(&text, "GENERAL", "PresetPath", &format!(r".\.reshade-manager\presets\{name}"));
    storage::atomic_write(&config, text.as_bytes())
}

fn legacy_setup(ui: &ManagerWindow, state: &State) {
    let Some(session) = &state.session else { return; };
    let Ok(target) = &session.target else { return; };
    if target.banned || !state.data.join("metadata/Compatibility.ini").is_file() {
        ui.set_stage("The compatibility catalog blocks setup or is unavailable. Rescan online to verify the game.".into());
        return;
    }
    if session.addon && !session.acknowledged {
        ui.set_stage("Acknowledge the full add-on warning to open the add-on-enabled installer.".into());
        return;
    }
    let executable = target.executable.clone();
    let url = install::runtime_url(session.addon);
    let addon = session.addon;
    let vulkan = session.api == Some(Api::Vulkan);
    let cache = cache(state);
    let setup_folder = state.data.join("legacy-setup");
    let events = state.events.clone();
    ui.set_busy(true);
    ui.set_stage("Opening official setup. Close it to return here.".into());
    thread::spawn(move || {
        let result = (|| -> Result<()> {
            let client = catalog::client()?;
            let (cached, bytes) = cache.obtain(&client, &url, false, false, |data| install::verify_distribution(data, addon))?;
            let setup = setup_folder.join(format!("{}.exe", cached.digest));
            storage::atomic_write(&setup, &bytes)?;
            let mut command = std::process::Command::new(setup);
            command.arg(&executable);
            if vulkan { command.args(["--api", "vulkan"]); }
            let mut child = command.spawn().context("Open the verified ReShade installer")?;
            child.wait().context("Wait for the official ReShade installer")?;
            Ok(())
        })();
        let _ = events.send(Event::Legacy(result));
    });
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        if let Ok(data) = storage::data_directory() { diagnostics(&data, &info.to_string()); }
    }));
    if let Err(error) = run() {
        if let Ok(data) = storage::data_directory() { diagnostics(&data, &format!("{error:#}")); }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let start = std::time::Instant::now();
    if std::env::args().any(|arg| arg == "--render-fixtures") {
        return render_fixtures();
    }
    let data = storage::data_directory()?;
    fs::create_dir_all(&data)?;
    let settings = storage::read_json::<Settings>(&data.join("settings.json")).unwrap_or_else(|error| {
        diagnostics(&data, &format!("Could not read settings · {error:#}"));
        Settings::default()
    });
    let (events, receiver) = mpsc::channel();
    let metadata = compatibility(&data);
    let games = storage::read_json::<Vec<Game>>(&data.join("library.json")).unwrap_or_default().into_iter().map(|game| {
        let summary = inspect(&game, &metadata);
        ViewGame { game, summary, artwork: None }
    }).collect();
    let state = Rc::new(RefCell::new(State { data, settings, games, query: String::new(), filter: 0, session: None, events, artwork_generation: 0, checking_compatibility: false }));
    let ui = ManagerWindow::new()?;
    ui.set_apis(rows(Api::ALL.iter().map(|api| api.label().into()).collect()));
    ui.set_categories(rows(CATEGORIES.iter().map(|label| (*label).into()).collect()));
    render_settings(&ui, &state.borrow());
    render_games(&ui, &state.borrow());
    bind(&ui, &state);
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    let event_model = state.clone();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(60), move || {
        if let Some(ui) = weak.upgrade() {
            let Ok(mut state) = event_model.try_borrow_mut() else { return; };
            for event in receiver.try_iter().take(12) { handle(&ui, &mut state, event); }
        }
    });
    let data = state.borrow().data.clone();
    if std::env::args().any(|arg| arg == "--measure-startup") {
        let sample = data.join("startup-ms.txt");
        slint::Timer::single_shot(Duration::ZERO, move || { let _ = fs::write(sample, start.elapsed().as_millis().to_string()); });
    } else if state.borrow().settings.scan_on_launch {
        scan(&ui, &mut state.borrow_mut());
    } else {
        artwork_jobs(&mut state.borrow_mut(), None);
    }
    let automatic = slint::Timer::default();
    let weak = ui.as_weak();
    let monitor = state.clone();
    automatic.start(slint::TimerMode::Repeated, Duration::from_secs(180), move || {
        let Ok(mut model) = monitor.try_borrow_mut() else { return; };
        if let Some(ui) = weak.upgrade().filter(|ui| ui.get_page() <= 2 && !ui.get_busy() && model.settings.scan_on_launch) {
            scan(&ui, &mut model);
        }
    });
    ui.run()?;
    Ok(())
}


fn render_fixtures() -> Result<()> {
    let page = if std::env::args().any(|arg| arg == "--review") { 7 }
        else if std::env::args().any(|arg| arg == "--recovery") { 8 }
        else if std::env::args().any(|arg| arg == "--details") { 2 }
        else { 0 };
    let ui = ManagerWindow::new()?;
    let directory = storage::data_directory()?;
    fs::create_dir_all(&directory)?;
    let mut fixture_games = vec![
        GameRow { key: "1".into(), title: "Direct3D 11 fixture".into(), source: "Steam".into(), status: "Installed · 6.8.0".into(), installed: true, art: Image::default(), selected: false },
        GameRow { key: "2".into(), title: "OpenGL fixture".into(), source: "Epic Games".into(), status: "Not installed".into(), installed: false, art: Image::default(), selected: false },
        GameRow { key: "3".into(), title: "Vulkan fixture".into(), source: "GOG Galaxy".into(), status: "Not installed".into(), installed: false, art: Image::default(), selected: false },
    ];
    if page != 0 {
        ui.set_game(fixture_games[1].clone());
        ui.set_game_facts("OpenGL  ·  x64".into());
        ui.set_game_path(r"C:\Games\Fixture\Fixture.exe".into());
        ui.set_primary_action("Set up ReShade".into());
        ui.set_can_addon(true);
        fixture_games[1].selected = true;
        ui.set_page(page);
        ui.set_operation("Install".into());
        ui.set_summary("OpenGL fixture\nOpenGL · x64\n\nStandard ReShade 6.8.0 · cached\n\nDestination\nC:\\Games\\Fixture\n\nShader packages\n  Standard effects · cached\n\nAdd-ons\n  None selected".into());
        ui.set_can_edit_renderer(true);
        ui.set_can_edit_content(true);
        if page == 8 {
            ui.set_result_failed(true);
            ui.set_stage("The game folder is in use. Close the game and review the installation before retrying.".into());
        }
    }
    ui.set_library_count(fixture_games.len() as i32);
    ui.set_games(rows(fixture_games));
    let name = match page {
        2 => "game-details.png",
        7 => "install-review.png",
        8 => "install-recovery.png",
        _ => "game-library.png",
    };
    let path = directory.join(name);
    let weak = ui.as_weak();
    let capture_result = Rc::new(RefCell::new(None));
    let callback_result = capture_result.clone();
    slint::Timer::single_shot(Duration::from_millis(750), move || {
        let result = (|| -> Result<()> {
            let window = weak.upgrade().context("Fixture window was closed")?;
            let pixels = window.window().take_snapshot().context("Capture native Slint window")?;
            let rgb: Vec<u8> = pixels.as_slice().iter().flat_map(|p| [p.r, p.g, p.b]).collect();
            image::save_buffer(path, &rgb, pixels.width(), pixels.height(), image::ColorType::Rgb8)?;
            Ok(())
        })();
        *callback_result.borrow_mut() = Some(result);
        let _ = slint::quit_event_loop();
    });
    ui.run()?;
    let result = capture_result.borrow_mut().take();
    result.context("The window closed before the fixture was captured")?
}

#[cfg(test)]
mod workflow_tests {
    use super::*;
    use reshade_manager::pe::Arch;

    fn target(api: Option<Api>) -> Target {
        Target {
            executable: "game.exe".into(), base: ".".into(), arch: Arch::X64, api,
            forced: false, banned: false, notes: vec![], compatibility: Default::default(),
        }
    }

    #[test]
    fn chooses_setup_compatibility_or_official_handoff_for_new_games() {
        let status = install::Status::default();
        let receipt = install::Receipt::default();
        assert_eq!(next_action(Some(&target(None)), &status, &receipt, true), GameAction::Install);
        assert_eq!(next_action(Some(&target(None)), &status, &receipt, false), GameAction::CheckCompatibility);
        assert_eq!(next_action(Some(&target(Some(Api::Vulkan))), &status, &receipt, true), GameAction::Legacy);
        assert_eq!(next_action(None, &status, &receipt, true), GameAction::Unavailable);
    }

    #[test]
    fn routes_existing_installations_by_ownership_runtime_and_version() {
        let target = target(Some(Api::Dxgi));
        let mut status = install::Status { wrapper: Some("dxgi.dll".into()), ..Default::default() };
        let mut receipt = install::Receipt::default();
        assert_eq!(next_action(Some(&target), &status, &receipt, true), GameAction::Import);
        receipt.game = Some(target.executable.clone());
        status.version = Some("6.7.0.2045".into());
        assert_eq!(next_action(Some(&target), &status, &receipt, true), GameAction::Update);
        status.version = Some(install::RUNTIME_VERSION.into());
        assert_eq!(next_action(Some(&target), &status, &receipt, true), GameAction::Shaders);
        status.wrapper = None;
        assert_eq!(next_action(Some(&target), &status, &receipt, true), GameAction::Repair);
        status.receipt_conflict = true;
        assert_eq!(next_action(Some(&target), &status, &receipt, true), GameAction::Unavailable);
    }
}
