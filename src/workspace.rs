//! The window's main view: collection sidebar, environment picker, and request editor.

pub mod palette;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::TextareaState;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, IndexPath, Root, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::credentials::{hoist_credentials_with, hoist_header, reserved_names, unique_name};
use crate::environment_editor::{EnvironmentEditor, EnvironmentEditorEvent, Target};
use crate::graphql::SchemaCache;
use crate::import::postman::ImportItem;
use crate::import::{curl, postman};
use crate::model::{RequestFile, Variables, placeholder};
use crate::paths::{AppPaths, AppState};
use crate::project;
use crate::request_editor::{RequestEditor, RequestEditorEvent};
use crate::response_cache::{CacheKey, Liveness, ResponseCache};
use crate::secret_store::{self, DEFAULTS_LABEL, DEFAULTS_SCOPE, SecretRef, SecretStore, SecretWrite};
use crate::settings::AppSettings;
use crate::storage::{self, Collection, Item};
use crate::ui::{dialog_footer, textarea};

type EnvironmentSelect = SelectState<SearchableVec<SharedString>>;

/// A folder the app was started with (argument or working directory).
pub struct Launch {
    pub dir: PathBuf,
    /// Given on the command line, so offer to create a collection if there isn't one.
    pub explicit: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum MainView {
    Request,
    Environments,
}

pub struct Workspace {
    paths: AppPaths,
    state: AppState,
    collections: Vec<Collection>,
    collapsed: HashSet<PathBuf>,
    editor: Entity<RequestEditor>,
    environment: Entity<EnvironmentSelect>,
    environment_editor: Entity<EnvironmentEditor>,
    main_view: MainView,
    focus_handle: FocusHandle,
    secret_store: Option<SecretStore>,
}

impl Workspace {
    pub fn new(paths: AppPaths, launch: Option<Launch>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            RequestEditor::new(
                ResponseCache::new(&paths.cache_dir),
                SchemaCache::new(&paths.cache_dir),
                window,
                cx,
            )
        });
        let environment = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(vec![SharedString::from(t!("ws.no_environment").to_string())]),
                None,
                window,
                cx,
            )
        });

        cx.subscribe_in(&editor, window, |this, _, event, window, cx| match event {
            RequestEditorEvent::Saved(path) => this.reload_containing(path, window, cx),
            RequestEditorEvent::MoveHeaderToSecret { path, index } => {
                this.move_header_to_secret(path.clone(), *index, window, cx)
            }
            RequestEditorEvent::Error(message) => notify_error(message.clone(), window, cx),
        })
        .detach();
        let environment_editor = cx.new(|cx| EnvironmentEditor::new(window, cx));
        cx.subscribe_in(&environment_editor, window, |this, _, event, window, cx| match event {
            EnvironmentEditorEvent::Changed(root) => this.reload_collection(root, window, cx),
            EnvironmentEditorEvent::Deleted { root, path } => {
                if this.state.active_environments.get(root) == Some(path) {
                    this.state.active_environments.remove(root);
                    this.save_state();
                }
                this.reload_collection(root, window, cx);
            }
            EnvironmentEditorEvent::Error(message) => notify_error(message.clone(), window, cx),
            EnvironmentEditorEvent::Close => {
                this.close_environments(window, cx);
            }
        })
        .detach();
        cx.subscribe_in(&environment, window, |this, select, _: &SelectEvent<_>, window, cx| {
            let index = select.read(cx).selected_index(cx).map(|ix| ix.row);
            this.choose_environment(index, window, cx);
        })
        .detach();

        let mut this = Self {
            state: paths.load_state(),
            paths,
            collections: Vec::new(),
            collapsed: HashSet::new(),
            editor,
            environment,
            environment_editor,
            main_view: MainView::Request,
            focus_handle: cx.focus_handle(),
            secret_store: None,
        };
        this.connect_secret_store(window, cx);
        this.start_response_tidy(cx);
        this.restore(launch, window, cx);
        this.focus_handle.focus(window, cx);
        this
    }

    /// Reopens last session's projects, then the project the app was launched for.
    fn restore(&mut self, launch: Option<Launch>, window: &mut Window, cx: &mut Context<Self>) {
        for project in self.state.open_projects.clone() {
            match storage::load_collection(&project::collection_dir(&project)) {
                Ok(collection) => self.collections.push(collection),
                Err(e) => eprintln!("skipping project {}: {e:#}", project.display()),
            }
        }
        self.state.open_projects = self
            .collections
            .iter()
            .map(|c| project::project_dir(&c.root).to_path_buf())
            .collect();
        self.save_state();

        let last = self
            .state
            .last_request
            .clone()
            .filter(|p| self.find_request(p).is_some());
        let first = || self.collections.iter().find_map(Collection::first_request);
        if let Some(path) = last.or_else(first) {
            self.select_request(path, window, cx);
        } else {
            self.refresh_environments(window, cx);
        }

        if let Some(launch) = launch {
            match project::find(&launch.dir) {
                Some(root) => self.open_collection(root, window, cx),
                None if launch.explicit => {
                    let weak = cx.entity().downgrade();
                    // Dialogs need the window's root view, which exists once this view does.
                    window.defer(cx, move |window, cx| {
                        weak.update(cx, |this, cx| this.offer_init_project(launch.dir, window, cx))
                            .ok();
                    });
                }
                None => {}
            }
        }
    }

    fn save_state(&self) {
        if let Err(e) = self.paths.save_state(&self.state) {
            eprintln!("could not save state: {e:#}");
        }
    }

    fn find_request(&self, path: &Path) -> Option<&RequestFile> {
        self.collections.iter().find_map(|c| c.find_request(path))
    }

    fn collection_index_for(&self, path: &Path) -> Option<usize> {
        self.collections.iter().position(|c| path.starts_with(&c.root))
    }

    /// The collection whose environments are being managed, else the one owning the open
    /// request, else the first one.
    fn active_collection(&self, cx: &App) -> Option<&Collection> {
        let path = match self.main_view {
            MainView::Environments => self.environment_editor.read(cx).root().map(Path::to_path_buf),
            MainView::Request => self.editor.read(cx).path().cloned(),
        };
        path.and_then(|p| self.collection_index_for(&p))
            .or((!self.collections.is_empty()).then_some(0))
            .map(|ix| &self.collections[ix])
    }

    // MARK: Collections

    fn open_collection(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.collections.iter().position(|c| c.root == root) {
            // Already open: bring it into view, unless one of its requests is already showing.
            self.collapsed.remove(&root);
            let showing = self
                .editor
                .read(cx)
                .path()
                .is_some_and(|p| self.collection_index_for(p) == Some(ix));
            if !showing && let Some(path) = self.collections[ix].first_request() {
                self.select_request(path, window, cx);
            }
            cx.notify();
            return;
        }
        match storage::load_collection(&root) {
            Ok(collection) => {
                for (path, error) in &collection.errors {
                    eprintln!("{}: {error}", path.display());
                }
                let first = collection.first_request();
                self.collections.push(collection);
                self.state.open_projects.push(project::project_dir(&root).to_path_buf());
                self.save_state();
                if let Some(path) = first {
                    self.select_request(path, window, cx);
                }
                cx.notify();
            }
            Err(e) => notify_error(format!("{e:#}"), window, cx),
        }
    }

    fn reload_collection(&mut self, root: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.collections.iter().position(|c| c.root == root) else {
            return;
        };
        match storage::load_collection(root) {
            Ok(collection) => {
                if !collection.errors.is_empty() {
                    let files: Vec<_> = collection.errors.iter().map(|(p, _)| p.display().to_string()).collect();
                    notify_error(
                        t!("ws.could_not_read", files = files.join(", ")).to_string(),
                        window,
                        cx,
                    );
                }
                if self.environment_editor.read(cx).root() == Some(root) {
                    self.environment_editor.update(cx, |editor, cx| {
                        editor.update_collection(collection.clone(), window, cx)
                    });
                }
                self.collections[ix] = collection;
            }
            Err(e) => notify_error(format!("{e:#}"), window, cx),
        }
        self.refresh_environments(window, cx);
        cx.notify();
    }

    fn reload_containing(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.collection_index_for(path) {
            let root = self.collections[ix].root.clone();
            self.reload_collection(&root, window, cx);
        }
    }

    fn close_collection(&mut self, root: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            if editor.path().is_some_and(|p| p.starts_with(root)) {
                editor.save(cx);
                editor.unload(window, cx);
            }
        });
        if self.environment_editor.read(cx).root() == Some(root) {
            self.environment_editor.update(cx, |editor, cx| {
                editor.save(window, cx);
                editor.close(cx);
            });
            self.main_view = MainView::Request;
        }
        self.collections.retain(|c| c.root != root);
        self.state.open_projects.retain(|p| p != project::project_dir(root));
        self.state.active_environments.remove(root);
        self.save_state();
        self.refresh_environments(window, cx);
        cx.notify();
    }

    /// The response-cache key for a request in an open collection.
    fn response_key(&self, path: &Path) -> Option<CacheKey> {
        Some(self.collections[self.collection_index_for(path)?].response_key(path))
    }

    /// Shows a request from an open collection in the editor.
    fn load_in_editor(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(request) = self.find_request(path).cloned() {
            let key = self.response_key(path);
            self.editor.update(cx, |editor, cx| {
                editor.load(path.to_path_buf(), request, key, window, cx)
            });
        }
    }

    fn select_request(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_request(&path).is_none() {
            return;
        }
        if self.main_view == MainView::Environments && !self.close_environments(window, cx) {
            return;
        }
        // Switching requests saves the one you were editing, like most modern API clients.
        self.editor.update(cx, |editor, cx| editor.save(cx));
        self.load_in_editor(&path, window, cx);
        self.state.last_request = Some(path);
        self.save_state();
        self.refresh_environments(window, cx);
        cx.notify();
    }

    fn new_request(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match storage::create_request(&dir, &RequestFile::new(t!("ws.new_request"))) {
            Ok(path) => {
                self.collapsed.remove(&dir);
                self.reload_containing(&path, window, cx);
                self.select_request(path, window, cx);
            }
            Err(e) => notify_error(format!("{e:#}"), window, cx),
        }
    }

    // MARK: Environments

    fn refresh_environments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (names, selected, layered, active) = match self.active_collection(cx) {
            Some(collection) => {
                let active = self.state.active_environments.get(&collection.root);
                let selected = active
                    .and_then(|path| collection.environments.iter().position(|e| &e.path == path))
                    .map(|ix| ix + 1);
                let environment = selected
                    .map(|ix| &collection.environments[ix - 1])
                    .map(|env| (env.path.as_path(), &env.file));
                let names = std::iter::once(SharedString::from(t!("ws.no_environment").to_string()))
                    .chain(collection.environments.iter().map(|e| e.file.name.clone().into()))
                    .collect::<Vec<_>>();
                (
                    names,
                    selected.unwrap_or(0),
                    secret_store::layer(&collection.file, environment),
                    active.cloned(),
                )
            }
            None => (
                vec![t!("ws.no_environment").to_string().into()],
                0,
                secret_store::Layered::default(),
                None,
            ),
        };
        self.environment.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(names), window, cx);
            select.set_selected_index(Some(IndexPath::new(selected)), window, cx);
        });
        self.editor
            .update(cx, |editor, _| editor.set_variables(layered.variables, layered.secrets));
        self.environment_editor
            .update(cx, |editor, cx| editor.set_active(active, cx));
    }

    fn manage_environments(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.collections.iter().find(|c| c.root == root).cloned() else {
            return;
        };
        let editing_other = self.environment_editor.read(cx).root().is_some_and(|r| r != root);
        if editing_other && !self.environment_editor.update(cx, |editor, cx| editor.save(window, cx)) {
            return;
        }
        self.editor.update(cx, |editor, cx| editor.save(cx));
        let target = self
            .state
            .active_environments
            .get(&root)
            .cloned()
            .map_or(Target::Defaults, Target::Environment);
        self.environment_editor
            .update(cx, |editor, cx| editor.open(collection, target, window, cx));
        self.main_view = MainView::Environments;
        self.refresh_environments(window, cx);
        cx.notify();
    }

    /// Saves and leaves the environment manager. Returns false, staying put, if the
    /// pending edits are invalid.
    fn close_environments(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.environment_editor.update(cx, |editor, cx| editor.save(window, cx)) {
            return false;
        }
        self.environment_editor.update(cx, |editor, cx| editor.close(cx));
        self.main_view = MainView::Request;
        self.refresh_environments(window, cx);
        cx.notify();
        true
    }

    fn set_language(&mut self, language: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        AppSettings::update(cx, |settings| settings.language = language.clone());
        crate::i18n::apply(crate::i18n::resolve(language.as_deref()));
        // Strings built during render update on the next frame; placeholders set when inputs
        // were created need re-applying.
        self.editor.update(cx, |editor, cx| editor.relocalize(window, cx));
        self.environment_editor
            .update(cx, |editor, cx| editor.relocalize(window, cx));
        self.refresh_environments(window, cx);
        cx.refresh_windows();
    }

    /// Shortly after startup and then hourly, deletes saved responses whose request is gone.
    fn start_response_tidy(&mut self, cx: &mut Context<Self>) {
        const FIRST_RUN: Duration = Duration::from_secs(30);
        const EVERY: Duration = Duration::from_secs(60 * 60);
        let cache = ResponseCache::new(&self.paths.cache_dir);
        let schemas = SchemaCache::new(&self.paths.cache_dir);
        cx.spawn(async move |this, cx| {
            let mut delay = FIRST_RUN;
            loop {
                cx.background_executor().timer(delay).await;
                delay = EVERY;
                let schemas = schemas.clone();
                cx.background_executor()
                    .spawn(async move {
                        match schemas.tidy(std::time::SystemTime::now()) {
                            Ok(0) => {}
                            Ok(removed) => eprintln!("tidied schema cache: removed {removed}"),
                            Err(e) => eprintln!("could not tidy schema cache: {e:#}"),
                        }
                    })
                    .await;
                let Ok(roots) = this.update(cx, |this, cx| {
                    let roots: Vec<_> = this.collections.iter().map(|c| c.root.clone()).collect();
                    AppSettings::get(cx).remember_responses.then_some(roots)
                }) else {
                    break;
                };
                let Some(roots) = roots else {
                    continue;
                };
                let cache = cache.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { cache.tidy(&liveness_from_disk(&roots), std::time::SystemTime::now()) })
                    .await;
                match result {
                    Ok(report) if report.removed > 0 => {
                        eprintln!(
                            "tidied response cache: removed {}, kept {}",
                            report.removed, report.kept
                        )
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("could not tidy response cache: {e:#}"),
                }
            }
        })
        .detach();
    }

    fn set_remember_responses(&mut self, remember: bool, window: &mut Window, cx: &mut Context<Self>) {
        AppSettings::update(cx, |settings| settings.remember_responses = remember);
        if !remember {
            // Turning it off means nothing should stay on disk.
            self.clear_saved_responses(window, cx);
        }
    }

    fn clear_saved_responses(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let removed = self.editor.update(cx, |editor, cx| editor.clear_responses(window, cx));
        window.push_notification(
            Notification::success(t!("ws.responses_cleared", count = removed).to_string()),
            cx,
        );
    }

    fn choose_environment(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.active_collection(cx) else {
            return;
        };
        let root = collection.root.clone();
        match index
            .filter(|ix| *ix > 0)
            .and_then(|ix| collection.environments.get(ix - 1))
        {
            Some(env) => {
                let path = env.path.clone();
                self.state.active_environments.insert(root, path);
            }
            None => {
                self.state.active_environments.remove(&root);
            }
        }
        self.save_state();
        self.refresh_environments(window, cx);
    }

    // MARK: Dialogs and imports

    /// Shows the file chooser, then hands the chosen path to `apply`.
    fn pick_path(
        &mut self,
        directories: bool,
        prompt: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut Self, PathBuf, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: !directories,
            directories,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| apply(this, dir, window, cx)).ok();
        })
        .detach();
    }

    /// Asks for a project folder, then hands it to `apply`.
    fn pick_project_folder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut Self, PathBuf, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.pick_path(true, t!("ws.open_project").to_string(), window, cx, apply);
    }

    fn open_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_project_folder(window, cx, |this, dir, window, cx| match project::find(&dir) {
            Some(root) => this.open_collection(root, window, cx),
            None => this.offer_init_project(dir, window, cx),
        });
    }

    /// Confirms, then creates `.courier/` in a folder that has no collection yet.
    fn offer_init_project(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        window.open_dialog(cx, move |dialog, _, _| {
            let (weak, dir) = (weak.clone(), dir.clone());
            let message = t!(
                "ws.init_project_message",
                path = dir.display(),
                dir = project::DOT_DIR,
                name = name
            )
            .to_string();
            dialog
                .title(t!("ws.init_project_title").to_string())
                .w(px(480.))
                .content(move |content, _, _| content.child(message.clone()))
                .footer(dialog_footer(Some(t!("ws.create").to_string()), ButtonVariant::Primary))
                .on_ok(move |_, window, cx| {
                    let result = project::init(&dir);
                    weak.update(cx, |this, cx| match result {
                        Ok(root) => this.open_collection(root, window, cx),
                        Err(e) => notify_error(format!("{e:#}"), window, cx),
                    })
                    .ok();
                    true
                })
        });
    }

    fn import_curl_dialog(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let command = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(8)
                .placeholder(t!("ws.curl_placeholder").to_string())
        });
        command.focus_handle(cx).focus(window, cx);
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let command = command.clone();
            let weak = weak.clone();
            let dir = dir.clone();
            dialog
                .title(t!("ws.import_curl_title").to_string())
                .footer(dialog_footer(Some(t!("ws.import").to_string()), ButtonVariant::Primary))
                .w(px(640.))
                .content({
                    let command = command.clone();
                    move |content, _, _| content.child(textarea(&command))
                })
                .on_ok(move |_, window, cx| {
                    let text = command.read(cx).value().to_string();
                    let result = curl::parse(&text).and_then(|mut request| {
                        let (hoisted, writes) = weak
                            .update(cx, |this, _| this.hoist_into_defaults(&dir, &mut request))
                            .map_err(|_| anyhow::anyhow!("workspace closed"))??;
                        let path = storage::create_request(&dir, &request)?;
                        Ok((path, hoisted, writes))
                    });
                    match result {
                        Ok((path, hoisted, writes)) => {
                            weak.update(cx, |this, cx| {
                                this.store_secrets(writes, window, cx);
                                this.reload_containing(&path, window, cx);
                                this.select_request(path, window, cx);
                                if !hoisted.is_empty() {
                                    window.push_notification(
                                        Notification::info(
                                            t!("ws.moved_to_defaults", names = hoisted.join(", ")).to_string(),
                                        ),
                                        cx,
                                    );
                                }
                            })
                            .ok();
                            true
                        }
                        Err(e) => {
                            notify_error(format!("{e:#}"), window, cx);
                            false
                        }
                    }
                })
        });
    }

    /// Asks for a Postman JSON file, then hands its contents to `apply`.
    fn pick_postman_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut Self, String, &mut Window, &mut Context<Self>) -> Result<()> + 'static,
    ) {
        self.pick_path(
            false,
            t!("ws.import_postman_file").to_string(),
            window,
            cx,
            |this, file, window, cx| {
                let result = fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))
                    .and_then(|json| apply(this, json, window, cx));
                if let Err(e) = result {
                    notify_error(format!("{e:#}"), window, cx);
                }
            },
        );
    }

    /// Picks a project folder without a collection, then a Postman file to create it from.
    fn import_postman_as_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_project_folder(window, cx, |this, dir, window, cx| {
            if let Some(existing) = project::find(&dir) {
                notify_error(
                    t!(
                        "ws.project_has_collection",
                        path = project::project_dir(&existing).display()
                    )
                    .to_string(),
                    window,
                    cx,
                );
                return;
            }
            this.pick_postman_file(window, cx, move |this, json, window, cx| {
                this.create_project_from_postman(&dir, &json, window, cx)
            });
        });
    }

    fn create_project_from_postman(
        &mut self,
        dir: &Path,
        json: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let import = postman::parse_collection(json)?;
        let root = postman::write_project_collection(dir, &import)?;
        let collection = storage::load_collection(&root)?;
        let writes = import
            .secrets
            .iter()
            .map(|(name, value)| SecretWrite::new(&collection.file, DEFAULTS_SCOPE, DEFAULTS_LABEL, name, value))
            .collect();
        self.store_secrets(writes, window, cx);
        self.open_collection(root, window, cx);
        notify_import(&import.warnings, window, cx);
        Ok(())
    }

    fn import_postman_into(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_postman_file(window, cx, move |this, json, window, cx| {
            let mut import = postman::parse_collection(&json)?;
            let mut collection = storage::load_collection(&root)?;
            // Imported secret names that clash with this collection's names get a suffix.
            let mut renamed = Variables::new();
            for (name, value) in std::mem::take(&mut import.secrets) {
                let file = &collection.file;
                let new_name = unique_name(&name, |candidate| {
                    file.secrets.iter().any(|s| s == candidate)
                        || file.variables.contains_key(candidate)
                        || renamed.contains_key(candidate)
                });
                if new_name != name {
                    rename_placeholder(&mut import.items, &name, &new_name);
                }
                renamed.insert(new_name, value);
            }
            postman::write_items(&root, &import.items)?;
            if !renamed.is_empty() {
                collection.file.ensure_id();
                collection.file.secrets.extend(renamed.keys().cloned());
                storage::save_collection_file(&root, &collection.file)?;
                let writes = renamed
                    .iter()
                    .map(|(name, value)| {
                        SecretWrite::new(&collection.file, DEFAULTS_SCOPE, DEFAULTS_LABEL, name, value)
                    })
                    .collect();
                this.store_secrets(writes, window, cx);
            }
            this.reload_collection(&root, window, cx);
            notify_import(&import.warnings, window, cx);
            Ok(())
        });
    }

    fn import_postman_environment(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_postman_file(window, cx, move |this, json, window, cx| {
            let import = postman::parse_environment(&json)?;
            let mut collection = storage::load_collection(&root)?;
            if collection.file.ensure_id().1 {
                storage::save_collection_file(&root, &collection.file)?;
            }
            let path = storage::create_environment(&root, &import.file)?;
            let scope = SecretRef::environment_scope(&path);
            let writes = import
                .secrets
                .iter()
                .map(|(name, value)| SecretWrite::new(&collection.file, &scope, &import.file.name, name, value))
                .collect();
            this.store_secrets(writes, window, cx);
            this.state.active_environments.insert(root.clone(), path);
            this.save_state();
            this.reload_collection(&root, window, cx);
            if import.warnings.is_empty() {
                window.push_notification(
                    Notification::success(t!("ws.imported_environment", name = import.file.name).to_string()),
                    cx,
                );
            } else {
                notify_import(&import.warnings, window, cx);
            }
            Ok(())
        });
    }

    // MARK: Secrets

    fn connect_secret_store(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.paths.clone();
        cx.spawn_in(window, async move |this, cx| {
            let store = cx
                .background_executor()
                .spawn(async move {
                    // Tests must never touch the developer's real keyring.
                    #[cfg(test)]
                    let store = {
                        let _ = paths;
                        anyhow::Ok(SecretStore::in_memory())
                    };
                    #[cfg(not(test))]
                    let store = SecretStore::open(&paths).await;
                    store
                })
                .await;
            this.update_in(cx, |this, window, cx| match store {
                Ok(store) => {
                    this.editor
                        .update(cx, |editor, _| editor.set_secret_store(store.clone()));
                    this.environment_editor
                        .update(cx, |editor, cx| editor.set_store(store.clone(), window, cx));
                    this.secret_store = Some(store);
                }
                Err(e) => notify_error(
                    t!("ws.secrets_unavailable", error = format!("{e:#}")).to_string(),
                    window,
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// Writes secret values in the background, reporting failures.
    fn store_secrets(&self, writes: Vec<SecretWrite>, window: &mut Window, cx: &mut Context<Self>) {
        if writes.is_empty() {
            return;
        }
        let Some(store) = self.secret_store.clone() else {
            let names = writes
                .iter()
                .map(|w| w.secret.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            notify_error(
                t!("ws.store_unavailable_no_value", names = names).to_string(),
                window,
                cx,
            );
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { store.apply(&writes, &[]).await })
                .await;
            this.update_in(cx, |_, window, cx| {
                if let Err(e) = result {
                    notify_error(
                        t!("ws.could_not_store_secrets", error = format!("{e:#}")).to_string(),
                        window,
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    /// Hoists literal credentials from a request being imported into `dir` into the owning
    /// collection's secret defaults and saves the collection file. The caller writes the
    /// request, then stores the returned secret values once that has succeeded.
    fn hoist_into_defaults(&self, dir: &Path, request: &mut RequestFile) -> Result<(Vec<String>, Vec<SecretWrite>)> {
        let Some(ix) = self.collection_index_for(dir) else {
            return Ok(Default::default());
        };
        let mut file = self.collections[ix].file.clone();
        let mut secrets = reserved_names(file.secrets.iter().cloned());
        let added = hoist_credentials_with(request, &mut secrets, &|name| file.variables.contains_key(name));
        if added.is_empty() {
            return Ok(Default::default());
        }
        file.ensure_id();
        file.secrets.extend(added.iter().cloned());
        storage::save_collection_file(&self.collections[ix].root, &file)?;
        let writes = added
            .iter()
            .map(|name| SecretWrite::new(&file, DEFAULTS_SCOPE, DEFAULTS_LABEL, name, secrets[name].clone()))
            .collect();
        Ok((added, writes))
    }

    /// Moves one header's literal credential into a secret in the active environment, or in
    /// the collection defaults when no environment is active.
    fn move_header_to_secret(&mut self, path: PathBuf, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| -> Result<(PathBuf, String, SecretWrite)> {
            let ix = self
                .collection_index_for(&path)
                .context(t!("ws.not_in_open_collection").to_string())?;
            let root = self.collections[ix].root.clone();
            let mut request: RequestFile = storage::read_yaml(&path)?;
            let mut file = self.collections[ix].file.clone();
            let environment = self
                .state
                .active_environments
                .get(&root)
                .and_then(|env_path| self.collections[ix].environment(env_path))
                .cloned();

            let (scope_names, scope_vars) = match &environment {
                Some(env) => (env.file.secrets.clone(), env.file.variables.clone()),
                None => (file.secrets.clone(), file.variables.clone()),
            };
            let mut secrets = reserved_names(scope_names.into_iter().chain(scope_vars.into_keys()));
            let name =
                hoist_header(&mut request, index, &mut secrets).context(t!("ws.no_literal_credential").to_string())?;
            let value = secrets[&name].clone();

            let assigned = file.ensure_id().1;
            let (scope, scope_label) = match environment {
                Some(mut env) => {
                    if assigned {
                        storage::save_collection_file(&root, &file)?;
                    }
                    env.file.secrets.push(name.clone());
                    storage::write_yaml(&env.path, &env.file)?;
                    (SecretRef::environment_scope(&env.path), env.file.name)
                }
                None => {
                    file.secrets.push(name.clone());
                    storage::save_collection_file(&root, &file)?;
                    (DEFAULTS_SCOPE.to_string(), t!("ws.defaults").to_string())
                }
            };
            storage::write_yaml(&path, &request)?;
            let write = SecretWrite::new(&file, &scope, &scope_label, &name, value);
            Ok((
                root,
                t!("ws.moved_to_secret", name = name, scope = scope_label).to_string(),
                write,
            ))
        })();

        match result {
            Ok((root, message, write)) => {
                self.store_secrets(vec![write], window, cx);
                self.reload_collection(&root, window, cx);
                self.load_in_editor(&path, window, cx);
                window.push_notification(Notification::success(message), cx);
            }
            Err(e) => notify_error(format!("{e:#}"), window, cx),
        }
    }

    // MARK: Rendering

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let weak = cx.entity().downgrade();
        let mut rows = Vec::new();
        for collection in &self.collections {
            self.render_collection(collection, &mut rows, cx);
        }

        v_flex()
            .w(px(280.))
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("ws.projects").to_string()),
                    )
                    .child(
                        Button::new("sidebar-add")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip(t!("ws.new_or_import").to_string())
                            .dropdown_menu(move |menu, _, _| {
                                menu.item(menu_item(t!("ws.open_project_ellipsis"), &weak, |this, window, cx| {
                                    this.open_project(window, cx)
                                }))
                                .separator()
                                .item(menu_item(
                                    t!("ws.new_collection_from_postman"),
                                    &weak,
                                    |this, window, cx| this.import_postman_as_new(window, cx),
                                ))
                            }),
                    ),
            )
            .child(
                v_flex()
                    .id("sidebar-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_1()
                    .children(rows),
            )
    }

    fn render_no_projects(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .size_full()
            .justify_center()
            .items_center()
            .gap_3()
            .child(div().text_lg().child(t!("ws.empty_title").to_string()))
            .child(
                div()
                    .max_w(px(460.))
                    .text_center()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("ws.empty_hint", dir = project::DOT_DIR).to_string()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("empty-open-project")
                            .primary()
                            .icon(IconName::FolderOpen)
                            .label(t!("ws.open_project_ellipsis").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.open_project(window, cx))),
                    )
                    .child(
                        Button::new("empty-import-postman")
                            .label(t!("ws.new_collection_from_postman").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.import_postman_as_new(window, cx))),
                    ),
            )
    }

    fn render_collection(&self, collection: &Collection, rows: &mut Vec<AnyElement>, cx: &mut Context<Self>) {
        let theme = cx.theme();
        let root = collection.root.clone();
        let collapsed = self.collapsed.contains(&root);
        let weak = cx.entity().downgrade();
        let row_id = rows.len();

        rows.push(
            h_flex()
                .id(("collection", row_id))
                .group("collection-row")
                .px_1()
                .py_1()
                .mt_1()
                .gap_1()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(theme.sidebar_accent))
                .child(chevron(collapsed))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(collection.file.name.clone()),
                )
                .when(!collection.errors.is_empty(), |this| {
                    this.child(Icon::new(IconName::TriangleAlert).xsmall().text_color(theme.warning))
                })
                .child({
                    let root = root.clone();
                    Button::new(("collection-menu", row_id))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Ellipsis)
                        .dropdown_menu(move |menu, _, _| collection_menu(menu, &weak, &root))
                })
                .on_click(cx.listener({
                    let root = root.clone();
                    move |this, _, _, cx| {
                        if !this.collapsed.remove(&root) {
                            this.collapsed.insert(root.clone());
                        }
                        cx.notify();
                    }
                }))
                .into_any_element(),
        );

        if !collapsed {
            self.render_items(&collection.items, 1, rows, cx);
        }
    }

    fn render_items(&self, items: &[Item], depth: usize, rows: &mut Vec<AnyElement>, cx: &mut Context<Self>) {
        let theme = cx.theme().clone();
        let selected = self.editor.read(cx).path().cloned();
        let indent = px(4. + depth as f32 * 14.);

        for item in items {
            let row_id = rows.len();
            match item {
                Item::Folder { name, path, children } => {
                    let collapsed = self.collapsed.contains(path);
                    let path = path.clone();
                    rows.push(
                        h_flex()
                            .id(("folder", row_id))
                            .pl(indent)
                            .pr_1()
                            .py_1()
                            .gap_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_sm()
                            .hover(|s| s.bg(theme.sidebar_accent))
                            .child(chevron(collapsed))
                            .child(
                                Icon::new(if collapsed {
                                    IconName::Folder
                                } else {
                                    IconName::FolderOpen
                                })
                                .xsmall()
                                .text_color(theme.muted_foreground),
                            )
                            .child(div().min_w_0().truncate().child(name.clone()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.collapsed.remove(&path) {
                                    this.collapsed.insert(path.clone());
                                }
                                cx.notify();
                            }))
                            .into_any_element(),
                    );
                    if !collapsed {
                        self.render_items(children, depth + 1, rows, cx);
                    }
                }
                Item::Request { path, request } => {
                    let is_selected = selected.as_ref() == Some(path);
                    let path = path.clone();
                    rows.push(
                        h_flex()
                            .id(("request", row_id))
                            .pl(indent + px(18.))
                            .pr_1()
                            .py_1()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .text_sm()
                            .when(is_selected, |s| {
                                s.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground)
                            })
                            .hover(|s| s.bg(theme.sidebar_accent))
                            .child(
                                div()
                                    .w(px(52.))
                                    .flex_none()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(method_color(&request_label(request), &theme))
                                    .child(short_method(&request_label(request))),
                            )
                            .child(div().min_w_0().truncate().child(request.name.clone()))
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.select_request(path.clone(), window, cx)),
                            )
                            .into_any_element(),
                    );
                }
            }
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let active = self
            .active_collection(cx)
            .map(|c| (c.file.name.clone(), c.root.clone()));
        let collection_name = active.as_ref().map(|(name, _)| name.clone()).unwrap_or_default();
        let managing = self.main_view == MainView::Environments;
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);

        h_flex()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &palette::OpenCommandPalette, window, cx| this.open_command_palette(window, cx)),
            )
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_sidebar(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        h_flex()
                            .px_3()
                            .py_2()
                            .gap_3()
                            .justify_end()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(collection_name),
                            )
                            .child(
                                Button::new("open-command-palette")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Search)
                                    .tooltip(t!("palette.open_tooltip").to_string())
                                    .on_click(cx.listener(|this, _, window, cx| this.open_command_palette(window, cx))),
                            )
                            .child(div().w(px(200.)).child(Select::new(&self.environment).small()))
                            .when_some(active.map(|(_, root)| root), |this, root| {
                                this.child(
                                    Button::new("manage-environments")
                                        .ghost()
                                        .small()
                                        .icon(IconName::Settings)
                                        .selected(managing)
                                        .tooltip(t!("ws.manage_environments").to_string())
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            if this.main_view == MainView::Environments {
                                                this.close_environments(window, cx);
                                            } else {
                                                this.manage_environments(root.clone(), window, cx);
                                            }
                                        })),
                                )
                            }),
                    )
                    .child(div().flex_1().min_h_0().child(if self.collections.is_empty() {
                        self.render_no_projects(cx).into_any_element()
                    } else if managing {
                        self.environment_editor.clone().into_any_element()
                    } else {
                        self.editor.clone().into_any_element()
                    })),
            )
            .children(dialog_layer)
            .children(notification_layer)
    }
}

// MARK: Helpers

fn menu_item(
    label: impl Into<SharedString>,
    weak: &WeakEntity<Workspace>,
    action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> PopupMenuItem {
    let weak = weak.clone();
    PopupMenuItem::new(label.into()).on_click(move |_, window, cx| {
        weak.update(cx, |this, cx| action(this, window, cx)).ok();
    })
}

/// A collection action, as used by the collection menu and the command palette.
pub(super) type RootAction = fn(&mut Workspace, PathBuf, &mut Window, &mut Context<Workspace>);

fn collection_menu(menu: PopupMenu, weak: &WeakEntity<Workspace>, root: &Path) -> PopupMenu {
    let item = |key: &str, action: RootAction| {
        let root = root.to_path_buf();
        menu_item(t!(key), weak, move |this, window, cx| {
            action(this, root.clone(), window, cx)
        })
    };
    menu.item(item("ws.new_request", Workspace::new_request))
        .item(item("ws.manage_environments_ellipsis", Workspace::manage_environments))
        .separator()
        .item(item("ws.import_curl", Workspace::import_curl_dialog))
        .item(item("ws.import_postman_here", Workspace::import_postman_into))
        .item(item(
            "ws.import_postman_environment",
            Workspace::import_postman_environment,
        ))
        .separator()
        .item(item("ws.show_in_file_manager", |_, root, _, cx| cx.reveal_path(&root)))
        .item(item("ws.reload_from_disk", |this, root, window, cx| {
            this.reload_collection(&root, window, cx)
        }))
        .item(item("ws.close_collection", |this, root, window, cx| {
            this.close_collection(&root, window, cx)
        }))
}

fn chevron(collapsed: bool) -> Icon {
    Icon::new(if collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    })
    .xsmall()
}

/// The sidebar label: the HTTP method, or WS / GQL for WebSocket and GraphQL requests.
fn request_label(request: &RequestFile) -> String {
    if request.graphql.is_some() {
        "GQL".into()
    } else if crate::model::is_websocket_url(&request.url) {
        "WS".into()
    } else {
        request.method.clone()
    }
}

fn short_method(method: &str) -> String {
    match method.to_ascii_uppercase().as_str() {
        "DELETE" => "DEL".into(),
        "OPTIONS" => "OPT".into(),
        other => other.into(),
    }
}

fn method_color(method: &str, theme: &gpui_kit::component::theme::Theme) -> Hsla {
    match method.to_ascii_uppercase().as_str() {
        "WS" | "GQL" => theme.magenta,
        "GET" => theme.success,
        "POST" => theme.warning,
        "PUT" | "PATCH" => theme.info,
        "DELETE" => theme.danger,
        _ => theme.muted_foreground,
    }
}

fn notify_error(message: String, window: &mut Window, cx: &mut App) {
    window.push_notification(Notification::error(message), cx);
}

fn notify_import(warnings: &[String], window: &mut Window, cx: &mut App) {
    let note = if warnings.is_empty() {
        Notification::success(t!("ws.postman_imported").to_string())
    } else {
        for warning in warnings {
            eprintln!("postman import: {warning}");
        }
        let shown: Vec<_> = warnings.iter().take(5).cloned().collect();
        let more = warnings.len().saturating_sub(shown.len());
        let mut message = shown.join("\n");
        if more > 0 {
            message.push('\n');
            message.push_str(&t!("ws.and_more", count = more));
        }
        Notification::warning(message).title(t!("ws.imported_with_warnings").to_string())
    };
    window.push_notification(note, cx);
}

/// Renames `{{old}}` to `{{new}}` everywhere in imported requests.
fn rename_placeholder(items: &mut [ImportItem], old: &str, new: &str) {
    let (from, to) = (placeholder(old), placeholder(new));
    ImportItem::for_each_request_mut(items, &mut |request| {
        request.url = request.url.replace(&from, &to);
        for header in &mut request.headers {
            header.value = header.value.replace(&from, &to);
        }
        if let Some(body) = &mut request.body {
            body.content = body.content.replace(&from, &to);
        }
    });
}

/// Every request of the collections at `roots`, re-read from disk so requests deleted
/// outside the app count as gone. Unreadable collections are skipped, so none of their
/// responses count as orphans.
fn liveness_from_disk(roots: &[PathBuf]) -> Liveness {
    let mut live = Liveness::default();
    for collection in roots.iter().filter_map(|root| storage::load_collection(root).ok()) {
        live.collection_ids.extend(collection.file.id.clone());
        live.keys.extend(
            collection
                .requests()
                .iter()
                .map(|entry| collection.response_key(entry.path).key),
        );
    }
    live
}

/// An example project for tests: `dir/example/.courier` with two requests and a
/// "Local" environment. Returns the collection root.
#[cfg(test)]
fn create_example_project(dir: &Path) -> Result<PathBuf> {
    let project = dir.join("example");
    fs::create_dir_all(&project)?;
    let mut file = crate::model::CollectionFile::new("Example");
    file.variables.insert("base_url".into(), "https://httpbin.org".into());
    let root = project::init_with(&project, &file)?;

    let mut env = crate::model::EnvironmentFile::new("Local");
    env.variables.insert("base_url".into(), "http://localhost:8080".into());
    storage::create_environment(&root, &env)?;

    let mut get = RequestFile::new("Get JSON");
    get.url = "{{base_url}}/json".into();
    get.order = Some(1);
    storage::create_request(&root, &get)?;

    let mut post = RequestFile::new("Echo POST");
    post.method = "POST".into();
    post.url = "{{base_url}}/anything".into();
    post.headers = crate::model::headers_from_text("Accept: application/json");
    post.body = Some(crate::model::Body {
        kind: crate::model::BodyKind::Json,
        content: "{\n  \"hello\": \"world\"\n}".into(),
    });
    post.order = Some(2);
    storage::create_request(&root, &post)?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::TestAppContext;
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};

    use super::*;
    use crate::environment_editor;
    // `gpui_kit::*` (via `super::*`) exports GPUI's test macro; keep Rust's for `#[test]`.
    #[allow(unused_imports)]
    use core::prelude::v1::test;

    /// Globals and key bindings for a UI test, with every app directory under `tmp`.
    fn setup(cx: &mut TestAppContext, tmp: &Path) -> AppPaths {
        let paths = AppPaths::under(tmp);
        // Requests run on the network runtime's threads, which wake the test's tasks.
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::request_editor::init(cx);
            environment_editor::init(cx);
            palette::init(cx);
            cx.set_global(AppSettings::load(&paths));
        });
        paths
    }

    fn open_workspace(
        cx: &mut TestAppContext,
        paths: &AppPaths,
        launch: Option<Launch>,
    ) -> (Entity<Workspace>, AnyWindowHandle) {
        let mut workspace = None;
        let handle = cx.open_window(size(px(1280.), px(900.)), |window, cx| {
            let view = cx.new(|cx| Workspace::new(paths.clone(), launch, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        (workspace.unwrap(), handle.into())
    }

    fn launch(root: &Path) -> Option<Launch> {
        Some(Launch {
            dir: project::project_dir(root).to_path_buf(),
            explicit: true,
        })
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// Drives the environment manager through a real window: edit the collection
    /// defaults with the keyboard, then create and delete an environment.
    #[gpui_kit::test]
    async fn manages_environments_end_to_end(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));

        // Open the manager from the toolbar button.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("manage-environments", cx);
        })
        .unwrap();
        cx.update(|cx| assert!(workspace.read(cx).main_view == MainView::Environments));

        // Local is the active environment? No: first run has none, so defaults are shown.
        // Type a new default variable at the end and save with Ctrl+S.
        let env_editor = cx.update(|cx| workspace.read(cx).environment_editor.clone());
        cx.update_window(window, |_, window, cx| {
            env_editor.read(cx).variables_focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
            window.press("ctrl-end", cx);
            window.input("\ntoken: abc123", cx);
            window.press("ctrl-s", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let collection_yaml = read(&root.join("collection.yaml"));
        assert!(collection_yaml.contains("token: abc123"), "{collection_yaml}");
        assert!(
            collection_yaml.find("base_url").unwrap() < collection_yaml.find("token").unwrap(),
            "new variable is appended, order kept:\n{collection_yaml}"
        );

        // Invalid text is not saved and shows an error.
        cx.update_window(window, |_, window, cx| {
            window.input("\nthis line is broken", cx);
            window.press("ctrl-s", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!read(&root.join("collection.yaml")).contains("broken"));
        cx.update(|cx| assert!(env_editor.read(cx).error().is_some()));
        cx.update_window(window, |_, window, cx| {
            // Undo the broken line so switching targets can save cleanly.
            for _ in 0.."\nthis line is broken".len() {
                window.press("backspace", cx);
            }
        })
        .unwrap();

        // Create an environment; it is written to disk and selected once reloaded.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("new-environment", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let new_env = root.join("environments/new-environment.yaml");
        assert!(read(&new_env).contains("name: New environment"));
        cx.wait_for(window, Duration::from_secs(1), |_, cx| {
            env_editor.read(cx).name_value(cx) == "New environment"
        })
        .await;

        // Delete it through the confirmation dialog: Cancel keeps it, the Delete button removes it.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("delete-environment", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("dialog-cancel", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(new_env.exists(), "cancel keeps the environment");
        cx.update_window(window, |_, window, cx| {
            assert!(!window.has_active_dialog(cx), "cancel closes the dialog");
            window.render_frame(cx);
            window.click("delete-environment", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("dialog-ok", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!new_env.exists(), "environment file should be deleted");
        cx.update(|cx| {
            let ws = workspace.read(cx);
            let collection = ws.collections.iter().find(|c| c.root == root).unwrap();
            assert_eq!(collection.environments.len(), 1, "only Local remains");
        });
    }

    /// Every file under `dir`, recursively.
    fn all_files(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(all_files(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    fn assert_not_on_disk(dir: &Path, needle: &str) {
        for file in all_files(dir) {
            let bytes = fs::read(&file).unwrap();
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "secret value found in plain text in {}",
                file.display()
            );
        }
    }

    fn stored_secret(cx: &mut TestAppContext, workspace: &Entity<Workspace>, secret: &SecretRef) -> Option<String> {
        cx.run_until_parked();
        let store = cx
            .update(|cx| workspace.read(cx).secret_store.clone())
            .expect("store connected");
        futures_lite::future::block_on(store.get(secret)).unwrap()
    }

    #[gpui_kit::test]
    async fn launching_in_a_folder_offers_to_create_a_collection(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let plain = tmp.path().join("plain");
        let einvoicing = tmp.path().join("einvoicing");
        fs::create_dir_all(&plain).unwrap();
        fs::create_dir_all(&einvoicing).unwrap();

        // Started from a folder that isn't a project (e.g. $HOME): empty state, no dialog.
        let (workspace, window) = open_workspace(
            cx,
            &paths,
            Some(Launch {
                dir: plain.clone(),
                explicit: false,
            }),
        );
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(!window.has_active_dialog(cx));
            assert!(workspace.read(cx).collections.is_empty());
            window.render_frame(cx);
            assert!(
                window.try_find("empty-open-project").is_some(),
                "empty state offers Open project"
            );
        })
        .unwrap();
        assert!(!plain.join(".courier").exists());

        // `courier ~/Work/einvoicing` offers to create the collection; Create makes and opens it.
        let (workspace, window) = open_workspace(
            cx,
            &paths,
            Some(Launch {
                dir: einvoicing.clone(),
                explicit: true,
            }),
        );
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(window.has_active_dialog(cx), "offers to create .courier");
            window.render_frame(cx);
            window.click("dialog-ok", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let root = einvoicing.join(".courier");
        assert!(read(&root.join("collection.yaml")).contains("name: einvoicing"));
        cx.update(|cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.collections.len(), 1);
            assert_eq!(
                ws.state.open_projects,
                vec![einvoicing.clone()],
                "remembered as a project"
            );
        });
        let state = read(&paths.state_dir.join("state.yaml"));
        assert!(state.contains("einvoicing"), "{state}");
    }

    #[gpui_kit::test]
    async fn command_palette_jumps_to_a_request(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let echo = root.join("echo-post.yaml");

        // Every label resolves to real text, not a missing translation key.
        cx.update(|cx| {
            for group in workspace.read(cx).palette_groups(cx) {
                assert!(!group.label.contains('.'), "untranslated group {}", group.label);
                for entry in group.entries {
                    for prefix in ["ws.", "palette.", "settings."] {
                        assert!(!entry.label.contains(prefix), "untranslated entry {}", entry.label);
                    }
                }
            }
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-shift-p", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(window.has_active_dialog(cx), "palette opened");
            window.render_frame(cx);
            window.input("echo post", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(!window.has_active_dialog(cx), "palette closed after confirming");
            assert_eq!(workspace.read(cx).editor.read(cx).path(), Some(&echo));
        })
        .unwrap();
    }

    /// Serves one HTTP exchange with a JSON `reply`, reporting the request head and body.
    fn http_server(reply: &'static str) -> (u16, std::sync::mpsc::Receiver<(String, String)>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{BufRead as _, Read as _, Write as _};
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let (mut head, mut length) = (String::new(), 0);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            write!(
                reader.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
            sent.send((head, String::from_utf8(body).unwrap())).unwrap();
        });
        (port, received)
    }

    /// Answers one HTTP request on a local port. Returns the port and a channel that
    /// receives the raw request text.
    fn one_shot_server(response: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            stream.write_all(response.as_bytes()).unwrap();
            let _ = sent.send(String::from_utf8_lossy(&request).into_owned());
        });
        (port, received)
    }

    #[gpui_kit::test]
    async fn responses_stay_with_their_request_and_survive_restarts(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let (get_json, echo) = (root.join("get-json.yaml"), root.join("echo-post.yaml"));
        let (port, _) = one_shot_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: session=SESSION_SECRET_42\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
        );
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.url = format!("http://127.0.0.1:{port}/json");
        storage::write_yaml(&get_json, &request).unwrap();

        // Send from "Get JSON", then switch to "Echo POST" before the response arrives.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.select_request(get_json.clone(), window, cx);
            });
            let headers = workspace.read(cx).editor.read(cx).headers_entity();
            headers.focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
            window.press("ctrl-enter", cx);
        })
        .unwrap();
        // Events from the key press are delivered when that update ends, so the send has
        // started (but not finished) before this switch.
        cx.update_window(window, |_, window, cx| {
            assert!(workspace.read(cx).editor.read(cx).is_sending(), "send started");
            workspace.update(cx, |this, cx| this.select_request(echo.clone(), window, cx));
        })
        .unwrap();
        for _ in 0..500 {
            cx.run_until_parked();
            if cx.update(|cx| workspace.read(cx).editor.read(cx).response_for(&get_json).is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cx.update(|cx| {
            let editor = workspace.read(cx).editor.read(cx);
            assert_eq!(editor.path(), Some(&echo));
            assert!(
                editor.shown_response().is_none(),
                "the response must not land on Echo POST"
            );
            let Some(response) = editor.response_for(&get_json) else {
                panic!("response lost")
            };
            assert!(matches!(
                response.outcome,
                crate::response_cache::Outcome::Response { status: 200, .. }
            ));
        });

        // Switching back shows it again.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.select_request(get_json.clone(), window, cx));
            let editor = workspace.read(cx).editor.read(cx);
            let (_, restored) = editor.shown_response().expect("shown after switching back");
            assert!(!restored);
        })
        .unwrap();

        // It was cached to disk with the session cookie masked.
        cx.run_until_parked();
        let cached: Vec<_> = all_files(&paths.cache_dir);
        assert_eq!(cached.len(), 1, "{cached:?}");
        assert_not_on_disk(tmp.path(), "SESSION_SECRET_42");

        // A fresh workspace (as after a restart) restores it, marked as restored.
        let (restarted, window) = open_workspace(cx, &paths, launch(&root));
        cx.update_window(window, |_, window, cx| {
            restarted.update(cx, |this, cx| this.select_request(get_json.clone(), window, cx));
            let editor = restarted.read(cx).editor.read(cx);
            let (response, restored) = editor.shown_response().expect("restored from cache");
            assert!(restored);
            let crate::response_cache::Outcome::Response { headers, .. } = &response.outcome else {
                panic!()
            };
            assert!(
                headers
                    .iter()
                    .any(|(n, v)| n.eq_ignore_ascii_case("set-cookie") && v == crate::response_cache::MASK)
            );
        })
        .unwrap();

        // Deleting a request file outside the app makes its saved response an orphan that the
        // background tidy removes; other requests keep theirs.
        let cache = ResponseCache::new(&paths.cache_dir);
        let echo_key = cx.update(|cx| restarted.read(cx).response_key(&echo).unwrap());
        cache
            .save(&echo_key, &crate::response_cache::StoredResponse::failed(0, "x"))
            .unwrap();
        fs::remove_file(&get_json).unwrap();
        let roots: Vec<_> = cx.update(|cx| restarted.read(cx).collections.iter().map(|c| c.root.clone()).collect());
        let live = liveness_from_disk(&roots);
        let later = std::time::SystemTime::now() + crate::response_cache::TIDY_GRACE + Duration::from_secs(1);
        let report = cache.tidy(&live, later).unwrap();
        assert_eq!(report, crate::response_cache::TidyReport { removed: 1, kept: 1 });
        assert!(cache.load(&echo_key).is_some());

        // Turning the setting off clears the cache.
        cx.update_window(window, |_, window, cx| {
            restarted.update(cx, |this, cx| this.set_remember_responses(false, window, cx));
        })
        .unwrap();
        assert!(all_files(&paths.cache_dir).is_empty());
    }

    #[gpui_kit::test]
    async fn cancel_stops_a_slow_response(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");

        // A server that sends headers, then trickles the body until the client goes away.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap();
            loop {
                if stream.write_all(b"1\r\nx\r\n").and_then(|_| stream.flush()).is_err() {
                    closed_tx.send(()).unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.url = format!("http://127.0.0.1:{port}/slow");
        storage::write_yaml(&get_json, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.select_request(get_json.clone(), window, cx);
            });
            window.render_frame(cx);
            window.click("send", cx);
        })
        .unwrap();
        // Wait until bytes are arriving.
        for _ in 0..200 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
            if cx.update(|cx| workspace.read(cx).editor.read(cx).received_bytes() > 3) {
                break;
            }
        }
        cx.update_window(window, |_, window, cx| {
            assert!(workspace.read(cx).editor.read(cx).is_sending());
            assert!(
                workspace.read(cx).editor.read(cx).received_bytes() > 3,
                "body is streaming in"
            );
            window.render_frame(cx);
            window.click("send", cx); // now labelled Cancel
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let editor = workspace.read(cx).editor.read(cx);
            assert!(!editor.is_sending(), "cancelled");
            assert!(editor.shown_response().is_none(), "a cancelled send leaves no response");
        });
        assert!(
            closed_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the connection was closed"
        );
    }

    #[gpui_kit::test]
    async fn event_streams_show_live_events_and_reconnect(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");

        // Two connections: the first sends two events and closes; the reconnect must carry
        // Last-Event-ID and gets one more event.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (resumed_tx, resumed_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for connection in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).unwrap();
                    head.extend_from_slice(&buf[..n]);
                }
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                    .unwrap();
                if connection == 0 {
                    stream
                        .write_all(
                            b"id: 1\nevent: price\ndata: {\"usd\": 10}\n\nid: 2\nevent: price\ndata: {\"usd\": 11}\n\n",
                        )
                        .unwrap();
                } else {
                    resumed_tx.send(head.contains("last-event-id: 2")).unwrap();
                    stream
                        .write_all(b"id: 3\nevent: price\ndata: {\"usd\": 12}\n\n")
                        .unwrap();
                }
            }
        });
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.url = format!("http://127.0.0.1:{port}/prices");
        storage::write_yaml(&get_json, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor.clone());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.select_request(get_json.clone(), window, cx);
            });
            window.render_frame(cx);
            window.click("send", cx);
        })
        .unwrap();
        let wait_until = |cx: &mut TestAppContext, done: &dyn Fn(&RequestEditor) -> bool| {
            for _ in 0..300 {
                cx.run_until_parked();
                if cx.update(|cx| done(editor.read(cx))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out waiting");
        };
        wait_until(cx, &|e| {
            e.sse_summary().is_some_and(|(count, ended)| count == 2 && ended)
        });

        // Selecting an event shows its data, pretty-printed.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(("sse-event", 0usize), cx);
        })
        .unwrap();
        cx.update(|cx| {
            assert!(
                editor.read(cx).stream_detail_text(cx).contains("\"usd\": 10"),
                "detail shows the data"
            )
        });

        // Reconnect resumes with Last-Event-ID and keeps earlier events.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("sse-reconnect", cx);
        })
        .unwrap();
        wait_until(cx, &|e| {
            e.sse_summary().is_some_and(|(count, ended)| count == 3 && ended)
        });
        assert!(
            resumed_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "reconnect sent Last-Event-ID: 2"
        );
        cx.update(|cx| {
            assert!(
                editor.read(cx).shown_response().is_none(),
                "streams aren't stored as responses"
            )
        });
    }

    #[gpui_kit::test]
    #[allow(clippy::result_large_err)] // tungstenite's handshake callback signature
    async fn websockets_connect_send_receive_and_disconnect(cx: &mut TestAppContext) {
        use tokio_tungstenite::tungstenite::{Message, accept_hdr};

        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");

        // An echo server that also reports the handshake's Authorization header.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (auth_tx, auth_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept_hdr(
                stream,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    let auth = request
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default();
                    auth_tx.send(auth.to_string()).unwrap();
                    Ok(response)
                },
            )
            .unwrap();
            loop {
                match socket.read() {
                    Ok(Message::Text(text)) => socket
                        .send(Message::Text(format!("echo: {}", text.as_str()).into()))
                        .unwrap(),
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.url = format!("ws://127.0.0.1:{port}/live");
        request.headers = crate::model::headers_from_text("Authorization: Bearer abc");
        request.body = Some(crate::model::Body {
            kind: crate::model::BodyKind::Json,
            content: "{\"hello\": \"{{base_url}}\"}".into(),
        });
        storage::write_yaml(&get_json, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor.clone());
        let wait_until = |cx: &mut TestAppContext, done: &dyn Fn(&RequestEditor) -> bool| {
            for _ in 0..300 {
                cx.run_until_parked();
                if cx.update(|cx| done(editor.read(cx))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out waiting");
        };
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.select_request(get_json.clone(), window, cx);
            });
            window.render_frame(cx);
            window.click("send", cx); // Connect
        })
        .unwrap();
        wait_until(cx, &|e| e.ws_summary().is_some_and(|(_, connected, _)| connected));
        assert_eq!(auth_rx.recv_timeout(Duration::from_secs(5)).unwrap(), "Bearer abc");

        // Send the composer's message; variables resolve before it goes out.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("ws-send-message", cx);
        })
        .unwrap();
        wait_until(cx, &|e| e.ws_summary().is_some_and(|(count, _, _)| count == 2));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(("ws-message", 1usize), cx); // the echo, newest first
        })
        .unwrap();
        cx.update(|cx| {
            let detail = editor.read(cx).stream_detail_text(cx);
            assert!(
                detail.contains("echo: {\"hello\": \"https://httpbin.org\"}"),
                "{detail}"
            );
        });

        // Save the message as a template; it lands in the request file.
        cx.update(|cx| editor.update(cx, |editor, cx| editor.add_template_for_test("Hello", cx)));
        cx.run_until_parked();
        let saved: RequestFile = storage::read_yaml(&get_json).unwrap();
        assert_eq!(saved.messages.len(), 1);
        assert_eq!(saved.messages[0].name, "Hello");

        // Disconnect closes gracefully and records the close.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("send", cx); // Disconnect
        })
        .unwrap();
        wait_until(cx, &|e| {
            e.ws_summary().is_some_and(|(_, connected, ended)| !connected && ended)
        });
        cx.update(|cx| assert!(!editor.read(cx).is_sending()));
    }

    #[gpui_kit::test]
    async fn graphql_requests_post_query_and_variables(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");

        let (port, received) = http_server("{\"data\":{\"pets\":[]}}");
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.method = "POST".into();
        request.url = format!("http://127.0.0.1:{port}/graphql");
        request.body = None;
        request.graphql = Some(crate::model::Graphql {
            query: "query Pets($first: Int) { pets(first: $first) { id } }".into(),
            variables: "{\"first\": 2}".into(),
            operation_name: Some("Pets".into()),
        });
        storage::write_yaml(&get_json, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor.clone());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.select_request(get_json.clone(), window, cx);
            });
            window.render_frame(cx);
            window.click("send", cx);
        })
        .unwrap();
        for _ in 0..300 {
            cx.run_until_parked();
            if cx.update(|cx| editor.read(cx).shown_response().is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (head, body) = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.starts_with("POST /graphql "), "{head}");
        assert!(
            head.to_ascii_lowercase().contains("content-type: application/json"),
            "{head}"
        );
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["query"], "query Pets($first: Int) { pets(first: $first) { id } }");
        assert_eq!(body["variables"]["first"], 2);
        assert_eq!(body["operationName"], "Pets");
        cx.update(|cx| {
            let (response, _) = editor.read(cx).shown_response().expect("response shown");
            assert!(matches!(
                response.outcome,
                crate::response_cache::Outcome::Response { status: 200, .. }
            ));
        });

        // The editor reproduces the GraphQL section exactly, so loading it isn't an edit.
        assert!(cx.update(|cx| !editor.read(cx).is_modified_for_test(cx)));
    }

    #[gpui_kit::test]
    async fn graphql_schemas_fetch_browse_complete_and_persist(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");
        let (port, received) = http_server(crate::graphql::tests::PETSTORE);
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.method = "POST".into();
        request.url = format!("http://127.0.0.1:{port}/graphql");
        request.headers = crate::model::headers_from_text("X-Api: demo");
        request.body = None;
        request.graphql = Some(crate::model::Graphql {
            query: "{ pets { id } }".into(),
            ..Default::default()
        });
        storage::write_yaml(&get_json, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor.clone());
        let wait_until = |cx: &mut TestAppContext, done: &dyn Fn(&App) -> bool| {
            for _ in 0..300 {
                cx.run_until_parked();
                if cx.update(|cx| done(cx)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out waiting");
        };
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.select_request(get_json.clone(), window, cx);
            });
            editor.update(cx, |editor, cx| editor.show_schema_tab_for_test(cx));
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| editor.read(cx).schema_type_count(cx)),
            None,
            "nothing cached yet"
        );
        assert!(cx.update(|cx| editor.read(cx).complete_for_test("{ pe", 4)).is_empty());

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("graphql-fetch-schema", cx);
        })
        .unwrap();
        wait_until(cx, &|cx| editor.read(cx).schema_type_count(cx).is_some());
        let (head, body) = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            head.to_ascii_lowercase().contains("x-api: demo"),
            "sent with the request's headers: {head}"
        );
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["operationName"], "IntrospectionQuery");
        assert_eq!(cx.update(|cx| editor.read(cx).schema_type_count(cx)), Some(13));
        assert_eq!(
            cx.update(|cx| editor.read(cx).complete_for_test("{ pe", 4)),
            ["pets", "pet"]
        );
        assert_eq!(cx.update(|cx| editor.read(cx).query_problems_for_test(cx)), []);

        // Mistakes are underlined as you type, and cleared once fixed.
        let query = cx.update(|cx| editor.read(cx).graphql_query_for_test());
        cx.update_window(window, |_, window, cx| {
            query.update(cx, |s, cx| s.replace_all("{ pets { id nmae } }", window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| editor.read(cx).query_problems_for_test(cx)),
            [(
                "Cannot query field `nmae` on type `Pet`".to_string(),
                "nmae".to_string()
            )]
        );
        cx.update_window(window, |_, window, cx| {
            query.update(cx, |s, cx| s.replace_all("{ pets { id name } }", window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| editor.read(cx).query_problems_for_test(cx)), []);

        // The first row is the query root; opening it lists its fields.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(("schema-row", 0usize), cx);
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            assert_eq!(editor.read(cx).schema_nav_for_test(), ["Query"]);
            window.render_frame(cx);
            window.click(("schema-row", 0usize), cx); // pets: [Pet!]!
        })
        .unwrap();
        cx.update(|cx| assert_eq!(editor.read(cx).schema_nav_for_test(), ["Query", "Pet"]));

        // After a restart the schema comes from the cache without fetching again.
        let (restarted, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| restarted.read(cx).editor.clone());
        cx.update_window(window, |_, window, cx| {
            restarted.update(cx, |this, cx| this.select_request(get_json.clone(), window, cx));
        })
        .unwrap();
        wait_until(cx, &|cx| editor.read(cx).schema_type_count(cx).is_some());
        assert_eq!(
            cx.update(|cx| editor.read(cx).complete_for_test("{ pets(species: ", 16)),
            ["DOG", "CAT", "FERRET"]
        );
    }

    #[gpui_kit::test]
    async fn secrets_never_reach_collection_files(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        cx.run_until_parked();
        cx.update(|cx| assert!(workspace.read(cx).secret_store.is_some(), "store connected"));

        // 1. Add a secret to the collection defaults through the environment manager.
        let value = "sk_live_TESTVALUE_0123456789";
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("manage-environments", cx);
        })
        .unwrap();
        let env_editor = cx.update(|cx| workspace.read(cx).environment_editor.clone());
        cx.update_window(window, |_, window, cx| {
            env_editor.read(cx).new_secret_focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
            window.input("api_token", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            // Adding a secret focuses its value input.
            window.input(value, cx);
            window.press("ctrl-s", cx);
        })
        .unwrap();
        cx.run_until_parked();

        let collection_yaml = read(&root.join("collection.yaml"));
        assert!(collection_yaml.contains("secrets:\n- api_token"), "{collection_yaml}");
        let id = storage::load_collection(&root)
            .unwrap()
            .file
            .id
            .expect("collection got an id");
        let secret = SecretRef::new(&id, DEFAULTS_SCOPE, "api_token");
        assert_eq!(stored_secret(cx, &workspace, &secret).as_deref(), Some(value));
        assert_not_on_disk(tmp.path(), value);
        cx.update(|cx| {
            let ws = workspace.read(cx);
            assert!(
                ws.editor.read(cx).secret_names().contains(&"api_token".to_string()),
                "requests can use it"
            );
        });

        // 2. A literal bearer token typed into a request header moves to a secret with one click.
        cx.update_window(window, |_, window, cx| {
            window.click("close-environments", cx);
        })
        .unwrap();
        let token = "eyJhbGciOiJIUzI1NiJ9.TESTTOKEN.signature";
        let request_path = root.join("get-json.yaml");
        let headers = cx.update(|cx| workspace.read(cx).editor.read(cx).headers_entity());
        cx.update_window(window, |_, window, cx| {
            headers.focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
            window.input(&format!("Authorization: Bearer {token}"), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(("move-header-to-secret", 0usize), cx);
        })
        .unwrap();
        cx.run_until_parked();

        let request_yaml = read(&request_path);
        assert!(request_yaml.contains("Bearer {{bearer_token}}"), "{request_yaml}");
        assert!(read(&root.join("collection.yaml")).contains("- bearer_token"));
        let bearer = SecretRef::new(&id, DEFAULTS_SCOPE, "bearer_token");
        assert_eq!(stored_secret(cx, &workspace, &bearer).as_deref(), Some(token));
        assert_not_on_disk(tmp.path(), token);

        // 3. Sending resolves the placeholder from the store: the real token goes on the wire.
        let (port, received) = one_shot_server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        let mut request: RequestFile = storage::read_yaml(&request_path).unwrap();
        request.url = format!("http://127.0.0.1:{port}/json");
        storage::write_yaml(&request_path, &request).unwrap();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                let request = this.find_request(&request_path).cloned().unwrap();
                let key = this.response_key(&request_path);
                this.editor.update(cx, |editor, cx| {
                    editor.load(request_path.clone(), request, key, window, cx)
                });
            });
            headers.focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
            window.press("ctrl-enter", cx);
        })
        .unwrap();
        let mut wire = None;
        for _ in 0..500 {
            cx.run_until_parked();
            if let Ok(request) = received.try_recv() {
                wire = Some(request);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let wire = wire.expect("request reached the local server");
        assert!(
            wire.lines()
                .any(|l| l.eq_ignore_ascii_case(&format!("authorization: Bearer {token}"))),
            "real token sent:\n{wire}"
        );
    }
}
