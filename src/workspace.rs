//! The window's main view: collection sidebar, environment picker, and request editor.

pub mod palette;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent as TextInputEvent, InputState, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, IndexPath, Root, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::auth_form::{AuthForm, AuthFormEvent};
use crate::chain;
use crate::cookies::Cookies;
use crate::credentials::{hoist_credentials_with, hoist_header, reserved_names, unique_name};
use crate::environment_editor::{EnvironmentEditor, EnvironmentEditorEvent, Target};
use crate::graphql::SchemaCache;
use crate::import::{self, CollectionImport, ImportFormat, ImportItem};
use crate::import::{curl, postman};
use crate::model::{Auth, CollectionFile, RequestFile, RequestKind, RequestSettings, Variables, placeholder};
use crate::paths::{AppPaths, AppState};
use crate::project;
use crate::request_editor::{RequestEditor, RequestEditorEvent};
use crate::response_cache::{CacheKey, Liveness, ResponseCache};
use crate::runner;
use crate::runner_view::{RunSetup, RunnerView, RunnerViewEvent, Scope};
use crate::secret_store::{self, DEFAULTS_LABEL, DEFAULTS_SCOPE, SecretRef, SecretStore, SecretWrite};
use crate::settings::{AppSettings, LabelColor};
use crate::settings_form::{PathField, SettingsForm, SettingsFormEvent};
use crate::storage::{self, Collection, Item};
use crate::ui::{dialog_footer, focus_in_dialog, text_input, textarea};

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
    Runner,
}

const SIDEBAR_WIDTH: f32 = 280.;
const SIDEBAR_MIN_WIDTH: f32 = 180.;
/// Room always left for the request editor when widening the sidebar.
const MAIN_MIN_WIDTH: f32 = 420.;

/// A request being dragged in the sidebar, and what it looks like while dragging.
#[derive(Clone)]
struct DraggedRequest {
    path: PathBuf,
    label: SharedString,
}

impl Render for DraggedRequest {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .text_sm()
            .text_color(theme.popover_foreground)
            .child(self.label.clone())
    }
}

/// Dragged while resizing the sidebar.
struct SidebarResize;

impl Render for SidebarResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

pub struct Workspace {
    paths: AppPaths,
    /// The always-open collection for quick requests, kept in the data dir, not a project.
    scratch_root: PathBuf,
    state: AppState,
    collections: Vec<Collection>,
    collapsed: HashSet<PathBuf>,
    request_editor: Entity<RequestEditor>,
    /// Open requests, in tab order; `active` is the one the editor is showing.
    open_tabs: Vec<PathBuf>,
    active: usize,
    environment: Entity<EnvironmentSelect>,
    /// Filters the sidebar to matching requests while it has text.
    search: Entity<InputState>,
    environment_editor: Entity<EnvironmentEditor>,
    runner: Entity<RunnerView>,
    main_view: MainView,
    focus_handle: FocusHandle,
    secret_store: Option<SecretStore>,
    /// Cookie jars of collections, by root, opened when first needed.
    cookies: HashMap<PathBuf, Cookies>,
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

        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("ws.search_placeholder").to_string())
                .clean_on_escape()
        });
        cx.subscribe(&search, |_, _, event: &TextInputEvent, cx| {
            if let TextInputEvent::Change = event {
                cx.notify();
            }
        })
        .detach();

        Self::watch_editor(&editor, window, cx);
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
        let runner = cx.new(|_| RunnerView::new());
        cx.subscribe_in(&runner, window, |this, _, event, window, cx| match event {
            RunnerViewEvent::Open(path) => this.select_request(path.clone(), window, cx),
            RunnerViewEvent::Ran(responses) => this
                .editor()
                .update(cx, |editor, cx| editor.take_chained(responses.clone(), cx)),
            RunnerViewEvent::RunAgain(scope) => this.run_scope(scope.path.clone(), window, cx),
            RunnerViewEvent::Close => {
                this.main_view = MainView::Request;
                cx.notify();
            }
        })
        .detach();
        cx.subscribe_in(&environment, window, |this, select, _: &SelectEvent<_>, window, cx| {
            let index = select.read(cx).selected_index(cx).map(|ix| ix.row);
            this.choose_environment(index, window, cx);
        })
        .detach();

        let mut this = Self {
            scratch_root: project::collection_dir(&paths.data_dir.join("scratchpad")),
            state: paths.load_state(),
            paths,
            collections: Vec::new(),
            collapsed: HashSet::new(),
            request_editor: editor,
            open_tabs: Vec::new(),
            active: 0,
            environment,
            search,
            environment_editor,
            runner,
            main_view: MainView::Request,
            focus_handle: cx.focus_handle(),
            secret_store: None,
            cookies: HashMap::new(),
        };
        this.connect_secret_store(window, cx);
        this.start_response_tidy(cx);
        this.restore(launch, window, cx);
        this.focus_handle.focus(window, cx);
        this
    }

    /// Reopens last session's projects, then the project the app was launched for.
    fn restore(&mut self, launch: Option<Launch>, window: &mut Window, cx: &mut Context<Self>) {
        match self.load_scratchpad() {
            Ok(scratch) => self.collections.push(scratch),
            Err(e) => eprintln!("could not open the scratchpad: {e:#}"),
        }
        for project in self.state.open_projects.clone() {
            match storage::load_collection(&project::collection_dir(&project)) {
                Ok(collection) => self.collections.push(collection),
                Err(e) => eprintln!("skipping project {}: {e:#}", project.display()),
            }
        }
        self.state.open_projects = self
            .projects()
            .map(|c| project::project_dir(&c.root).to_path_buf())
            .collect();
        self.save_state();

        // The tabs from last time, minus anything that's since been deleted or renamed.
        self.open_tabs = self
            .state
            .open_tabs
            .clone()
            .into_iter()
            .filter(|path| self.find_request(path).is_some())
            .collect();
        let last = self
            .state
            .last_request
            .clone()
            .filter(|p| self.find_request(p).is_some());
        let first = || self.projects().find_map(Collection::first_request);
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
                        weak.update(cx, |this, cx| this.offer_init_project(launch.dir, None, window, cx))
                            .ok();
                    });
                }
                None => {}
            }
        }
    }

    /// The sidebar width, kept within the window so the editor always has room.
    fn sidebar_width(&self, window: &Window) -> Pixels {
        let max = (f32::from(window.viewport_size().width) - MAIN_MIN_WIDTH).max(SIDEBAR_MIN_WIDTH);
        px(self
            .state
            .sidebar_width
            .unwrap_or(SIDEBAR_WIDTH)
            .clamp(SIDEBAR_MIN_WIDTH, max))
    }

    fn resize_sidebar(&mut self, x: Pixels, window: &Window, cx: &mut Context<Self>) {
        self.state.sidebar_width = Some(f32::from(x).round());
        let clamped = f32::from(self.sidebar_width(window));
        self.state.sidebar_width = Some(clamped);
        cx.notify();
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
    /// The scratchpad collection, created the first time it's needed.
    fn load_scratchpad(&self) -> Result<Collection> {
        if !storage::is_collection(&self.scratch_root) {
            let dir = project::project_dir(&self.scratch_root);
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            project::init_with(dir, &CollectionFile::new("Scratchpad"))?;
        }
        storage::load_collection(&self.scratch_root)
    }

    fn is_scratch(&self, root: &Path) -> bool {
        root == self.scratch_root
    }

    /// Open project collections, without the scratchpad.
    fn projects(&self) -> impl Iterator<Item = &Collection> {
        self.collections.iter().filter(|c| !self.is_scratch(&c.root))
    }

    /// A collection's name as shown: the scratchpad's is translated.
    fn collection_label(&self, collection: &Collection) -> String {
        if self.is_scratch(&collection.root) {
            t!("ws.scratchpad").to_string()
        } else {
            collection.file.name.clone()
        }
    }

    fn set_request_color(&mut self, kind: RequestKind, color: LabelColor, cx: &mut Context<Self>) {
        AppSettings::update(cx, |settings| settings.request_colors.set(kind, color));
        cx.notify();
    }

    fn new_scratch_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_scratch_request_of(RequestKind::Http, window, cx);
    }

    fn new_scratch_request_of(&mut self, kind: RequestKind, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.scratch_root.clone();
        if !self.collections.iter().any(|c| c.root == root) {
            match self.load_scratchpad() {
                Ok(scratch) => self.collections.insert(0, scratch),
                Err(e) => return notify_error(format!("{e:#}"), window, cx),
            }
        }
        self.new_request_of(root, kind, window, cx);
    }

    /// Moves a request into another collection's top level, e.g. from the scratchpad into a
    /// project.
    fn move_request(&mut self, path: PathBuf, destination: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.editor().update(cx, |editor, cx| editor.save(cx));
        match storage::move_request(&path, &destination) {
            Ok(target) => {
                self.collapsed.remove(&destination);
                self.after_move(&path, &target, window, cx);
                if let Some(ix) = self.collections.iter().position(|c| c.root == destination) {
                    let name = self.collection_label(&self.collections[ix]);
                    window.push_notification(
                        Notification::success(t!("ws.moved_to", collection = name).to_string()),
                        cx,
                    );
                }
            }
            Err(e) => notify_error(format!("{e:#}"), window, cx),
        }
    }

    /// Dropping a dragged request: into `folder`, straight after `after` when it was
    /// dropped on a request. Ordering is saved, so it survives a reload.
    fn drop_request(
        &mut self,
        path: PathBuf,
        folder: PathBuf,
        after: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if after.as_deref() == Some(path.as_path()) {
            return;
        }
        self.editor().update(cx, |editor, cx| editor.save(cx));
        match storage::place_request(&path, &folder, after.as_deref()) {
            Ok(target) => {
                self.collapsed.remove(&folder);
                if target != path {
                    self.after_move(&path, &target, window, cx);
                } else {
                    self.reload_containing(&target, window, cx);
                }
            }
            Err(e) => notify_error(format!("{e:#}"), window, cx),
        }
    }

    fn active_collection(&self, cx: &App) -> Option<&Collection> {
        let path = match self.main_view {
            MainView::Environments => self.environment_editor.read(cx).root().map(Path::to_path_buf),
            MainView::Request => self.editor().read(cx).path().cloned(),
            MainView::Runner => self.runner.read(cx).scope().map(|scope| scope.root.clone()),
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
                .editor()
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
                if !self.is_scratch(&root) {
                    self.state.open_projects.push(project::project_dir(&root).to_path_buf());
                }
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
                if let Some(open) = self.editor().read(cx).path().cloned()
                    && open.starts_with(root)
                {
                    let requests = self.request_index(&open);
                    self.editor()
                        .update(cx, |editor, _| editor.set_collection_requests(requests));
                }
            }
            Err(e) => notify_error(format!("{e:#}"), window, cx),
        }
        self.refresh_environments(window, cx);
        cx.notify();
    }

    /// (name, path) of every request in the collection holding `path`.
    fn request_index(&self, path: &Path) -> Vec<(String, PathBuf)> {
        self.collection_index_for(path)
            .map(|ix| {
                self.collections[ix]
                    .requests()
                    .into_iter()
                    .map(|entry| (entry.request.name.clone(), entry.path.to_path_buf()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn reload_containing(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.collection_index_for(path) {
            let root = self.collections[ix].root.clone();
            self.reload_collection(&root, window, cx);
        }
    }

    fn close_collection(&mut self, root: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_scratch(root) {
            return;
        }
        self.editor().update(cx, |editor, cx| {
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
        // Read the file itself: the sidebar's copy may not have caught up with a save that
        // just happened (e.g. saving one request and opening another in the same update).
        let fresh = storage::read_yaml::<RequestFile>(path).ok();
        if let Some(request) = fresh.or_else(|| self.find_request(path).cloned()) {
            let key = self.response_key(path);
            let jar = self.cookies_for(path, cx);
            let root = self
                .collection_index_for(path)
                .map(|ix| self.collections[ix].root.clone());
            let requests = self.request_index(path);
            let inherited_settings = root
                .as_deref()
                .map(|root| storage::inherited_settings(root, path))
                .unwrap_or_default();
            self.editor().update(cx, |editor, cx| {
                editor.set_cookies(jar);
                editor.set_collection_root(root);
                editor.set_collection_requests(requests);
                editor.set_inherited_settings(inherited_settings);
                editor.load(path.to_path_buf(), request, key, window, cx)
            });
            self.refresh_inherited_auth(cx);
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
        self.editor().update(cx, |editor, cx| editor.save(cx));
        self.open_tab(&path);
        self.main_view = MainView::Request;
        self.load_in_editor(&path, window, cx);
        self.state.last_request = Some(path);
        self.state.open_tabs = self.open_tabs.clone();
        self.save_state();
        self.refresh_environments(window, cx);
        cx.notify();
    }

    // MARK: Runs

    /// Runs a collection or a folder, showing the results as they arrive.
    fn run_scope(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.main_view == MainView::Environments && !self.close_environments(window, cx) {
            return;
        }
        // The run reads requests from disk, so save what's being edited first.
        self.editor().update(cx, |editor, cx| editor.save(cx));
        let Some(setup) = self.run_setup(path, cx) else {
            return;
        };
        self.runner.update(cx, |runner, cx| runner.start(setup, cx));
        self.main_view = MainView::Runner;
        self.refresh_environments(window, cx);
        cx.notify();
    }

    /// The requests under `path`, the collection's variables and secrets, and a chaining
    /// context sharing the collection's cookie jar.
    fn run_setup(&mut self, path: PathBuf, cx: &mut Context<Self>) -> Option<RunSetup> {
        let index = self.collection_index_for(&path)?;
        let collection = &self.collections[index];
        let root = collection.root.clone();
        let title = if path == root {
            collection.file.name.clone()
        } else {
            path.file_name()?.to_string_lossy().to_string()
        };
        let requests = runner::requests_in(collection, &path);
        let collection_id = collection.file.id.clone();
        let environment = self
            .state
            .active_environments
            .get(&root)
            .and_then(|active| collection.environments.iter().find(|env| &env.path == active))
            .map(|env| (env.path.as_path(), &env.file));
        let layered = secret_store::layer(&collection.file, environment);
        let cookies = self.cookies_for(&root, cx);
        Some(RunSetup {
            scope: Scope {
                root: root.clone(),
                path,
                title,
            },
            requests,
            context: chain::Context {
                root,
                variables: layered.variables,
                latest: Default::default(),
                cache: self.editor().read(cx).shared_cache(cx),
                collection_id,
                default_timeout_secs: AppSettings::get(cx).request_timeout_secs,
                cookies: cookies.as_ref().map(|jar| jar.store().clone()),
            },
            secrets: layered.secrets,
            store: self.secret_store.clone(),
        })
    }

    // MARK: Tabs

    /// The request editor. One editor serves every tab: it already keeps responses and
    /// filters per request, and switching tabs saves what was open.
    fn editor(&self) -> Entity<RequestEditor> {
        self.request_editor.clone()
    }

    fn watch_editor(editor: &Entity<RequestEditor>, window: &mut Window, cx: &mut Context<Self>) {
        cx.subscribe_in(editor, window, |this, _, event, window, cx| match event {
            RequestEditorEvent::Saved(path) => this.reload_containing(path, window, cx),
            RequestEditorEvent::MoveHeaderToSecret { path, index } => {
                this.move_header_to_secret(path.clone(), *index, window, cx)
            }
            RequestEditorEvent::MoveAuthToSecret { path } => this.move_auth_to_secret(path.clone(), window, cx),
            RequestEditorEvent::Error(message) => notify_error(message.clone(), window, cx),
            RequestEditorEvent::Notice(message) => window.push_notification(Notification::success(message.clone()), cx),
            RequestEditorEvent::EditSettings(path) => this.edit_settings(path.clone(), window, cx),
            RequestEditorEvent::PickUploadFile => this.pick_upload_file(window, cx),
        })
        .detach();
    }

    /// Gives `path` a tab, or moves to the one it already has.
    fn open_tab(&mut self, path: &Path) {
        match self.open_tabs.iter().position(|open| open == path) {
            Some(index) => self.active = index,
            None => {
                self.open_tabs.push(path.to_path_buf());
                self.active = self.open_tabs.len() - 1;
            }
        }
    }

    /// Closes a tab, showing its neighbour, or nothing when it was the last.
    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.open_tabs.len() {
            return;
        }
        let closing_open = self.editor().read(cx).path() == Some(&self.open_tabs[index]);
        self.editor().update(cx, |editor, cx| editor.save(cx));
        self.open_tabs.remove(index);
        self.active = self
            .active
            .saturating_sub((self.active >= index && self.active > 0) as usize);
        if closing_open {
            self.show_active_tab(window, cx);
        }
        cx.notify();
    }

    /// Closes the tabs for anything at or inside `path`, for a delete or a move away.
    fn close_tabs_under(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let was_open = self.editor().read(cx).path().is_some_and(|open| open.starts_with(path));
        self.open_tabs.retain(|tab| !tab.starts_with(path));
        self.active = self.active.min(self.open_tabs.len().saturating_sub(1));
        if was_open {
            self.show_active_tab(window, cx);
        }
        cx.notify();
    }

    /// Loads whatever the active tab points at, emptying the editor when there is none.
    fn show_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.open_tabs.get(self.active).cloned() {
            Some(path) => {
                self.load_in_editor(&path, window, cx);
                self.state.last_request = Some(path);
            }
            None => {
                self.editor().update(cx, |editor, cx| editor.unload(window, cx));
                self.state.last_request = None;
            }
        }
        self.state.open_tabs = self.open_tabs.clone();
        self.save_state();
        self.refresh_environments(window, cx);
    }

    /// Moves to the next or previous tab, wrapping around.
    fn step_tab(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.open_tabs.len();
        if count < 2 {
            return;
        }
        let next = (self.active as isize + by).rem_euclid(count as isize) as usize;
        self.select_tab(next, window, cx);
    }

    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.open_tabs.get(index).cloned() else {
            return;
        };
        self.select_request(path, window, cx);
    }

    /// The editor with a tab strip above it, once more than one request is open.
    fn render_request_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.editor().into_any_element();
        if self.open_tabs.len() < 2 {
            return editor;
        }
        let theme = cx.theme().clone();
        let open = self.editor().read(cx).path().cloned();
        let dirty = self.editor().read(cx).is_dirty();
        let tabs: Vec<AnyElement> = self
            .open_tabs
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let selected = open.as_deref() == Some(path.as_path());
                let name = self
                    .find_request(path)
                    .map(|request| request.name.clone())
                    .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
                h_flex()
                    .id(sidebar_row_id("tab", path))
                    .test_support()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .max_w(px(220.))
                    .flex_none()
                    .border_b_2()
                    .border_color(if selected { theme.primary } else { theme.background })
                    .when(selected, |tab| tab.bg(theme.accent))
                    .cursor_pointer()
                    .hover(|tab| tab.bg(theme.accent))
                    .child(div().min_w_0().truncate().text_sm().child(name))
                    .when(selected && dirty, |tab| {
                        tab.child(div().text_xs().text_color(theme.muted_foreground).child("●"))
                    })
                    .child(
                        Button::new(("close-tab", index))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t!("ws.close_tab").to_string())
                            .on_click(cx.listener(move |this, _, window, cx| this.close_tab(index, window, cx))),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.select_tab(index, window, cx)))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .id("tab-strip")
                    .overflow_x_scroll()
                    .border_b_1()
                    .border_color(theme.border)
                    .children(tabs),
            )
            .child(div().flex_1().min_h_0().child(editor))
            .into_any_element()
    }

    fn new_request(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.new_request_of(dir, RequestKind::Http, window, cx);
    }

    fn new_request_of(&mut self, dir: PathBuf, kind: RequestKind, window: &mut Window, cx: &mut Context<Self>) {
        match storage::create_request(&dir, &kind.template(new_request_label(kind))) {
            Ok(path) => {
                self.collapsed.remove(&dir);
                self.reload_containing(&path, window, cx);
                self.select_request(path, window, cx);
            }
            Err(e) => notify_error(format!("{e:#}"), window, cx),
        }
    }

    fn new_folder(&mut self, parent: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_name(
            t!("ws.new_folder_title").to_string(),
            String::new(),
            t!("ws.create").to_string(),
            window,
            cx,
            move |this, name, window, cx| {
                let folder = storage::create_folder(&parent, &name)?;
                this.collapsed.remove(&parent);
                this.reload_containing(&folder, window, cx);
                Ok(())
            },
        );
    }

    /// Renames a request or folder, moving its file or directory to match.
    fn rename_item(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let (title, current) = match self.find_request(&path) {
            Some(request) => (t!("ws.rename_request_title"), request.name.clone()),
            None => (
                t!("ws.rename_folder_title"),
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        };
        self.prompt_name(
            title.to_string(),
            current,
            t!("ws.rename").to_string(),
            window,
            cx,
            move |this, name, window, cx| {
                // Unsaved edits go with the request rather than being left behind.
                this.editor().update(cx, |editor, cx| editor.save(cx));
                let target = if path.is_dir() {
                    storage::rename_folder(&path, &name)?
                } else {
                    storage::rename_request(&path, &name)?
                };
                this.after_move(&path, &target, window, cx);
                Ok(())
            },
        );
    }

    /// Updates everything that remembers paths after `from` moved to `to`: the sidebar, saved
    /// responses, the open request and the saved state.
    fn after_move(&mut self, from: &Path, to: &Path, window: &mut Window, cx: &mut Context<Self>) {
        // `to.join("")` would leave a trailing separator, so the moved item itself maps to `to`.
        let remap = |path: &Path| {
            path.strip_prefix(from).ok().map(|rest| {
                if rest.as_os_str().is_empty() {
                    to.to_path_buf()
                } else {
                    to.join(rest)
                }
            })
        };
        let old_keys: Vec<(CacheKey, PathBuf)> = self
            .collection_index_for(from)
            .map(|ix| {
                let collection = &self.collections[ix];
                collection
                    .requests()
                    .into_iter()
                    .filter_map(|entry| Some((collection.response_key(entry.path), remap(entry.path)?)))
                    .collect()
            })
            .unwrap_or_default();
        let from_root = self
            .collection_index_for(from)
            .map(|ix| self.collections[ix].root.clone());
        self.reload_containing(to, window, cx);
        if let Some(from_root) = from_root
            && !to.starts_with(&from_root)
        {
            self.reload_collection(&from_root, window, cx);
        }
        let cache = ResponseCache::new(&self.paths.cache_dir);
        for (old_key, new_path) in old_keys {
            if let (Some(response), Some(new_key)) = (cache.load(&old_key), self.response_key(&new_path))
                && let Err(e) = cache.save(&new_key, &response)
            {
                eprintln!("could not move saved response: {e:#}");
            }
        }
        self.collapsed = self
            .collapsed
            .drain()
            .map(|path| remap(&path).unwrap_or(path))
            .collect();
        if let Some(last) = self.state.last_request.as_deref().and_then(remap) {
            self.state.last_request = Some(last);
            self.save_state();
        }
        let open = self.editor().read(cx).path().cloned();
        self.editor().update(cx, |editor, _| editor.moved(from, to));
        for tab in &mut self.open_tabs {
            if let Some(moved) = remap(tab) {
                *tab = moved;
            }
        }
        if let Some(new) = open.as_deref().and_then(remap) {
            self.load_in_editor(&new, window, cx);
        }
        cx.notify();
    }

    /// Copies `{{ response("Name", "$.path") }}` for using this request's response in another,
    /// with the JSONPath filter set on its response (or `$`).
    fn copy_response_reference(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.collection_index_for(&path) else {
            return;
        };
        let collection = &self.collections[ix];
        let Some(request) = collection.find_request(&path) else {
            return;
        };
        // A name shared by several requests can't identify this one; use its path instead.
        let shared = collection
            .requests()
            .iter()
            .filter(|entry| entry.request.name.eq_ignore_ascii_case(&request.name))
            .count()
            > 1;
        let target = if shared {
            path.strip_prefix(&collection.root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            request.name.clone()
        };
        let json_path = self
            .editor()
            .read(cx)
            .filter_for(&path)
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "$".into());
        cx.write_to_clipboard(ClipboardItem::new_string(crate::chain::reference(&target, &json_path)));
        window.push_notification(
            Notification::success(t!("ws.copied_response_reference").to_string()),
            cx,
        );
    }

    /// Opens `path` if it isn't open, then copies it as a curl command.
    fn copy_as_curl(&mut self, path: PathBuf, include_secrets: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor().read(cx).path() != Some(&path) {
            self.select_request(path, window, cx);
        }
        self.editor()
            .update(cx, |editor, cx| editor.copy_as_curl(include_secrets, cx));
    }

    fn duplicate_request(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.find_request(&path).map(|r| r.name.clone()) else {
            return;
        };
        self.editor().update(cx, |editor, cx| editor.save(cx));
        match storage::duplicate_request(&path, &t!("ws.copy_name", name = name)) {
            Ok(copy) => {
                self.reload_containing(&copy, window, cx);
                self.select_request(copy, window, cx);
            }
            Err(e) => notify_error(format!("{e:#}"), window, cx),
        }
    }

    /// Asks, then deletes a request or a folder with its contents.
    fn delete_item(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let message = match self.find_request(&path) {
            Some(request) => t!("ws.delete_request_message", name = request.name).to_string(),
            None => {
                let inside = self
                    .collection_index_for(&path)
                    .map(|ix| {
                        self.collections[ix]
                            .requests()
                            .iter()
                            .filter(|entry| entry.path.starts_with(&path))
                            .count()
                    })
                    .unwrap_or(0);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                t!("ws.delete_folder_message", name = name, count = inside).to_string()
            }
        };
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (weak, path, message) = (weak.clone(), path.clone(), message.clone());
            dialog
                .title(t!("ws.delete_title").to_string())
                .w(px(420.))
                .content(move |content, _, _| content.child(message.clone()))
                .footer(dialog_footer(Some(t!("ws.delete").to_string()), ButtonVariant::Danger))
                .on_ok(move |_, window, cx| {
                    weak.update(cx, |this, cx| {
                        this.close_tabs_under(&path, window, cx);
                        match storage::delete_item(&path) {
                            Ok(()) => {
                                if this.state.last_request.as_ref().is_some_and(|p| p.starts_with(&path)) {
                                    this.state.last_request = None;
                                    this.save_state();
                                }
                                this.collapsed.retain(|p| !p.starts_with(&path));
                                this.reload_containing(&path, window, cx);
                            }
                            Err(e) => notify_error(format!("{e:#}"), window, cx),
                        }
                    })
                    .ok();
                    true
                })
        });
    }

    /// A small dialog asking for a name. `apply` errors are shown and keep the dialog open.
    fn prompt_name(
        &mut self,
        title: String,
        initial: String,
        ok_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl Fn(&mut Self, String, &mut Window, &mut Context<Self>) -> Result<()> + 'static,
    ) {
        let name = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        // Opening the dialog moves focus, so focus the name (selected, ready to retype) after.
        window.defer(cx, {
            let name = name.clone();
            move |window, cx| {
                name.update(cx, |state, cx| {
                    state.focus(window, cx);
                    state.select_all(window, cx);
                })
            }
        });
        let weak = cx.entity().downgrade();
        let apply = Rc::new(apply);
        window.open_dialog(cx, move |dialog, _, _| {
            let (weak, name, apply, title, ok_label) = (
                weak.clone(),
                name.clone(),
                apply.clone(),
                title.clone(),
                ok_label.clone(),
            );
            dialog
                .title(title)
                .w(px(420.))
                .content({
                    let name = name.clone();
                    move |content, _, _| content.child(text_input(&name))
                })
                .footer(dialog_footer(Some(ok_label), ButtonVariant::Primary))
                .on_ok(move |_, window, cx| {
                    let value = name.read(cx).value().trim().to_string();
                    if value.is_empty() {
                        return false;
                    }
                    let result = weak.update(cx, |this, cx| apply(this, value, window, cx));
                    match result {
                        Ok(Err(e)) => {
                            notify_error(format!("{e:#}"), window, cx);
                            false
                        }
                        _ => true,
                    }
                })
        });
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
        self.editor().update(cx, |editor, cx| {
            editor.set_variables(layered.variables, layered.secrets, cx)
        });
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
        self.editor().update(cx, |editor, cx| editor.save(cx));
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
        self.editor().update(cx, |editor, cx| editor.relocalize(window, cx));
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
        let removed = self
            .editor()
            .update(cx, |editor, cx| editor.clear_responses(window, cx));
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
    /// Picks a file for the open request's body, storing it relative to the project when
    /// it's inside, so the collection still works on another machine.
    fn pick_upload_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self
            .editor()
            .read(cx)
            .path()
            .and_then(|path| self.collection_index_for(path))
            .map(|ix| project::project_dir(&self.collections[ix].root).to_path_buf());
        self.pick_path(
            false,
            t!("request.choose_file").to_string(),
            window,
            cx,
            move |this, file, window, cx| {
                let stored = match project.as_ref().and_then(|dir| file.strip_prefix(dir).ok()) {
                    Some(relative) => relative.display().to_string(),
                    None => file.display().to_string(),
                };
                this.editor()
                    .update(cx, |editor, cx| editor.add_upload_file(stored, window, cx));
            },
        );
    }

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

    /// Asks what to start the new project from: nothing, or a Postman, OpenAPI or AsyncAPI file.
    fn new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let weak = weak.clone();
            let content = move |content: gpui_kit::component::dialog::DialogContent, _: &mut Window, cx: &mut App| {
                let theme = cx.theme().clone();
                let choice =
                    |id: &'static str,
                     icon: IconName,
                     title: String,
                     detail: String,
                     start: fn(&mut Workspace, &mut Window, &mut Context<Workspace>)| {
                        let weak = weak.clone();
                        h_flex()
                            .id(id)
                            .test_support()
                            .gap_3()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .cursor_pointer()
                            .hover(|row| row.bg(theme.accent))
                            .child(Icon::new(icon).text_color(theme.muted_foreground))
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(title))
                                    .child(div().text_xs().text_color(theme.muted_foreground).child(detail)),
                            )
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                weak.update(cx, |this, cx| start(this, window, cx)).ok();
                            })
                    };
                let rows = v_flex()
                    .gap_2()
                    .child(choice(
                        "new-project-blank",
                        IconName::Plus,
                        t!("ws.new_project_blank").to_string(),
                        t!("ws.new_project_blank_detail").to_string(),
                        Workspace::new_blank_project,
                    ))
                    .child(choice(
                        "new-project-postman",
                        IconName::Inbox,
                        t!("ws.new_project_postman").to_string(),
                        t!("ws.new_project_postman_detail").to_string(),
                        |this, window, cx| this.new_project_from(ImportFormat::Postman, window, cx),
                    ))
                    .child(choice(
                        "new-project-openapi",
                        IconName::BookOpen,
                        t!("ws.new_project_openapi").to_string(),
                        t!("ws.new_project_openapi_detail").to_string(),
                        |this, window, cx| this.new_project_from(ImportFormat::OpenApi, window, cx),
                    ))
                    .child(choice(
                        "new-project-asyncapi",
                        IconName::Bell,
                        t!("ws.new_project_asyncapi").to_string(),
                        t!("ws.new_project_asyncapi_detail").to_string(),
                        |this, window, cx| this.new_project_from(ImportFormat::AsyncApi, window, cx),
                    ));
                content.child(rows)
            };
            dialog
                .title(t!("ws.new_project_title").to_string())
                .w(px(480.))
                .content(content)
        });
    }

    /// Picks a folder for a new, empty project and names its collection. A folder that is
    /// already a project just opens.
    fn new_blank_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_path(
            true,
            t!("ws.new_project_prompt").to_string(),
            window,
            cx,
            |this, dir, window, cx| match project::find(&dir) {
                Some(root) => {
                    window.push_notification(
                        Notification::info(
                            t!("ws.already_a_project", path = project::project_dir(&root).display()).to_string(),
                        ),
                        cx,
                    );
                    this.open_collection(root, window, cx);
                }
                None => this.offer_init_project(dir, None, window, cx),
            },
        );
    }

    /// Picks a file to import, then a folder for the project, then names it.
    fn new_project_from(&mut self, format: ImportFormat, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_import_file(format, window, cx, |this, import, window, cx| {
            this.pick_path(
                true,
                t!("ws.new_project_prompt").to_string(),
                window,
                cx,
                move |this, dir, window, cx| {
                    if let Some(existing) = project::find(&dir) {
                        let path = project::project_dir(&existing).display().to_string();
                        notify_error(t!("ws.project_has_collection", path = path).to_string(), window, cx);
                        return;
                    }
                    this.offer_init_project(dir, Some(import), window, cx);
                },
            );
            Ok(())
        });
    }

    fn open_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_project_folder(window, cx, |this, dir, window, cx| match project::find(&dir) {
            Some(root) => this.open_collection(root, window, cx),
            None => this.offer_init_project(dir, None, window, cx),
        });
    }

    /// Asks for a name, then creates `.courier/` in a folder that has no collection yet: from
    /// `import` if given, otherwise empty with a first request.
    fn offer_init_project(
        &mut self,
        dir: PathBuf,
        import: Option<CollectionImport>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let default_name = match &import {
            Some(import) => import.collection.name.clone(),
            None => project::default_name(&dir),
        };
        let summary = import.as_ref().map(|import| {
            t!(
                "ws.import_summary",
                requests = ImportItem::count_requests(&import.items),
                environments = import.environments.len()
            )
            .to_string()
        });
        let import = Rc::new(RefCell::new(import));
        let name = cx.new(|cx| InputState::new(window, cx).default_value(default_name));
        focus_in_dialog(&name, window, cx);
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let (weak, dir, name, import) = (weak.clone(), dir.clone(), name.clone(), import.clone());
            let hint = t!("ws.init_project_hint", path = dir.display(), dir = project::DOT_DIR).to_string();
            let summary = summary.clone();
            let muted = cx.theme().muted_foreground;
            dialog
                .title(t!("ws.init_project_title").to_string())
                .w(px(480.))
                .content({
                    let name = name.clone();
                    move |content, _, _| {
                        content
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(t!("ws.project_name").to_string()),
                                    )
                                    .child(text_input(&name)),
                            )
                            .children(summary.clone().map(|summary| div().pt_2().text_sm().child(summary)))
                            .child(div().pt_2().text_sm().text_color(muted).child(hint.clone()))
                    }
                })
                .footer(dialog_footer(Some(t!("ws.create").to_string()), ButtonVariant::Primary))
                .on_ok(move |_, window, cx| {
                    let collection_name = name.read(cx).value().trim().to_string();
                    if collection_name.is_empty() {
                        return false;
                    }
                    let import = import.borrow_mut().take();
                    weak.update(cx, |this, cx| {
                        if let Err(e) = this.create_project(&dir, collection_name, import, window, cx) {
                            notify_error(format!("{e:#}"), window, cx);
                        }
                    })
                    .ok();
                    true
                })
        });
    }

    /// Creates and opens a project's collection, then shows its first request.
    fn create_project(
        &mut self,
        dir: &Path,
        name: String,
        import: Option<CollectionImport>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let Some(mut import) = import else {
            let root = project::init_with(dir, &CollectionFile::new(name))?;
            self.open_collection(root.clone(), window, cx);
            self.new_request(root, window, cx);
            return Ok(());
        };
        import.collection.name = name;
        let root = import::write_project_collection(dir, &import)?;
        let collection = storage::load_collection(&root)?;
        let writes = import
            .secrets
            .iter()
            .map(|(name, value)| SecretWrite::new(&collection.file, DEFAULTS_SCOPE, DEFAULTS_LABEL, name, value))
            .collect();
        self.store_secrets(writes, window, cx);
        self.open_collection(root.clone(), window, cx);
        if let Some(first) = collection.first_request() {
            self.select_request(first.to_path_buf(), window, cx);
        }
        notify_import(&import.warnings, window, cx);
        Ok(())
    }

    fn import_curl_dialog(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let command = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(8)
                .placeholder(t!("ws.curl_placeholder").to_string())
        });
        focus_in_dialog(&command, window, cx);
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
                    weak.update(cx, |this, cx| this.create_from_curl(&dir, &text, true, window, cx))
                        .unwrap_or(false)
                })
        });
    }

    /// Makes a request in `dir` from a curl command, hoisting any credentials into secrets,
    /// and opens it. Returns whether it parsed; `loud` reports the reason when it didn't.
    fn create_from_curl(
        &mut self,
        dir: &Path,
        command: &str,
        loud: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = curl::parse(command).and_then(|mut request| {
            let (hoisted, writes) = self.hoist_into_defaults(dir, &mut request)?;
            let path = storage::create_request(dir, &request)?;
            Ok((path, hoisted, writes))
        });
        match result {
            Ok((path, hoisted, writes)) => {
                self.store_secrets(writes, window, cx);
                self.reload_containing(&path, window, cx);
                self.select_request(path, window, cx);
                if !hoisted.is_empty() {
                    window.push_notification(
                        Notification::info(t!("ws.moved_to_defaults", names = hoisted.join(", ")).to_string()),
                        cx,
                    );
                }
                true
            }
            Err(e) => {
                if loud {
                    notify_error(format!("{e:#}"), window, cx);
                }
                false
            }
        }
    }

    /// Ctrl+V outside a text field: a curl command on the clipboard becomes a request in
    /// the collection you're working in.
    fn paste_curl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        if !text.trim_start().starts_with("curl") {
            window.push_notification(Notification::info(t!("ws.paste_curl_hint").to_string()), cx);
            return;
        }
        // Next to the open request when there is one, else the collection you're looking at.
        let dir = self
            .editor()
            .read(cx)
            .path()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .or_else(|| self.active_collection(cx).map(|c| c.root.clone()));
        let Some(dir) = dir else {
            return;
        };
        self.create_from_curl(&dir, &text, true, window, cx);
    }

    /// Asks for a Postman environment file, then hands its contents to `apply`.
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

    /// Asks for a Postman collection, OpenAPI or AsyncAPI file (`format` only sets the
    /// prompt; the file's own format is detected), then hands the parsed import to `apply`.
    fn pick_import_file(
        &mut self,
        format: ImportFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut Self, CollectionImport, &mut Window, &mut Context<Self>) -> Result<()> + 'static,
    ) {
        let prompt = match format {
            ImportFormat::Postman => t!("ws.pick_postman_collection"),
            ImportFormat::OpenApi => t!("ws.pick_openapi"),
            ImportFormat::AsyncApi => t!("ws.pick_asyncapi"),
        };
        self.pick_path(false, prompt.to_string(), window, cx, |this, file, window, cx| {
            let result = fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))
                .and_then(|text| import::parse_collection_file(&text))
                .and_then(|(_, import)| apply(this, import, window, cx));
            if let Err(e) = result {
                notify_error(format!("{e:#}"), window, cx);
            }
        });
    }

    fn import_file_into(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_import_file(ImportFormat::OpenApi, window, cx, move |this, import, window, cx| {
            this.import_into(&root, import, window, cx)
        });
    }

    /// Adds an import's requests, environments and secrets to an existing collection.
    fn import_into(
        &mut self,
        root: &Path,
        mut import: CollectionImport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let mut collection = storage::load_collection(root)?;
        // Imported secret names that clash with this collection's names get a suffix.
        let mut renamed = Variables::new();
        let mut names = Vec::new();
        for name in std::mem::take(&mut import.collection.secrets) {
            let value = import.secrets.shift_remove(&name).unwrap_or_default();
            let file = &collection.file;
            let new_name = unique_name(&name, |candidate| {
                file.secrets.iter().any(|s| s == candidate)
                    || file.variables.contains_key(candidate)
                    || names.iter().any(|n| n == candidate)
            });
            if new_name != name {
                rename_placeholder(&mut import.items, &name, &new_name);
            }
            if !value.is_empty() {
                renamed.insert(new_name.clone(), value);
            }
            names.push(new_name);
        }
        import::write_items(root, &import.items)?;
        for environment in &import.environments {
            storage::create_environment(root, environment)?;
        }
        // Variables the requests need (such as `base_url`) that the collection doesn't have yet.
        let new_variables: Vec<_> = import
            .collection
            .variables
            .iter()
            .filter(|(name, _)| !collection.file.variables.contains_key(*name))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !names.is_empty() || !new_variables.is_empty() {
            collection.file.ensure_id();
            for name in names {
                if !collection.file.secrets.contains(&name) {
                    collection.file.secrets.push(name);
                }
            }
            collection.file.variables.extend(new_variables);
            storage::save_collection_file(root, &collection.file)?;
            let writes = renamed
                .iter()
                .map(|(name, value)| SecretWrite::new(&collection.file, DEFAULTS_SCOPE, DEFAULTS_LABEL, name, value))
                .collect();
            self.store_secrets(writes, window, cx);
        }
        self.reload_collection(root, window, cx);
        notify_import(&import.warnings, window, cx);
        Ok(())
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
                    this.editor()
                        .update(cx, |editor, _| editor.set_secret_store(store.clone()));
                    this.environment_editor
                        .update(cx, |editor, cx| editor.set_store(store.clone(), window, cx));
                    this.secret_store = Some(store);
                    for (root, jar) in this.cookies.clone() {
                        this.persist_cookies(&root, &jar, cx);
                    }
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

    /// Adds a secret named like `base` holding `value` to the active environment, or to the
    /// collection defaults when none is active. Returns its name, where it went, and the
    /// write for the secret store.
    fn add_secret(&self, root: &Path, base: &str, value: String) -> Result<(String, String, SecretWrite)> {
        let ix = self
            .collections
            .iter()
            .position(|c| c.root == root)
            .context(t!("ws.not_in_open_collection").to_string())?;
        let mut file: CollectionFile = storage::read_yaml(&root.join(crate::model::COLLECTION_FILE))?;
        let environment = self
            .state
            .active_environments
            .get(root)
            .and_then(|env_path| self.collections[ix].environment(env_path))
            .cloned();
        let taken: Vec<String> = match &environment {
            Some(env) => env
                .file
                .secrets
                .iter()
                .chain(env.file.variables.keys())
                .cloned()
                .collect(),
            None => file.secrets.iter().chain(file.variables.keys()).cloned().collect(),
        };
        let name = unique_name(base, |candidate| taken.iter().any(|t| t == candidate));
        let assigned = file.ensure_id().1;
        let (scope, label) = match environment {
            Some(mut env) => {
                if assigned {
                    storage::save_collection_file(root, &file)?;
                }
                env.file.secrets.push(name.clone());
                storage::write_yaml(&env.path, &env.file)?;
                (SecretRef::environment_scope(&env.path), env.file.name)
            }
            None => {
                file.secrets.push(name.clone());
                storage::save_collection_file(root, &file)?;
                (DEFAULTS_SCOPE.to_string(), t!("ws.defaults").to_string())
            }
        };
        let write = SecretWrite::new(&file, &scope, &label, &name, value);
        Ok((name, label, write))
    }

    /// Moves a request's literal auth credential into a secret.
    fn move_auth_to_secret(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| -> Result<(PathBuf, String, SecretWrite)> {
            let root = self
                .collection_index_for(&path)
                .map(|ix| self.collections[ix].root.clone())
                .context(t!("ws.not_in_open_collection").to_string())?;
            let mut request: RequestFile = storage::read_yaml(&path)?;
            let literal = request
                .auth
                .literal_credential()
                .context(t!("ws.no_literal_credential").to_string())?
                .to_string();
            let (name, scope, write) = self.add_secret(&root, &request.auth.credential_name(), literal)?;
            request.auth.set_credential(placeholder(&name));
            storage::write_yaml(&path, &request)?;
            Ok((
                root,
                t!("ws.moved_to_secret", name = name, scope = scope).to_string(),
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

    /// The auth that an inheriting item at `path` gets (a request file, or a folder for its
    /// own settings), and the name of the folder or collection it comes from. `path` itself
    /// is not consulted.
    fn inherited_auth(&self, path: &Path) -> Option<(Auth, String)> {
        let collection = &self.collections[self.collection_index_for(path)?];
        let mut folders = storage::folder_auths(&collection.root, path);
        if path.is_dir() {
            // A folder's own settings aren't inherited by itself.
            folders.retain(|(dir, _)| dir != path);
        }
        let nearest = folders.into_iter().rev().find(|(_, auth)| !auth.is_inherit());
        Some(match nearest {
            Some((dir, auth)) => (
                auth,
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            None => (collection.file.auth.clone(), self.collection_label(collection)),
        })
    }

    /// The cookie jar of the collection holding `path`.
    fn cookies_for(&mut self, path: &Path, cx: &mut Context<Self>) -> Option<Cookies> {
        let collection = &self.collections[self.collection_index_for(path)?];
        let root = collection.root.clone();
        if let Some(jar) = self.cookies.get(&root) {
            return Some(jar.clone());
        }
        let jar = Cookies::default();
        self.cookies.insert(root.clone(), jar.clone());
        self.persist_cookies(&root, &jar, cx);
        Some(jar)
    }

    /// Loads a jar's saved cookies and keeps saving it, once the secret store is available
    /// (otherwise it's done when the store connects).
    fn persist_cookies(&self, root: &Path, jar: &Cookies, cx: &mut Context<Self>) {
        let (Some(store), Some(ix)) = (self.secret_store.clone(), self.collection_index_for(root)) else {
            return;
        };
        let collection = &self.collections[ix];
        let Some(id) = collection.file.id.clone() else {
            return;
        };
        let label = format!("Courier · {} · cookies", self.collection_label(collection));
        let jar = jar.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = jar.persist_in(store, Cookies::secret(&id), label).await {
                    eprintln!("could not load saved cookies: {e:#}");
                }
            })
            .detach();
    }

    /// Lists a collection's cookies, to delete one or clear them all.
    fn manage_cookies(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(jar) = self.cookies_for(&root, cx) else {
            return;
        };
        let name = self
            .collection_index_for(&root)
            .map(|ix| self.collection_label(&self.collections[ix]))
            .unwrap_or_default();
        window.open_dialog(cx, move |dialog, _, _| {
            let jar = jar.clone();
            dialog
                .title(t!("cookies.title", name = name).to_string())
                .w(px(640.))
                .content(move |content, _, cx| {
                    let theme = cx.theme().clone();
                    let cookies = jar.list();
                    let save = |jar: &Cookies, cx: &mut App| {
                        let jar = jar.clone();
                        cx.background_executor()
                            .spawn(async move {
                                if let Err(e) = jar.save().await {
                                    eprintln!("could not save cookies: {e:#}");
                                }
                            })
                            .detach();
                    };
                    let mut rows = v_flex().id("cookie-rows").max_h(px(360.)).overflow_y_scroll().gap_px();
                    if cookies.is_empty() {
                        rows = rows.child(
                            div()
                                .py_2()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("cookies.empty").to_string()),
                        );
                    }
                    for (ix, cookie) in cookies.into_iter().enumerate() {
                        let value: String = cookie.value.chars().take(40).collect();
                        let expires = cookie
                            .expires
                            .clone()
                            .unwrap_or_else(|| t!("cookies.session").to_string());
                        rows = rows.child(
                            h_flex()
                                .gap_2()
                                .py_1()
                                .text_sm()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(div().truncate().child(format!("{}={value}", cookie.name)))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format!("{}{} · {expires}", cookie.domain, cookie.path)),
                                        ),
                                )
                                .child({
                                    let jar = jar.clone();
                                    Button::new(("cookie-delete", ix))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Delete)
                                        .tooltip(t!("cookies.delete").to_string())
                                        .on_click(move |_, window, cx| {
                                            jar.remove(&cookie);
                                            save(&jar, cx);
                                            window.refresh();
                                        })
                                }),
                        );
                    }
                    content
                        .child(
                            h_flex()
                                .justify_between()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t!("cookies.hint").to_string())
                                .child({
                                    let jar = jar.clone();
                                    Button::new("cookies-clear")
                                        .xsmall()
                                        .label(t!("cookies.clear").to_string())
                                        .on_click(move |_, window, cx| {
                                            jar.clear();
                                            save(&jar, cx);
                                            window.refresh();
                                        })
                                }),
                        )
                        .child(rows)
                })
        });
    }

    /// Tells the editor what its request inherits, after loading it or changing auth above it.
    fn refresh_inherited_auth(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.editor().read(cx).path().cloned() else {
            return;
        };
        if let Some((auth, source)) = self.inherited_auth(&path) {
            self.editor()
                .update(cx, |editor, cx| editor.set_inherited_auth(auth, source, cx));
        }
    }

    /// Edits the request settings of a collection (at its root), a folder, or a request.
    fn edit_settings(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.collection_index_for(&path) else {
            return;
        };
        let root = self.collections[ix].root.clone();
        let project = project::project_dir(&root).to_path_buf();
        enum Target {
            Collection,
            Folder,
            Request,
        }
        let target = if path == root {
            Target::Collection
        } else if path.is_dir() {
            Target::Folder
        } else {
            Target::Request
        };
        let (current, name, inherited) = match target {
            Target::Collection => (
                self.collections[ix].file.settings.clone(),
                self.collection_label(&self.collections[ix]),
                RequestSettings::default(),
            ),
            Target::Folder => (
                storage::read_folder(&path).settings,
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                storage::inherited_settings(&root, &path),
            ),
            Target::Request => {
                let Some(request) = self.find_request(&path).cloned() else {
                    return;
                };
                (
                    request.settings,
                    request.name,
                    storage::inherited_settings(&root, &path),
                )
            }
        };
        let inherited = inherited.resolve(&project, AppSettings::get(cx).request_timeout_secs);
        let form = cx.new(|cx| SettingsForm::new(&current, inherited, window, cx));
        cx.subscribe_in(&form, window, {
            let project = project.clone();
            move |this, form, event: &SettingsFormEvent, window, cx| {
                let SettingsFormEvent::Browse(field) = *event;
                let (form, project) = (form.clone(), project.clone());
                this.pick_path(
                    false,
                    t!("settings_form.browse").to_string(),
                    window,
                    cx,
                    move |_, file, window, cx| {
                        // Files inside the project are stored relative to it, so the collection
                        // works on other machines; others keep their full path, with a warning.
                        let stored = match file.strip_prefix(&project) {
                            Ok(relative) if field != PathField::UnixSocket => relative.display().to_string(),
                            _ => {
                                if field != PathField::UnixSocket {
                                    window.push_notification(
                                        Notification::warning(
                                            t!("settings_form.outside_project", path = file.display()).to_string(),
                                        ),
                                        cx,
                                    );
                                }
                                file.display().to_string()
                            }
                        };
                        form.update(cx, |form, cx| form.set_path(field, stored, window, cx));
                    },
                );
            }
        })
        .detach();
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (weak, form, path, root, name) = (weak.clone(), form.clone(), path.clone(), root.clone(), name.clone());
            dialog
                .title(t!("settings_form.title", name = name).to_string())
                .w(px(620.))
                .content({
                    let form = form.clone();
                    move |content, _, _| content.child(form.clone())
                })
                .footer(dialog_footer(
                    Some(t!("ws.save_button").to_string()),
                    ButtonVariant::Primary,
                ))
                .on_ok(move |_, window, cx| {
                    let Ok(settings) = form.read(cx).value(cx) else {
                        return false;
                    };
                    let result = if path == root {
                        storage::read_yaml::<CollectionFile>(&root.join(crate::model::COLLECTION_FILE)).and_then(
                            |mut file| {
                                file.settings = settings;
                                storage::save_collection_file(&root, &file)
                            },
                        )
                    } else if path.is_dir() {
                        let mut folder = storage::read_folder(&path);
                        folder.settings = settings;
                        storage::write_folder(&path, &folder)
                    } else {
                        storage::read_yaml::<RequestFile>(&path).and_then(|mut request| {
                            request.settings = settings;
                            storage::write_yaml(&path, &request)
                        })
                    };
                    weak.update(cx, |this, cx| match result {
                        Ok(()) => {
                            this.reload_collection(&root, window, cx);
                            if let Some(open) = this.editor().read(cx).path().cloned()
                                && open.starts_with(&path)
                            {
                                this.load_in_editor(&open, window, cx);
                            }
                        }
                        Err(e) => notify_error(format!("{e:#}"), window, cx),
                    })
                    .ok();
                    true
                })
        });
    }

    /// Edits the auth of a collection (at its root) or a folder.
    fn edit_auth(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.collection_index_for(&path) else {
            return;
        };
        let root = self.collections[ix].root.clone();
        let is_collection = path == root;
        let (current, name) = if is_collection {
            (
                self.collections[ix].file.auth.clone(),
                self.collection_label(&self.collections[ix]),
            )
        } else {
            (
                storage::read_folder(&path).auth,
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        };
        let inherited = if is_collection {
            None
        } else {
            self.inherited_auth(&path)
        };
        let form = cx.new(|cx| {
            let mut form = AuthForm::new(!is_collection, window, cx);
            form.set(&current, window, cx);
            form.set_inherited(inherited, cx);
            form
        });
        cx.subscribe_in(&form, window, {
            let root = root.clone();
            move |this, form, event: &AuthFormEvent, window, cx| {
                if let AuthFormEvent::MoveToSecret = event {
                    let auth = form.read(cx).value(cx);
                    let Some(literal) = auth.literal_credential().map(str::to_string) else {
                        return;
                    };
                    match this.add_secret(&root, &auth.credential_name(), literal) {
                        Ok((name, scope, write)) => {
                            this.store_secrets(vec![write], window, cx);
                            this.reload_collection(&root, window, cx);
                            form.update(cx, |form, cx| form.set_credential(placeholder(&name), window, cx));
                            window.push_notification(
                                Notification::success(t!("ws.moved_to_secret", name = name, scope = scope).to_string()),
                                cx,
                            );
                        }
                        Err(e) => notify_error(format!("{e:#}"), window, cx),
                    }
                }
            }
        })
        .detach();
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (weak, form, path, root, name) = (weak.clone(), form.clone(), path.clone(), root.clone(), name.clone());
            dialog
                .title(t!("ws.auth_title", name = name).to_string())
                .w(px(560.))
                .content({
                    let form = form.clone();
                    move |content, _, _| content.child(form.clone())
                })
                .footer(dialog_footer(
                    Some(t!("ws.save_button").to_string()),
                    ButtonVariant::Primary,
                ))
                .on_ok(move |_, window, cx| {
                    let auth = form.read(cx).value(cx);
                    let result = if path == root {
                        storage::read_yaml::<CollectionFile>(&root.join(crate::model::COLLECTION_FILE)).and_then(
                            |mut file| {
                                file.auth = auth;
                                storage::save_collection_file(&root, &file)
                            },
                        )
                    } else {
                        let mut folder = storage::read_folder(&path);
                        folder.auth = auth;
                        storage::write_folder(&path, &folder)
                    };
                    weak.update(cx, |this, cx| match result {
                        Ok(()) => {
                            this.reload_collection(&root, window, cx);
                            this.refresh_inherited_auth(cx);
                        }
                        Err(e) => notify_error(format!("{e:#}"), window, cx),
                    })
                    .ok();
                    true
                })
        });
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

    fn render_sidebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let weak = cx.entity().downgrade();
        let mut rows = Vec::new();
        for collection in &self.collections {
            self.render_collection(collection, &mut rows, cx);
        }

        let handle = div()
            .id("sidebar-resize")
            .test_support()
            .absolute()
            .top_0()
            .right(px(-3.))
            .h_full()
            .w(px(6.))
            .cursor_col_resize()
            .hover(|handle| handle.bg(theme.accent))
            .on_drag(SidebarResize, |_, _, _, cx| cx.new(|_| SidebarResize))
            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    this.state.sidebar_width = None;
                    this.save_state();
                    cx.notify();
                }
            }));
        v_flex()
            .relative()
            .w(self.sidebar_width(window))
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .child(handle)
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
                            .dropdown_menu(move |menu, window, cx| {
                                scratch_request_submenu(menu, &weak, window, cx)
                                    .separator()
                                    .item(menu_item(t!("ws.new_project_ellipsis"), &weak, |this, window, cx| {
                                        this.new_project(window, cx)
                                    }))
                                    .item(menu_item(t!("ws.open_project_ellipsis"), &weak, |this, window, cx| {
                                        this.open_project(window, cx)
                                    }))
                            }),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(text_input(&self.search).small().cleanable(true)),
            )
            .child(
                v_flex()
                    .id("sidebar-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_1()
                    .when(rows.is_empty() && self.search_query(cx).is_some(), |list| {
                        list.child(
                            div()
                                .p_2()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("ws.search_none").to_string()),
                        )
                    })
                    .children(rows)
                    // Right-clicking the empty space below the rows.
                    .child(div().id("sidebar-space").flex_1().min_h(px(48.)).context_menu({
                        let weak = cx.entity().downgrade();
                        move |menu, window, cx| {
                            scratch_request_submenu(menu, &weak, window, cx)
                                .separator()
                                .item(menu_item(t!("ws.new_project_ellipsis"), &weak, |this, window, cx| {
                                    this.new_project(window, cx)
                                }))
                                .item(menu_item(t!("ws.open_project_ellipsis"), &weak, |this, window, cx| {
                                    this.open_project(window, cx)
                                }))
                        }
                    })),
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
                        Button::new("empty-scratch-request")
                            .primary()
                            .icon(IconName::SquareTerminal)
                            .label(t!("ws.new_scratch_request").to_string())
                            .dropdown_caret(true)
                            .dropdown_menu({
                                let weak = cx.entity().downgrade();
                                move |menu, _, _| {
                                    RequestKind::ALL.into_iter().fold(menu, |menu, kind| {
                                        menu.item(menu_item(new_request_label(kind), &weak, move |this, window, cx| {
                                            this.new_scratch_request_of(kind, window, cx)
                                        }))
                                    })
                                }
                            }),
                    )
                    .child(
                        Button::new("empty-new-project")
                            .icon(IconName::Plus)
                            .label(t!("ws.new_project_ellipsis").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.new_project(window, cx))),
                    )
                    .child(
                        Button::new("empty-open-project")
                            .icon(IconName::FolderOpen)
                            .label(t!("ws.open_project_ellipsis").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.open_project(window, cx))),
                    ),
            )
    }

    /// What the sidebar is filtered to, lowercased, or `None` when the box is empty.
    fn search_query(&self, cx: &App) -> Option<String> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        (!query.is_empty()).then_some(query)
    }

    /// A request matches on its name, method or URL, so "post pets" and "/v2/" both work.
    fn request_matches(request: &RequestFile, query: &str) -> bool {
        query.split_whitespace().all(|word| {
            request.name.to_lowercase().contains(word)
                || request.url.to_lowercase().contains(word)
                || request_label(request).to_lowercase().contains(word)
        })
    }

    fn folder_matches(items: &[Item], query: &str) -> bool {
        items.iter().any(|item| match item {
            Item::Request { request, .. } => Self::request_matches(request, query),
            Item::Folder { name, children, .. } => {
                name.to_lowercase().contains(query) || Self::folder_matches(children, query)
            }
        })
    }

    fn render_collection(&self, collection: &Collection, rows: &mut Vec<AnyElement>, cx: &mut Context<Self>) {
        let theme = cx.theme();
        let root = collection.root.clone();
        let scratch = self.is_scratch(&root);
        let query = self.search_query(cx);
        // While filtering, everything holding a match is open: hunting through folders for
        // the thing you just searched for defeats the search.
        let collapsed = query.is_none() && self.collapsed.contains(&root);
        if let Some(query) = &query
            && !Self::folder_matches(&collection.items, query)
            && !collection.file.name.to_lowercase().contains(query.as_str())
        {
            return;
        }
        let weak = cx.entity().downgrade();
        let row_id = rows.len();

        rows.push(
            h_flex()
                .id(sidebar_row_id("collection", &root))
                .test_support()
                .group("collection-row")
                .px_1()
                .py_1()
                .mt_1()
                .gap_1()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(theme.sidebar_accent))
                .child(chevron(collapsed))
                .when(scratch, |row| {
                    row.child(
                        Icon::new(IconName::SquareTerminal)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(self.collection_label(collection)),
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
                        .dropdown_menu(move |menu, _, _| collection_menu(menu, &weak, &root, scratch))
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
                .drag_over::<DraggedRequest>(|row, _, _, cx| row.bg(cx.theme().drop_target))
                .on_drop(cx.listener({
                    let root = root.clone();
                    move |this, dragged: &DraggedRequest, window, cx| {
                        this.drop_request(dragged.path.clone(), root.clone(), None, window, cx)
                    }
                }))
                .context_menu({
                    let (weak, root) = (cx.entity().downgrade(), root.clone());
                    move |menu, _, _| collection_menu(menu, &weak, &root, scratch)
                })
                .into_any_element(),
        );

        if !collapsed {
            self.render_items(&collection.items, 1, rows, cx);
        }
    }

    fn render_items(&self, items: &[Item], depth: usize, rows: &mut Vec<AnyElement>, cx: &mut Context<Self>) {
        let theme = cx.theme().clone();
        let destinations: Rc<Vec<(String, PathBuf)>> = Rc::new(
            self.collections
                .iter()
                .map(|c| (self.collection_label(c), c.root.clone()))
                .collect(),
        );
        let selected = self.editor().read(cx).path().cloned();
        let indent = px(4. + depth as f32 * 14.);
        let query = self.search_query(cx);

        for item in items {
            match item {
                Item::Folder { name, path, children } => {
                    if let Some(query) = &query
                        && !name.to_lowercase().contains(query.as_str())
                        && !Self::folder_matches(children, query)
                    {
                        continue;
                    }
                    let collapsed = query.is_none() && self.collapsed.contains(path);
                    let path = path.clone();
                    let menu_path = path.clone();
                    let weak = cx.entity().downgrade();
                    rows.push(
                        h_flex()
                            .id(sidebar_row_id("folder", &path))
                            .test_support()
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
                            .drag_over::<DraggedRequest>(|row, _, _, cx| row.bg(cx.theme().drop_target))
                            .on_drop(cx.listener({
                                let into = menu_path.clone();
                                move |this, dragged: &DraggedRequest, window, cx| {
                                    this.drop_request(dragged.path.clone(), into.clone(), None, window, cx)
                                }
                            }))
                            .context_menu(move |menu, _, _| folder_menu(menu, &weak, &menu_path))
                            .into_any_element(),
                    );
                    if !collapsed {
                        self.render_items(children, depth + 1, rows, cx);
                    }
                }
                Item::Request { path, request } => {
                    if let Some(query) = &query
                        && !Self::request_matches(request, query)
                    {
                        continue;
                    }
                    let is_selected = selected.as_ref() == Some(path);
                    let path = path.clone();
                    let menu_path = path.clone();
                    let kind = RequestKind::of(request);
                    let weak = cx.entity().downgrade();
                    rows.push(
                        h_flex()
                            .id(sidebar_row_id("request", &path))
                            .test_support()
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
                                    .text_color(request_color(request, &theme, cx))
                                    .child(short_method(&request_label(request))),
                            )
                            .child(div().min_w_0().truncate().child(request.name.clone()))
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.select_request(path.clone(), window, cx)),
                            )
                            .on_drag(
                                DraggedRequest {
                                    path: menu_path.clone(),
                                    label: request.name.clone().into(),
                                },
                                |dragged, _, _, cx| cx.new(|_| dragged.clone()),
                            )
                            .drag_over::<DraggedRequest>(|row, _, _, cx| row.bg(cx.theme().drop_target))
                            .on_drop(cx.listener({
                                let after = menu_path.clone();
                                move |this, dragged: &DraggedRequest, window, cx| {
                                    let Some(folder) = after.parent().map(Path::to_path_buf) else {
                                        return;
                                    };
                                    this.drop_request(dragged.path.clone(), folder, Some(after.clone()), window, cx)
                                }
                            }))
                            .context_menu({
                                let destinations = destinations.clone();
                                move |menu, window, cx| {
                                    request_menu(menu, &weak, &menu_path, kind, &destinations, window, cx)
                                }
                            })
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
            .map(|c| (self.collection_label(c), c.root.clone()));
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
            .on_action(
                cx.listener(|this, _: &palette::NewScratchRequest, window, cx| this.new_scratch_request(window, cx)),
            )
            .on_action(cx.listener(|this, _: &palette::CloseTab, window, cx| this.close_tab(this.active, window, cx)))
            .on_action(cx.listener(|this, _: &palette::PasteCurl, window, cx| this.paste_curl(window, cx)))
            .on_action(cx.listener(|this, _: &palette::NextTab, window, cx| this.step_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &palette::PreviousTab, window, cx| this.step_tab(-1, window, cx)))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<SidebarResize>, window, cx| {
                this.resize_sidebar(event.event.position.x, window, cx)
            }))
            .on_drop(cx.listener(|this, _: &SidebarResize, _, _| this.save_state()))
            .child(self.render_sidebar(window, cx))
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
                    .child(div().flex_1().min_h_0().child(match self.main_view {
                        MainView::Environments => self.environment_editor.clone().into_any_element(),
                        MainView::Runner => self.runner.clone().into_any_element(),
                        MainView::Request
                            if self.projects().next().is_none() && self.editor().read(cx).path().is_none() =>
                        {
                            self.render_no_projects(cx).into_any_element()
                        }
                        MainView::Request => self.render_request_view(cx),
                    })),
            )
            .children(dialog_layer)
            .children(notification_layer)
    }
}

// MARK: Helpers

/// A sidebar row's id: stable across reorders and filtering, unlike a row number, so GPUI
/// keeps each row's state (and tests can name one).
fn sidebar_row_id(kind: &str, path: &Path) -> ElementId {
    ElementId::Name(SharedString::from(format!("{kind}:{}", path.display())))
}

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

fn new_request_label(kind: RequestKind) -> String {
    match kind {
        RequestKind::Http => t!("ws.new_http_request"),
        RequestKind::Graphql => t!("ws.new_graphql_request"),
        RequestKind::WebSocket => t!("ws.new_websocket_request"),
        RequestKind::EventStream => t!("ws.new_event_stream_request"),
    }
    .to_string()
}

/// "New HTTP request", "New GraphQL request", … creating in `dir`.
fn new_request_items(menu: PopupMenu, weak: &WeakEntity<Workspace>, dir: &Path) -> PopupMenu {
    RequestKind::ALL.into_iter().fold(menu, |menu, kind| {
        let dir = dir.to_path_buf();
        menu.item(menu_item(new_request_label(kind), weak, move |this, window, cx| {
            this.new_request_of(dir.clone(), kind, window, cx)
        }))
    })
}

fn scratch_request_submenu(
    menu: PopupMenu,
    weak: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let weak = weak.clone();
    menu.submenu(
        t!("ws.new_scratch_request").to_string(),
        window,
        cx,
        move |menu, _, _| {
            RequestKind::ALL.into_iter().fold(menu, |menu, kind| {
                menu.item(menu_item(new_request_label(kind), &weak, move |this, window, cx| {
                    this.new_scratch_request_of(kind, window, cx)
                }))
            })
        },
    )
}

fn folder_menu(menu: PopupMenu, weak: &WeakEntity<Workspace>, folder: &Path) -> PopupMenu {
    let item = |key: &str, action: RootAction| {
        let folder = folder.to_path_buf();
        menu_item(t!(key), weak, move |this, window, cx| {
            action(this, folder.clone(), window, cx)
        })
    };
    new_request_items(menu, weak, folder)
        .separator()
        .item(item("run.menu_folder", Workspace::run_scope))
        .item(item("ws.new_folder_ellipsis", Workspace::new_folder))
        .item(item("ws.auth_ellipsis", Workspace::edit_auth))
        .item(item("settings_form.menu", Workspace::edit_settings))
        .separator()
        .item(item("ws.rename_ellipsis", Workspace::rename_item))
        .item(item("ws.delete_ellipsis", Workspace::delete_item))
        .separator()
        .item(item("ws.show_in_file_manager", |_, path, _, cx| cx.reveal_path(&path)))
}

fn request_menu(
    menu: PopupMenu,
    weak: &WeakEntity<Workspace>,
    request: &Path,
    kind: RequestKind,
    collections: &[(String, PathBuf)],
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    // Other collections this request can move to.
    let targets: Vec<(String, PathBuf)> = collections
        .iter()
        .filter(|(_, root)| !request.starts_with(root))
        .cloned()
        .collect();
    let item = |key: &str, action: RootAction| {
        let request = request.to_path_buf();
        menu_item(t!(key), weak, move |this, window, cx| {
            action(this, request.clone(), window, cx)
        })
    };
    menu.item(item("ws.open", Workspace::select_request))
        .item(item("ws.rename_ellipsis", Workspace::rename_item))
        .item(item("ws.duplicate", Workspace::duplicate_request))
        .item(item("settings_form.menu", Workspace::edit_settings))
        .separator()
        .item(item("ws.copy_response_reference", Workspace::copy_response_reference))
        .item(item("ws.copy_as_curl", |this, path, window, cx| {
            this.copy_as_curl(path, false, window, cx)
        }))
        .item(item("ws.copy_as_curl_with_secrets", |this, path, window, cx| {
            this.copy_as_curl(path, true, window, cx)
        }))
        .separator()
        .when_some(request.parent(), |menu, dir| {
            let (weak, dir) = (weak.clone(), dir.to_path_buf());
            menu.submenu(
                t!("ws.new_request_beside").to_string(),
                window,
                cx,
                move |menu, _, _| new_request_items(menu, &weak, &dir),
            )
        })
        .when(!targets.is_empty(), |menu| {
            let (weak, request) = (weak.clone(), request.to_path_buf());
            menu.submenu(t!("ws.move_to").to_string(), window, cx, move |mut menu, _, _| {
                for (name, root) in &targets {
                    let (request, root) = (request.clone(), root.clone());
                    menu = menu.item(menu_item(name.clone(), &weak, move |this, window, cx| {
                        this.move_request(request.clone(), root.clone(), window, cx)
                    }));
                }
                menu
            })
        })
        .submenu(t!("colors.menu", kind = kind_name(kind)).to_string(), window, cx, {
            let weak = weak.clone();
            move |menu, _, cx| {
                let current = AppSettings::get(cx).request_colors.get(kind);
                color_choices(kind).into_iter().fold(menu, |menu, color| {
                    menu.item(
                        menu_item(color_name(color), &weak, move |this, _, cx| {
                            this.set_request_color(kind, color, cx)
                        })
                        .checked(color == current),
                    )
                })
            }
        })
        .separator()
        .item(item("ws.delete_ellipsis", Workspace::delete_item))
        .item(item("ws.show_in_file_manager", |_, path, _, cx| cx.reveal_path(&path)))
}

/// A collection action, as used by the collection menu and the command palette.
pub(super) type RootAction = fn(&mut Workspace, PathBuf, &mut Window, &mut Context<Workspace>);

fn collection_menu(menu: PopupMenu, weak: &WeakEntity<Workspace>, root: &Path, scratch: bool) -> PopupMenu {
    let item = |key: &str, action: RootAction| {
        let root = root.to_path_buf();
        menu_item(t!(key), weak, move |this, window, cx| {
            action(this, root.clone(), window, cx)
        })
    };
    new_request_items(menu, weak, root)
        .separator()
        .item(item("run.menu_collection", Workspace::run_scope))
        .item(item("ws.new_folder_ellipsis", Workspace::new_folder))
        .item(item("ws.auth_ellipsis", Workspace::edit_auth))
        .item(item("settings_form.menu", Workspace::edit_settings))
        .item(item("ws.manage_environments_ellipsis", Workspace::manage_environments))
        .item(item("cookies.menu", Workspace::manage_cookies))
        .separator()
        .item(item("ws.import_curl", Workspace::import_curl_dialog))
        .item(item("ws.import_file_here", Workspace::import_file_into))
        .item(item(
            "ws.import_postman_environment",
            Workspace::import_postman_environment,
        ))
        .separator()
        .item(item("ws.show_in_file_manager", |_, root, _, cx| cx.reveal_path(&root)))
        .item(item("ws.reload_from_disk", |this, root, window, cx| {
            this.reload_collection(&root, window, cx)
        }))
        .when(!scratch, |menu| {
            menu.item(item("ws.close_collection", |this, root, window, cx| {
                this.close_collection(&root, window, cx)
            }))
        })
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
/// The sidebar label: the HTTP method, or WS / GQL / SSE for the other kinds.
fn request_label(request: &RequestFile) -> String {
    match RequestKind::of(request) {
        RequestKind::Graphql => "GQL".into(),
        RequestKind::WebSocket => "WS".into(),
        RequestKind::EventStream => "SSE".into(),
        RequestKind::Http => request.method.clone(),
    }
}

/// The label colour for `request`, from the settings and the active theme.
fn request_color(request: &RequestFile, theme: &gpui_kit::component::theme::Theme, cx: &App) -> Hsla {
    let kind = RequestKind::of(request);
    match AppSettings::get(cx).request_colors.get(kind) {
        LabelColor::Method => method_color(&request.method, theme),
        color => label_color(color, theme),
    }
}

pub(super) fn label_color(color: LabelColor, theme: &gpui_kit::component::theme::Theme) -> Hsla {
    match color {
        LabelColor::Method => theme.foreground,
        LabelColor::Red => theme.red,
        LabelColor::Yellow => theme.yellow,
        LabelColor::Green => theme.green,
        LabelColor::Cyan => theme.cyan,
        LabelColor::Blue => theme.blue,
        LabelColor::Magenta => theme.magenta,
        LabelColor::Grey => theme.muted_foreground,
    }
}

pub(super) fn color_name(color: LabelColor) -> String {
    match color {
        LabelColor::Method => t!("colors.by_method"),
        LabelColor::Red => t!("colors.red"),
        LabelColor::Yellow => t!("colors.yellow"),
        LabelColor::Green => t!("colors.green"),
        LabelColor::Cyan => t!("colors.cyan"),
        LabelColor::Blue => t!("colors.blue"),
        LabelColor::Magenta => t!("colors.magenta"),
        LabelColor::Grey => t!("colors.grey"),
    }
    .to_string()
}

pub(super) fn kind_name(kind: RequestKind) -> String {
    match kind {
        RequestKind::Http => t!("colors.kind_http"),
        RequestKind::Graphql => t!("colors.kind_graphql"),
        RequestKind::WebSocket => t!("colors.kind_websocket"),
        RequestKind::EventStream => t!("colors.kind_sse"),
    }
    .to_string()
}

/// The colours a kind of request can use: HTTP can also colour by method.
pub(super) fn color_choices(kind: RequestKind) -> Vec<LabelColor> {
    let mut choices = LabelColor::CHOICES.to_vec();
    if kind == RequestKind::Http {
        choices.insert(0, LabelColor::Method);
    }
    choices
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
        Notification::success(t!("ws.imported").to_string())
    } else {
        for warning in warnings {
            eprintln!("import: {warning}");
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
            assert_eq!(workspace.read(cx).projects().count(), 0, "only the scratchpad");
            window.render_frame(cx);
            assert!(
                window.try_find("empty-new-project").is_some(),
                "empty state offers New project"
            );
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
            assert_eq!(ws.projects().count(), 1);
            let opened = ws.editor().read(cx).path().cloned().expect("a first request is open");
            assert!(opened.starts_with(&root), "{}", opened.display());
            assert_eq!(ws.projects().next().unwrap().requests().len(), 1);
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
    async fn new_projects_start_blank_or_from_a_spec(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let (workspace, window) = open_workspace(cx, &paths, None);

        // New project offers each starting point.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.new_project(window, cx));
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            for id in [
                "new-project-blank",
                "new-project-postman",
                "new-project-openapi",
                "new-project-asyncapi",
            ] {
                assert!(window.try_find(id).is_some(), "offers {id}");
            }
            window.close_dialog(cx);
        })
        .unwrap();

        // From an OpenAPI spec: the name comes from the spec, then Create writes the collection.
        let spec = r#"
openapi: 3.0.3
info: { title: Pet API }
servers:
  - { url: https://pets.test, description: Production }
  - { url: http://localhost:3000, description: Local }
security: [{ bearerAuth: [] }]
paths:
  /pets/{petId}:
    get: { summary: Get a pet, tags: [pets] }
components:
  securitySchemes:
    bearerAuth: { type: http, scheme: bearer }
"#;
        let (format, import) = import::parse_collection_file(spec).unwrap();
        assert_eq!(format, ImportFormat::OpenApi);
        let project = tmp.path().join("pet-api");
        fs::create_dir_all(&project).unwrap();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.offer_init_project(project.clone(), Some(import), window, cx)
            });
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("dialog-ok", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let root = project.join(".courier");
        let collection = storage::load_collection(&root).unwrap();
        assert_eq!(collection.file.name, "Pet API");
        assert_eq!(collection.file.secrets, ["bearer_auth"]);
        assert_eq!(collection.environments.len(), 2);
        cx.update(|cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.projects().count(), 1);
            let opened = ws.editor().read(cx).path().cloned().expect("the first request is open");
            assert!(opened.starts_with(root.join("pets")), "{}", opened.display());
        });

        // Importing into an existing collection adds requests without overwriting variables.
        let example = create_example_project(tmp.path()).unwrap();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_collection(example.clone(), window, cx);
                let (_, import) = import::parse_collection_file(spec).unwrap();
                this.import_into(&example, import, window, cx).unwrap();
            });
        })
        .unwrap();
        let merged = storage::load_collection(&example).unwrap();
        assert_eq!(merged.file.variables["base_url"], "https://httpbin.org", "kept");
        assert_eq!(merged.file.variables["petId"], "", "added");
        assert_eq!(merged.file.secrets, ["bearer_auth"]);
        assert_eq!(merged.requests().len(), 3);
        assert_eq!(merged.environments.len(), 3);
    }

    #[gpui_kit::test]
    async fn sidebar_resizes_remembers_and_resets(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let (workspace, window) = open_workspace(cx, &paths, None);
        let sidebar_width = |cx: &mut TestAppContext| cx.update(|cx| workspace.read(cx).state.sidebar_width);

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let handle = window.find("sidebar-resize").bounds();
            let from = handle.center();
            window.drag(from, point(px(400.), from.y), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(sidebar_width(cx), Some(400.));
        let state = fs::read_to_string(paths.state_dir.join("state.yaml")).unwrap();
        assert!(
            state.contains("sidebar_width: 400"),
            "saved when the drag ends: {state}"
        );

        // Never so wide that the editor loses its room.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let from = window.find("sidebar-resize").bounds().center();
            let far = window.viewport_size().width + px(500.);
            window.drag(from, point(far, from.y), cx);
            let max = f32::from(window.viewport_size().width) - MAIN_MIN_WIDTH;
            assert_eq!(workspace.read(cx).state.sidebar_width, Some(max));
        })
        .unwrap();

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.double_click("sidebar-resize", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(sidebar_width(cx), None, "double-click resets");
    }

    #[gpui_kit::test]
    async fn variables_are_coloured_and_the_url_stays_one_line(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.url = "{{base_url}}/json?page=2&key={{api_key}}".into();
        request.headers = crate::model::headers_from_text("X-Token: {{token}}");
        storage::write_yaml(&get_json, &request).unwrap();
        let mut file: crate::model::CollectionFile = storage::read_yaml(&root.join("collection.yaml")).unwrap();
        file.secrets.push("token".into());
        storage::save_collection_file(&root, &file).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.select_request(get_json.clone(), window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        let pairs = |items: &[(&str, &str)]| {
            items
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            cx.update(|cx| editor.read(cx).highlights_for_test(cx)),
            pairs(&[
                ("{{base_url}}", "Variable"),
                ("page", "QueryName"),
                ("key", "QueryName"),
                ("{{api_key}}", "Undefined"),
                ("{{token}}", "Secret"),
            ])
        );

        // Edits recolour straight away, and a pasted line break doesn't split the URL.
        let url = cx.update(|cx| editor.read(cx).url_for_test());
        cx.update_window(window, |_, window, cx| {
            url.update(cx, |s, cx| s.replace_all("{{base_url}}/a\n?b={{nope}}", window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| url.read(cx).value().to_string()),
            "{{base_url}}/a?b={{nope}}"
        );
        assert_eq!(
            cx.update(|cx| editor.read(cx).highlights_for_test(cx)),
            pairs(&[
                ("{{base_url}}", "Variable"),
                ("b", "QueryName"),
                ("{{nope}}", "Undefined"),
                ("{{token}}", "Secret"),
            ])
        );

        // Enter in the URL sends the request rather than adding a line.
        let (port, received) = one_shot_server("HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        cx.update_window(window, |_, window, cx| {
            url.update(cx, |s, cx| {
                s.replace_all(format!("http://127.0.0.1:{port}/entered"), window, cx);
                s.focus(window, cx);
            });
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let head = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.starts_with("GET /entered "), "{head}");
        assert!(!cx.update(|cx| url.read(cx).value().contains('\n')));
    }

    #[gpui_kit::test]
    async fn requests_and_folders_are_created_renamed_duplicated_and_deleted(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        let type_and_confirm = |cx: &mut TestAppContext, text: &str| {
            // The dialog slides in; clicking before it settles hits where it isn't yet.
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.input(text, cx);
            })
            .unwrap();
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("dialog-ok", cx);
            })
            .unwrap();
            cx.run_until_parked();
        };

        // New folder, named in a dialog.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.new_folder(root.clone(), window, cx));
        })
        .unwrap();
        type_and_confirm(cx, "Pets");
        let pets = root.join("Pets");
        assert!(pets.is_dir());

        // Renaming the open request keeps its unsaved edits, its saved response and the selection.
        let old_key = cx.update(|cx| workspace.read(cx).response_key(&get_json).unwrap());
        let cache = ResponseCache::new(&paths.cache_dir);
        cache
            .save(&old_key, &crate::response_cache::StoredResponse::failed(1, "earlier"))
            .unwrap();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.select_request(get_json.clone(), window, cx));
            let url = editor.read(cx).url_for_test();
            url.update(cx, |s, cx| s.replace_all("{{base_url}}/edited", window, cx));
            workspace.update(cx, |this, cx| this.rename_item(get_json.clone(), window, cx));
        })
        .unwrap();
        type_and_confirm(cx, "Fetch JSON");
        let fetch = root.join("fetch-json.yaml");
        assert!(fetch.exists() && !get_json.exists());
        let saved: RequestFile = storage::read_yaml(&fetch).unwrap();
        assert_eq!(
            (saved.name.as_str(), saved.url.as_str()),
            ("Fetch JSON", "{{base_url}}/edited")
        );
        cx.update(|cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.editor().read(cx).path(), Some(&fetch));
            assert_eq!(ws.state.last_request.as_ref(), Some(&fetch));
            let key = ws.response_key(&fetch).unwrap();
            assert!(cache.load(&key).is_some(), "saved response moved with it");
        });

        // Duplicate opens the copy.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.duplicate_request(fetch.clone(), window, cx));
        })
        .unwrap();
        let copy = root.join("fetch-json-copy.yaml");
        assert_eq!(
            storage::read_yaml::<RequestFile>(&copy).unwrap().name,
            "Fetch JSON copy"
        );
        cx.update(|cx| assert_eq!(editor.read(cx).path(), Some(&copy)));

        // Renaming a folder carries the open request inside it along.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.new_request(pets.clone(), window, cx));
            workspace.update(cx, |this, cx| this.rename_item(pets.clone(), window, cx));
        })
        .unwrap();
        type_and_confirm(cx, "Animals");
        let animals = root.join("Animals");
        let inside = animals.join("new-http-request.yaml");
        assert!(inside.exists() && !pets.exists());
        cx.update(|cx| assert_eq!(editor.read(cx).path(), Some(&inside)));

        // Deleting asks first, then closes the request that was inside.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.delete_item(animals.clone(), window, cx));
        })
        .unwrap();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(window.has_active_dialog(cx), "asks first");
            assert!(animals.exists());
            window.render_frame(cx);
            window.click("dialog-ok", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!animals.exists());
        // Its tab goes with it, leaving the request that was open in the other tab.
        cx.update(|cx| {
            assert_eq!(editor.read(cx).path(), Some(&copy));
            assert_eq!(workspace.read(cx).state.last_request.as_ref(), Some(&copy));
            assert_eq!(
                workspace.read(cx).open_tabs,
                vec![fetch.clone(), copy.clone()],
                "the other tabs stay, renamed along with their files"
            );
        });
    }

    #[gpui_kit::test]
    async fn the_scratchpad_is_always_there_for_quick_requests(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let (workspace, window) = open_workspace(cx, &paths, None);
        let scratch = project::collection_dir(&paths.data_dir.join("scratchpad"));

        // No projects: the scratchpad exists, and the welcome screen offers a quick request.
        cx.update_window(window, |_, window, cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.collections.len(), 1);
            assert_eq!(ws.collections[0].root, scratch);
            window.render_frame(cx);
            assert!(window.try_find("empty-scratch-request").is_some());
            workspace.update(cx, |this, cx| {
                this.new_scratch_request_of(RequestKind::Http, window, cx)
            });
        })
        .unwrap();
        cx.run_until_parked();
        let quick = scratch.join("new-http-request.yaml");
        assert!(quick.exists());
        cx.update(|cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.editor().read(cx).path(), Some(&quick));
            assert!(ws.state.open_projects.is_empty(), "not remembered as a project");
        });

        // Ctrl+N makes another; the scratchpad can't be closed.
        cx.update_window(window, |_, window, cx| {
            window.dispatch_action(Box::new(palette::NewScratchRequest), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let second = scratch.join("new-http-request-2.yaml");
        assert!(second.exists());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.close_collection(&scratch, window, cx));
            assert_eq!(workspace.read(cx).collections.len(), 1);
        })
        .unwrap();

        // A quick request that turned out useful moves into a project.
        let root = create_example_project(tmp.path()).unwrap();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_collection(root.clone(), window, cx);
                this.select_request(quick.clone(), window, cx);
                this.move_request(quick.clone(), root.clone(), window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        let moved = root.join("new-http-request.yaml");
        assert!(moved.exists() && !quick.exists());
        cx.update(|cx| {
            let ws = workspace.read(cx);
            assert_eq!(ws.editor().read(cx).path(), Some(&moved));
            assert_eq!(ws.collections[0].requests().len(), 1, "scratchpad keeps the other one");
            assert_eq!(ws.projects().next().unwrap().requests().len(), 3);
        });

        // Still there after a restart.
        let (restarted, _) = open_workspace(cx, &paths, None);
        cx.update(|cx| {
            let ws = restarted.read(cx);
            assert_eq!(ws.collections[0].root, scratch);
            assert_eq!(ws.collections[0].requests().len(), 1);
            assert_eq!(ws.projects().count(), 1);
        });
    }

    #[gpui_kit::test]
    async fn json_responses_filter_with_jsonpath(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (get_json, echo) = (root.join("get-json.yaml"), root.join("echo-post.yaml"));
        let body = r#"{"items":[{"id":1,"name":"Rex"},{"id":2,"name":"Tom"}]}"#;
        let (port, _received) = http_server(body);
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.url = format!("http://127.0.0.1:{port}/pets");
        storage::write_yaml(&get_json, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
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
        let filter = cx.update(|cx| editor.read(cx).response_filter_for_test());
        let set_filter = |cx: &mut TestAppContext, text: &str| {
            cx.update_window(window, |_, window, cx| {
                filter.update(cx, |s, cx| s.replace_all(text.to_string(), window, cx));
            })
            .unwrap();
            cx.run_until_parked();
        };
        let shown = |cx: &mut TestAppContext| cx.update(|cx| editor.read(cx).response_body_text(cx));

        set_filter(cx, "$.items[*].name");
        assert_eq!(shown(cx), "[\n  \"Rex\",\n  \"Tom\"\n]");
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("filter-status").is_some(), "shows the match count");
        })
        .unwrap();

        // A half-typed expression keeps the whole body visible.
        set_filter(cx, "$.items[");
        assert!(shown(cx).contains("\"Tom\""));
        assert!(shown(cx).contains("\"items\""));

        // Each request remembers its own filter.
        set_filter(cx, "$.items[0].id");
        assert_eq!(shown(cx), "1");
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.select_request(echo.clone(), window, cx));
        })
        .unwrap();
        assert_eq!(cx.update(|cx| filter.read(cx).value().to_string()), "");
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.select_request(get_json.clone(), window, cx));
        })
        .unwrap();
        assert_eq!(cx.update(|cx| filter.read(cx).value().to_string()), "$.items[0].id");
        assert_eq!(shown(cx), "1");

        // Clearing it shows everything again.
        set_filter(cx, "");
        assert!(shown(cx).contains("\"Rex\""));
    }

    #[gpui_kit::test]
    async fn new_requests_come_in_kinds(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                for kind in RequestKind::ALL {
                    this.new_request_of(root.clone(), kind, window, cx);
                }
            });
        })
        .unwrap();
        let read = |name: &str| storage::read_yaml::<RequestFile>(&root.join(name)).unwrap();
        let http = read("new-http-request.yaml");
        assert_eq!((http.method.as_str(), request_label(&http).as_str()), ("GET", "GET"));
        let graphql = read("new-graphql-request.yaml");
        assert_eq!(request_label(&graphql), "GQL");
        assert!(graphql.graphql.unwrap().query.starts_with("query {"));
        let websocket = read("new-websocket.yaml");
        assert_eq!(request_label(&websocket), "WS");
        let stream = read("new-event-stream-sse.yaml");
        assert_eq!(stream.headers[0].value, "text/event-stream");
        assert_eq!(request_label(&stream), "SSE");

        // Each kind has its own label colour, which can be changed and is saved.
        cx.update(|cx| {
            let theme = cx.theme().clone();
            assert_eq!(
                request_color(&http, &theme, cx),
                theme.success,
                "HTTP colours by method"
            );
            assert_eq!(request_color(&stream, &theme, cx), theme.blue);
            assert_eq!(request_color(&websocket, &theme, cx), theme.cyan);
        });
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.set_request_color(RequestKind::EventStream, LabelColor::Yellow, cx);
                this.set_request_color(RequestKind::Http, LabelColor::Grey, cx);
            });
            let theme = cx.theme().clone();
            assert_eq!(request_color(&stream, &theme, cx), theme.yellow);
            assert_eq!(request_color(&http, &theme, cx), theme.muted_foreground);
        });
        let saved = fs::read_to_string(paths.config_dir.join("settings.yaml")).unwrap();
        assert!(saved.contains("sse: yellow") && saved.contains("http: grey"), "{saved}");
        cx.update(|cx| {
            let ws = workspace.read(cx);
            let path = ws.editor().read(cx).path().cloned().unwrap();
            assert!(path.ends_with("new-event-stream-sse.yaml"), "the newest is open");
        });
    }

    #[gpui_kit::test]
    async fn query_params_edit_with_the_url(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (get_json, echo) = (root.join("get-json.yaml"), root.join("echo-post.yaml"));
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.select_request(get_json.clone(), window, cx));
        })
        .unwrap();
        let (url, params) = cx.update(|cx| (editor.read(cx).url_for_test(), editor.read(cx).params_for_test()));
        let text = |cx: &mut TestAppContext, e: &Entity<gpui_kit::component::input::EditorState>| {
            cx.update(|cx| e.read(cx).value().to_string())
        };

        // Typing a query in the URL fills the params.
        let (port, received) = one_shot_server("HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        let base = format!("http://127.0.0.1:{port}/json");
        cx.update_window(window, |_, window, cx| {
            url.update(cx, |s, cx| {
                s.replace_all(format!("{base}?page=2&sort=name"), window, cx)
            });
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(text(cx, &params), "page=2\nsort=name");

        // Editing params rewrites the URL; `#` switches one off without losing it.
        cx.update_window(window, |_, window, cx| {
            params.update(cx, |s, cx| {
                s.replace_all("# page=2\nsort=name\nlimit={{limit}}", window, cx)
            });
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(text(cx, &url), format!("{base}?sort=name&limit={{{{limit}}}}"));
        assert_eq!(
            text(cx, &params),
            "# page=2\nsort=name\nlimit={{limit}}",
            "not rewritten under the cursor"
        );

        // Saved with the request, and restored when coming back to it.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.select_request(echo.clone(), window, cx);
                this.select_request(get_json.clone(), window, cx);
            });
        })
        .unwrap();
        let saved: RequestFile = storage::read_yaml(&get_json).unwrap();
        assert_eq!(saved.disabled_params[0].name, "page");
        assert_eq!(text(cx, &params), "sort=name\nlimit={{limit}}\n# page=2");

        // Only enabled params are sent.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("send", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let head = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            head.starts_with("GET /json?sort=name&limit=%7B%7Blimit%7D%7D ")
                || head.starts_with("GET /json?sort=name&limit={{limit}} "),
            "{head}"
        );
    }

    #[gpui_kit::test]
    async fn auth_is_inherited_from_folders_and_the_collection(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");

        // The collection uses a bearer token; the Public folder turns auth off.
        let mut file: CollectionFile = storage::read_yaml(&root.join("collection.yaml")).unwrap();
        file.variables.insert("token".into(), "abc".into());
        file.auth = Auth::Bearer {
            token: "{{token}}".into(),
        };
        storage::save_collection_file(&root, &file).unwrap();
        let public = storage::create_folder(&root, "Public").unwrap();
        storage::write_folder(
            &public,
            &crate::model::FolderFile {
                auth: Auth::None,
                ..Default::default()
            },
        )
        .unwrap();
        let open_request = storage::create_request(&public, &RequestFile::new("Status")).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        let send_to = |cx: &mut TestAppContext, path: &Path| {
            let (port, received) = one_shot_server("HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
            let mut request: RequestFile = storage::read_yaml(path).unwrap();
            request.url = format!("http://127.0.0.1:{port}/");
            storage::write_yaml(path, &request).unwrap();
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |this, cx| {
                    this.reload_collection(&root, window, cx);
                    this.select_request(path.to_path_buf(), window, cx);
                });
                window.render_frame(cx);
                window.click("send", cx);
            })
            .unwrap();
            cx.run_until_parked();
            received
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .to_ascii_lowercase()
        };

        let head = send_to(cx, &get_json);
        assert!(head.contains("authorization: bearer abc"), "{head}");
        cx.update(|cx| {
            assert!(matches!(editor.read(cx).inherited_auth_for_test(), Auth::Bearer { .. }));
        });
        let head = send_to(cx, &open_request);
        assert!(!head.contains("authorization"), "{head}");

        // A literal token typed into a request moves into a secret, leaving a placeholder.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.select_request(get_json.clone(), window, cx));
            let form = editor.read(cx).auth_form_for_test();
            form.update(cx, |form, cx| {
                form.set(&Auth::Bearer { token: "s3cr3t".into() }, window, cx);
                cx.emit(AuthFormEvent::MoveToSecret);
            });
        })
        .unwrap();
        cx.run_until_parked();
        let saved: RequestFile = storage::read_yaml(&get_json).unwrap();
        assert_eq!(
            saved.auth,
            Auth::Bearer {
                token: "{{token_2}}".into()
            },
            "`token` is already a variable"
        );
        let file: CollectionFile = storage::read_yaml(&root.join("collection.yaml")).unwrap();
        assert!(file.secrets.contains(&"token_2".to_string()));
        assert!(!fs::read_to_string(&get_json).unwrap().contains("s3cr3t"));

        // Folder auth is edited in a dialog.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.edit_auth(public.clone(), window, cx));
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.render_frame(cx);
            window.click("dialog-ok", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(storage::read_folder(&public).auth, Auth::None, "kept as it was");
    }

    #[gpui_kit::test]
    async fn cookies_from_responses_are_sent_back_and_saved(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        let send = |cx: &mut TestAppContext, response: &'static str| {
            let (port, received) = one_shot_server(response);
            let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
            request.url = format!("http://127.0.0.1:{port}/app");
            storage::write_yaml(&get_json, &request).unwrap();
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |this, cx| {
                    this.reload_collection(&root, window, cx);
                    this.select_request(get_json.clone(), window, cx);
                });
                window.render_frame(cx);
                window.click("send", cx);
            })
            .unwrap();
            let mut head = None;
            for _ in 0..500 {
                cx.run_until_parked();
                if head.is_none() {
                    head = received.try_recv().ok();
                }
                if head.is_some() && !cx.update(|cx| editor.read(cx).is_sending()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            head.expect("the server got the request").to_ascii_lowercase()
        };

        let first = send(
            cx,
            "HTTP/1.1 200 OK\r\nSet-Cookie: session=s1; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert!(!first.contains("cookie:"), "{first}");
        let second = send(cx, "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        assert!(second.contains("cookie: session=s1"), "sent back: {second}");

        let jar = cx.update(|cx| workspace.update(cx, |this, cx| this.cookies_for(&root, cx).unwrap()));
        assert_eq!(jar.list()[0].name, "session");
        let id = storage::read_yaml::<CollectionFile>(&root.join("collection.yaml"))
            .unwrap()
            .id
            .unwrap();
        let saved = stored_secret(cx, &workspace, &Cookies::secret(&id)).expect("saved in the secret store");
        assert!(saved.contains("session"));

        // The dialog lists them; clearing empties the jar and its file.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.manage_cookies(root.clone(), window, cx));
        })
        .unwrap();
        // Let the dialog finish animating in, so clicks land where the buttons end up.
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(("cookie-delete", 0usize)).is_some());
            window.click("cookies-clear", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(jar.list().is_empty());
        assert_eq!(
            stored_secret(cx, &workspace, &Cookies::secret(&id)),
            None,
            "saved jar removed"
        );
        assert!(jar.list().is_empty());
    }

    #[gpui_kit::test]
    async fn requests_copy_as_curl_without_or_with_secrets(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");
        let mut file: CollectionFile = storage::read_yaml(&root.join("collection.yaml")).unwrap();
        file.ensure_id();
        file.secrets.push("token".into());
        storage::save_collection_file(&root, &file).unwrap();
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.auth = Auth::Bearer {
            token: "{{token}}".into(),
        };
        storage::write_yaml(&get_json, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.store_secrets(
                    vec![SecretWrite::new(
                        &file,
                        DEFAULTS_SCOPE,
                        DEFAULTS_LABEL,
                        "token",
                        "t0p-secret",
                    )],
                    window,
                    cx,
                );
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.copy_as_curl(get_json.clone(), false, window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        let clipboard = |cx: &mut TestAppContext| cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
        let shared = clipboard(cx);
        assert!(shared.starts_with("curl -L 'https://httpbin.org/json'"), "{shared}");
        assert!(shared.contains("-H 'Authorization: Bearer {{token}}'"), "{shared}");
        assert!(!shared.contains("t0p-secret"));

        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.copy_as_curl(get_json.clone(), true, window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        assert!(clipboard(cx).contains("Bearer t0p-secret"));
    }

    #[gpui_kit::test]
    async fn earlier_responses_stay_in_the_history(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        let send = |cx: &mut TestAppContext, body: &'static str, history: usize| {
            let (port, _received) = http_server(body);
            let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
            request.url = format!("http://127.0.0.1:{port}/");
            storage::write_yaml(&get_json, &request).unwrap();
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
                let done = cx.update(|cx| {
                    let editor = editor.read(cx);
                    !editor.is_sending() && editor.shown_response().is_some() && editor.history_len() == history
                });
                if done {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        send(cx, "{\"version\":1}", 0);
        send(cx, "{\"version\":2}", 1);
        let body = |cx: &mut TestAppContext| cx.update(|cx| editor.read(cx).response_body_text(cx));
        assert_eq!(cx.update(|cx| editor.read(cx).history_len()), 1);
        assert!(body(cx).contains("2"));

        cx.update_window(window, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.view_history(Some(0), window, cx));
        })
        .unwrap();
        assert!(body(cx).contains("\"version\": 1"), "{}", body(cx));
        cx.update_window(window, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.view_history(None, window, cx));
        })
        .unwrap();
        assert!(body(cx).contains("\"version\": 2"));

        // The history is saved with the response and comes back after a restart.
        let (restarted, window) = open_workspace(cx, &paths, launch(&root));
        cx.update_window(window, |_, window, cx| {
            restarted.update(cx, |this, cx| this.select_request(get_json.clone(), window, cx));
        })
        .unwrap();
        let editor = cx.update(|cx| restarted.read(cx).editor());
        assert_eq!(cx.update(|cx| editor.read(cx).history_len()), 1);
    }

    #[gpui_kit::test]
    async fn requests_chain_on_other_responses(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();

        let (login_port, login_received) = http_server("{\"token\":\"abc\",\"user\":{\"id\":7}}");
        let mut login = RequestFile::new("Login");
        login.method = "POST".into();
        login.url = format!("http://127.0.0.1:{login_port}/login");
        let login_path = storage::create_request(&root, &login).unwrap();

        let (me_port, me_received) = one_shot_server("HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        let mut me = RequestFile::new("Me");
        me.url = format!("http://127.0.0.1:{me_port}/users/{{{{ response(\"Login\", \"$.user.id\") }}}}");
        me.auth = Auth::Bearer {
            token: "{{response(\"Login\", \"$.token\")}}".into(),
        };
        let me_path = storage::create_request(&root, &me).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        let send = |cx: &mut TestAppContext, path: &Path| {
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |this, cx| {
                    this.reload_collection(&root, window, cx);
                    this.select_request(path.to_path_buf(), window, cx);
                });
                window.render_frame(cx);
                window.click("send", cx);
            })
            .unwrap();
            for _ in 0..500 {
                cx.run_until_parked();
                if cx.update(|cx| !editor.read(cx).is_sending() && editor.read(cx).shown_response().is_some()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };

        // Login has never been sent, so sending Me sends it first and uses its response.
        send(cx, &me_path);
        let (login_head, _) = login_received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(login_head.starts_with("POST /login "));
        let head = me_received
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .to_ascii_lowercase();
        assert!(head.starts_with("get /users/7 "), "{head}");
        assert!(head.contains("authorization: bearer abc"), "{head}");
        cx.update(|cx| {
            let editor = editor.read(cx);
            assert!(editor.response_for(&login_path).is_some(), "Login's response is kept");
            let (response, _) = editor.shown_response().unwrap();
            assert!(matches!(
                response.outcome,
                crate::response_cache::Outcome::Response { status: 204, .. }
            ));
        });

        // The next send reuses Login's latest response instead of sending it again (its
        // one-shot server is gone, so a resend would fail).
        let (me_port, me_received) = one_shot_server("HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        let mut me: RequestFile = storage::read_yaml(&me_path).unwrap();
        me.url = format!("http://127.0.0.1:{me_port}/again/{{{{ response(\"Login\", \"$.user.id\") }}}}");
        storage::write_yaml(&me_path, &me).unwrap();
        send(cx, &me_path);
        let head = me_received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.starts_with("GET /again/7 "), "{head}");

        // A reference to a request that doesn't exist fails the send with a reason.
        me.url = "http://127.0.0.1:1/{{ response(\"Nope\", \"$\") }}".into();
        storage::write_yaml(&me_path, &me).unwrap();
        send(cx, &me_path);
        cx.update(|cx| {
            let (response, _) = editor.read(cx).shown_response().unwrap();
            let crate::response_cache::Outcome::Error { message } = &response.outcome else {
                panic!("{:?}", response.outcome)
            };
            assert!(message.contains("no request named \"Nope\""), "{message}");
        });

        // References are copied from the request menu.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.copy_response_reference(login_path.clone(), window, cx)
            });
        })
        .unwrap();
        let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
        assert_eq!(copied, "{{ response(\"Login\", \"$\") }}");
    }

    #[gpui_kit::test]
    async fn template_calls_autocomplete_from_the_collection(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (port, _received) = http_server("{\"token\":\"abc\",\"user\":{\"id\":7}}");
        let mut login = RequestFile::new("Login");
        login.method = "POST".into();
        login.url = format!("http://127.0.0.1:{port}/login");
        let login_path = storage::create_request(&root, &login).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.select_request(login_path.clone(), window, cx);
            });
            window.render_frame(cx);
            window.click("send", cx);
        })
        .unwrap();
        for _ in 0..300 {
            cx.run_until_parked();
            if cx.update(|cx| !editor.read(cx).is_sending() && editor.read(cx).shown_response().is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.select_request(root.join("get-json.yaml"), window, cx)
            });
        })
        .unwrap();

        let suggest = |cx: &mut TestAppContext, text: &str| -> Vec<String> {
            cx.update(|cx| {
                editor
                    .read(cx)
                    .template_suggestions(text, text.len(), cx)
                    .map(|(_, suggestions)| suggestions.into_iter().map(|s| s.insert).collect())
                    .unwrap_or_default()
            })
        };
        assert_eq!(suggest(cx, "{{ base"), ["base_url"]);
        assert!(suggest(cx, "{{ ").contains(&"response(\"".to_string()));
        assert_eq!(suggest(cx, "{{ response(\"Lo"), ["Login"]);
        assert_eq!(suggest(cx, "{{ response(\"Login\", \"$"), ["$.token", "$.user"]);
        assert_eq!(suggest(cx, "{{ response(\"Login\", \"$.user."), ["$.user.id"]);
        assert_eq!(
            suggest(cx, "{{ response_header(\"Login\", \"Content-T"),
            ["content-type"]
        );
        assert_eq!(suggest(cx, "{{ response(\"Login\", \"$.token\", \"al"), ["always"]);
        assert!(suggest(cx, "no template here").is_empty());

        // The editors that take templates offer these completions.
        cx.update(|cx| {
            let url = editor.read(cx).url_for_test();
            assert!(url.read(cx).lsp().completion_provider.is_some());
        });
    }

    #[gpui_kit::test]
    async fn request_settings_inherit_and_change_how_requests_are_sent(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let get_json = root.join("get-json.yaml");

        // The collection doesn't follow redirects.
        let mut file: CollectionFile = storage::read_yaml(&root.join("collection.yaml")).unwrap();
        file.settings.follow_redirects = Some(false);
        storage::save_collection_file(&root, &file).unwrap();
        let (port, _received) = one_shot_server(
            "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.url = format!("http://127.0.0.1:{port}/start");
        storage::write_yaml(&get_json, &request).unwrap();

        // A Docker folder sends over a Unix socket.
        let docker = storage::create_folder(&root, "Docker").unwrap();
        let socket = tmp.path().join("docker.sock");
        storage::write_folder(
            &docker,
            &crate::model::FolderFile {
                settings: RequestSettings {
                    unix_socket: Some(socket.display().to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]")
                .unwrap();
        });
        let mut containers = RequestFile::new("Containers");
        containers.url = "http://localhost/v1.45/containers/json".into();
        let containers = storage::create_request(&docker, &containers).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        let send = |cx: &mut TestAppContext, path: &Path| -> u16 {
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |this, cx| {
                    this.reload_collection(&root, window, cx);
                    this.select_request(path.to_path_buf(), window, cx);
                });
                window.render_frame(cx);
                window.click("send", cx);
            })
            .unwrap();
            for _ in 0..300 {
                cx.run_until_parked();
                if cx.update(|cx| !editor.read(cx).is_sending() && editor.read(cx).shown_response().is_some()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            cx.update(|cx| match &editor.read(cx).shown_response().unwrap().0.outcome {
                crate::response_cache::Outcome::Response { status, .. } => *status,
                crate::response_cache::Outcome::Error { message } => panic!("{message}"),
            })
        };
        assert_eq!(send(cx, &get_json), 302, "redirect not followed");
        assert_eq!(send(cx, &containers), 200, "sent over the socket");

        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.copy_as_curl(containers.clone(), false, window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        let curl = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
        assert!(
            curl.contains(&format!("--unix-socket '{}'", socket.display())),
            "{curl}"
        );

        // The dialog edits a request's own settings.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.edit_settings(get_json.clone(), window, cx));
        })
        .unwrap();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            assert!(window.has_active_dialog(cx));
            window.render_frame(cx);
            window.click("dialog-ok", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| assert!(!window.has_active_dialog(cx)))
            .unwrap();
        let saved: RequestFile = storage::read_yaml(&get_json).unwrap();
        assert!(saved.settings.is_empty(), "nothing changed, nothing written");
    }

    #[gpui_kit::test]
    async fn checks_run_on_responses_and_can_be_added_from_the_filter(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (get_json, echo) = (root.join("get-json.yaml"), root.join("echo-post.yaml"));
        let (port, _received) = http_server("{\"id\":7,\"name\":\"Rex\"}");
        let mut request: RequestFile = storage::read_yaml(&get_json).unwrap();
        request.url = format!("http://127.0.0.1:{port}/pets/7");
        request.checks = vec![
            "status == 200".into(),
            "$.id == 7".into(),
            "$.name == Max".into(),
            "# time < 1".into(),
        ];
        storage::write_yaml(&get_json, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
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
            if cx.update(|cx| !editor.read(cx).is_sending() && editor.read(cx).shown_response().is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let results = |cx: &mut TestAppContext| cx.update(|cx| editor.read(cx).check_results_for_test());
        assert_eq!(
            results(cx),
            Some(vec![
                ("status == 200".to_string(), true),
                ("$.id == 7".to_string(), true),
                ("$.name == Max".to_string(), false),
            ]),
            "switched-off checks don't run"
        );
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("checks-badge").is_some());
        })
        .unwrap();

        // Filter to a value and turn it into a check.
        let filter = cx.update(|cx| editor.read(cx).response_filter_for_test());
        cx.update_window(window, |_, window, cx| {
            filter.update(cx, |s, cx| s.replace_all("$.name", window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("filter-add-check", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            results(cx).unwrap().last(),
            Some(&("$.name == \"Rex\"".to_string(), true))
        );

        // Checks are saved with the request.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.select_request(echo.clone(), window, cx));
        })
        .unwrap();
        let saved: RequestFile = storage::read_yaml(&get_json).unwrap();
        assert_eq!(saved.checks.len(), 5);
        assert_eq!(saved.checks[4], "$.name == \"Rex\"");
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
            assert_eq!(workspace.read(cx).editor().read(cx).path(), Some(&echo));
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

    /// Answers `count` HTTP requests on one port, in order, with the same JSON reply.
    fn http_server_serving(count: usize, reply: &'static str) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{BufRead as _, Read as _, Write as _};
            for _ in 0..count {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut length = 0;
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
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        port
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
            let headers = workspace.read(cx).editor().read(cx).headers_entity();
            headers.focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
            window.press("ctrl-enter", cx);
        })
        .unwrap();
        // Events from the key press are delivered when that update ends, so the send has
        // started (but not finished) before this switch.
        cx.update_window(window, |_, window, cx| {
            assert!(workspace.read(cx).editor().read(cx).is_sending(), "send started");
            workspace.update(cx, |this, cx| this.select_request(echo.clone(), window, cx));
        })
        .unwrap();
        for _ in 0..500 {
            cx.run_until_parked();
            if cx.update(|cx| workspace.read(cx).editor().read(cx).response_for(&get_json).is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cx.update(|cx| {
            let editor = workspace.read(cx).editor().read(cx);
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
            let editor = workspace.read(cx).editor().read(cx);
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
            let editor = restarted.read(cx).editor().read(cx);
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
            if cx.update(|cx| workspace.read(cx).editor().read(cx).received_bytes() > 3) {
                break;
            }
        }
        cx.update_window(window, |_, window, cx| {
            assert!(workspace.read(cx).editor().read(cx).is_sending());
            assert!(
                workspace.read(cx).editor().read(cx).received_bytes() > 3,
                "body is streaming in"
            );
            window.render_frame(cx);
            window.click("send", cx); // now labelled Cancel
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let editor = workspace.read(cx).editor().read(cx);
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
        let editor = cx.update(|cx| workspace.read(cx).editor());
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
            content: "{\"hello\": \"{{base_url}}\", \"id\": \"{{ uuid() }}\"}".into(),
        });
        storage::write_yaml(&get_json, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
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
                detail.contains("echo: {\"hello\": \"https://httpbin.org\", \"id\": \""),
                "{detail}"
            );
            assert!(
                !detail.contains("uuid()"),
                "functions in messages are evaluated: {detail}"
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
        let editor = cx.update(|cx| workspace.read(cx).editor());
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
        let (port, received) = http_server(crate::graphql::fixtures::PETSTORE);
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
        let editor = cx.update(|cx| workspace.read(cx).editor());
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
        let editor = cx.update(|cx| restarted.read(cx).editor());
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
                ws.editor().read(cx).secret_names().contains(&"api_token".to_string()),
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
        let headers = cx.update(|cx| workspace.read(cx).editor().read(cx).headers_entity());
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
                this.editor().update(cx, |editor, cx| {
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

    #[gpui_kit::test]
    async fn running_a_collection_reports_checks_and_keeps_the_responses(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let port = http_server_serving(2, "{\"id\":7}");
        for (name, checks) in [
            (
                "get-json.yaml",
                vec!["status == 200".to_string(), "$.id == 7".to_string()],
            ),
            ("echo-post.yaml", vec!["status == 500".to_string()]),
        ] {
            let path = root.join(name);
            let mut request: RequestFile = storage::read_yaml(&path).unwrap();
            request.url = format!("http://127.0.0.1:{port}/thing");
            request.checks = checks;
            storage::write_yaml(&path, &request).unwrap();
        }

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let runner = cx.update(|cx| workspace.read(cx).runner.clone());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.run_scope(root.clone(), window, cx);
            });
        })
        .unwrap();
        for _ in 0..300 {
            cx.run_until_parked();
            if cx.update(|cx| !runner.read(cx).running()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let results = cx.update(|cx| runner.read(cx).results_for_test());
        assert_eq!(
            results,
            vec![
                ("get-json.yaml".to_string(), crate::runner::RunStatus::Passed),
                ("echo-post.yaml".to_string(), crate::runner::RunStatus::Failed),
            ],
            "both ran, in sidebar order, and the checks decided the outcome"
        );
        let (passed, failed) = cx.update(|cx| {
            let summary = runner.read(cx).summary_for_test().cloned().unwrap();
            (summary.passed, summary.failed)
        });
        assert_eq!((passed, failed), (1, 1));
        cx.update(|cx| assert!(workspace.read(cx).main_view == MainView::Runner));

        // The run's responses become each request's latest, so opening one shows it.
        let editor = cx.update(|cx| workspace.read(cx).editor());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.select_request(root.join("get-json.yaml"), window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        let shown = cx.update(|cx| editor.read(cx).shown_response().map(|(r, _)| r.clone()));
        assert!(
            matches!(shown.map(|r| r.outcome), Some(crate::response_cache::Outcome::Response { status, .. }) if status == 200),
            "the editor shows the response from the run"
        );
    }

    #[gpui_kit::test]
    async fn multipart_bodies_upload_files_from_the_project(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let project = project::project_dir(&root).to_path_buf();
        fs::write(project.join("rex.txt"), "good dog").unwrap();
        let (port, received) = http_server("{\"ok\":true}");
        let path = root.join("echo-post.yaml");
        let mut request: RequestFile = storage::read_yaml(&path).unwrap();
        request.url = format!("http://127.0.0.1:{port}/photos");
        request.body = Some(crate::model::Body {
            kind: crate::model::BodyKind::Multipart,
            content: "name: Rex\nphoto: @rex.txt".into(),
        });
        storage::write_yaml(&path, &request).unwrap();

        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_collection(&root, window, cx);
                this.select_request(path.clone(), window, cx);
            });
            window.render_frame(cx);
            window.click("send", cx);
        })
        .unwrap();
        for _ in 0..300 {
            cx.run_until_parked();
            if cx.update(|cx| !editor.read(cx).is_sending() && editor.read(cx).shown_response().is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let (head, body) = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            head.to_ascii_lowercase().contains("multipart/form-data; boundary="),
            "{head}"
        );
        assert!(body.contains("name=\"name\"") && body.contains("Rex"), "{body}");
        assert!(
            body.contains("filename=\"rex.txt\"") && body.contains("good dog"),
            "the file next to the project went up with the request:\n{body}"
        );
        // The editor keeps the kind, so saving doesn't turn the parts list into a text body.
        cx.update(|cx| {
            let saved: RequestFile = storage::read_yaml(&path).unwrap();
            assert_eq!(
                saved.body.as_ref().map(|b| b.kind),
                Some(crate::model::BodyKind::Multipart)
            );
            assert_eq!(editor.read(cx).current_for_test(cx).body, saved.body);
        });
    }

    #[gpui_kit::test]
    async fn the_sidebar_filters_and_requests_can_be_dragged(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let pets = storage::create_folder(&root, "Pets").unwrap();
        let (get_json, echo) = (root.join("get-json.yaml"), root.join("echo-post.yaml"));
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let row = |kind: &str, path: &Path| format!("{kind}:{}", path.display());
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.reload_collection(&root, window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        cx.update_window(window, |_, window, _| {
            assert!(window.try_find(row("request", &get_json)).is_some());
            assert!(window.try_find(row("request", &echo)).is_some());
        })
        .unwrap();

        // Filtering hides what doesn't match, by name, method or URL.
        let search = cx.update(|cx| workspace.read(cx).search.clone());
        let filter = |cx: &mut TestAppContext, text: &str| {
            cx.update_window(window, |_, window, cx| {
                search.update(cx, |s, cx| s.set_value(text, window, cx));
                window.render_frame(cx);
            })
            .unwrap();
        };
        filter(cx, "echo");
        cx.update_window(window, |_, window, _| {
            assert!(window.try_find(row("request", &echo)).is_some(), "the match stays");
            assert!(window.try_find(row("request", &get_json)).is_none(), "the rest goes");
            assert!(window.try_find(row("folder", &pets)).is_none(), "so do empty folders");
        })
        .unwrap();
        filter(cx, "post any");
        cx.update_window(window, |_, window, _| {
            assert!(
                window.try_find(row("request", &echo)).is_some(),
                "every word counts, across the method and the URL"
            );
        })
        .unwrap();
        filter(cx, "");

        // Dragging a request onto a folder moves it there.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.drag_to(row("request", &echo), row("folder", &pets), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let moved = pets.join("echo-post.yaml");
        assert!(moved.exists() && !echo.exists(), "the file moved into the folder");

        // Dragging onto a request puts it straight after that one.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.drag_to(row("request", &moved), row("request", &get_json), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let back = root.join("echo-post.yaml");
        assert!(back.exists() && !moved.exists());
        let order: Vec<String> = cx.update(|cx| {
            workspace
                .read(cx)
                .collections
                .iter()
                .find(|c| c.root == root)
                .unwrap()
                .requests()
                .into_iter()
                .map(|entry| entry.request.name.clone())
                .collect()
        });
        assert_eq!(
            order.first().map(String::as_str),
            Some("Get JSON"),
            "and the order sticks: {order:?}"
        );
        assert_eq!(order.get(1).map(String::as_str), Some("Echo POST"), "{order:?}");
    }

    #[gpui_kit::test]
    async fn open_requests_get_tabs_that_survive_a_restart(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (get_json, echo) = (root.join("get-json.yaml"), root.join("echo-post.yaml"));
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());
        let open = |cx: &mut TestAppContext, path: &Path| {
            let path = path.to_path_buf();
            cx.update_window(window, |_, window, cx| {
                workspace.update(cx, |this, cx| this.select_request(path.clone(), window, cx));
                window.render_frame(cx);
            })
            .unwrap();
        };
        open(cx, &get_json);
        open(cx, &echo);
        cx.update(|cx| {
            assert_eq!(
                workspace.read(cx).open_tabs,
                vec![get_json.clone(), echo.clone()],
                "each request opened gets its own tab"
            );
            assert_eq!(editor.read(cx).path(), Some(&echo));
        });

        // Opening one that's already open moves to its tab instead of adding another.
        open(cx, &get_json);
        cx.update(|cx| {
            assert_eq!(workspace.read(cx).open_tabs.len(), 2);
            assert_eq!(workspace.read(cx).active, 0);
        });

        // Clicking a tab shows it; closing one leaves the other.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(format!("tab:{}", echo.display()), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(editor.read(cx).path(), Some(&echo)));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(("close-tab", 1usize), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(workspace.read(cx).open_tabs, vec![get_json.clone()]);
            assert_eq!(editor.read(cx).path(), Some(&get_json), "the neighbour takes over");
        });

        // A restart brings the tabs back.
        open(cx, &echo);
        let (restarted, _) = open_workspace(cx, &paths, launch(&root));
        cx.update(|cx| {
            assert_eq!(restarted.read(cx).open_tabs, vec![get_json.clone(), echo.clone()]);
            assert_eq!(restarted.read(cx).editor().read(cx).path(), Some(&echo));
        });
    }

    #[gpui_kit::test]
    async fn a_pasted_curl_command_becomes_a_request(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = setup(cx, tmp.path());
        let root = create_example_project(tmp.path()).unwrap();
        let (workspace, window) = open_workspace(cx, &paths, launch(&root));
        let editor = cx.update(|cx| workspace.read(cx).editor());

        // Into the URL bar: the open request is filled in, not overwritten with the text.
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.select_request(root.join("get-json.yaml"), window, cx)
            });
            let url = editor.read(cx).url_for_test();
            url.update(cx, |state, cx| {
                state.replace_all(
                    "curl -X POST 'https://api.test/pets?big=1' -H 'Accept: application/json' -d '{\"name\":\"Rex\"}'",
                    window,
                    cx,
                )
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let request = editor.read(cx).current_for_test(cx);
            assert_eq!(request.method, "POST");
            assert_eq!(request.url, "https://api.test/pets?big=1");
            assert_eq!(request.name, "Get JSON", "its name is left alone");
            assert!(request.headers.iter().any(|h| h.name == "Accept"));
            assert_eq!(
                request.body.as_ref().map(|b| b.content.as_str()),
                Some("{\"name\":\"Rex\"}")
            );
        });

        // Anywhere else: a new request in the folder you're working in.
        cx.update(|cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(
                "curl https://api.test/ping -H 'X-Trace: abc'".into(),
            ))
        });
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.paste_curl(window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let path = editor.read(cx).path().cloned().unwrap();
            assert!(path.starts_with(&root) && path != root.join("get-json.yaml"));
            let made: RequestFile = storage::read_yaml(&path).unwrap();
            assert_eq!(
                (made.method.as_str(), made.url.as_str()),
                ("GET", "https://api.test/ping")
            );
        });
    }
}
