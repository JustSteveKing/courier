//! Managing a collection's environments: create, rename, duplicate, delete, and edit
//! variables and secrets, plus the collection-level defaults every environment inherits.
//!
//! Secret values never touch YAML: files list secret names, and values go to the
//! [`SecretStore`]. Typed values are cleared from the inputs once saved.

use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{EditorState, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::credentials::looks_sensitive_name;
use crate::ui::{code_editor, secret_input, text_input};
use crate::model::{CollectionFile, EnvironmentFile, Variables, variables_from_text, variables_to_text};
use crate::secret_store::{self, DEFAULTS_LABEL, DEFAULTS_SCOPE, SecretRef, SecretStore, SecretWrite};
use crate::storage::{self, Collection};

const CONTEXT: &str = "EnvironmentEditor";

gpui_kit::actions!(environment_editor, [SaveEnvironment]);

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("ctrl-s", SaveEnvironment, Some(CONTEXT))]);
}

/// What is being edited: the collection's own defaults, or one environment file.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Defaults,
    Environment(PathBuf),
}

pub enum EnvironmentEditorEvent {
    /// Files under this collection root changed; reload it.
    Changed(PathBuf),
    Deleted { root: PathBuf, path: PathBuf },
    Error(String),
    Close,
}

impl EventEmitter<EnvironmentEditorEvent> for EnvironmentEditor {}

/// What the store holds for a secret row.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stored {
    Checking,
    Set,
    NotSet,
    /// The store couldn't be reached or read.
    Unknown,
}

struct SecretRow {
    name: String,
    input: Entity<InputState>,
    stored: Stored,
    /// The stored value, once fetched with "Reveal". Equal input text is not a change.
    revealed: Option<String>,
    /// Added in this editing session and not yet in the YAML file.
    is_new: bool,
    /// Whether the value is currently shown unmasked.
    shown: bool,
    _subscription: Subscription,
}

/// Name, variables and secret names as saved on disk for the current target.
struct Saved {
    name: String,
    variables: Variables,
    secrets: Vec<String>,
}

pub struct EnvironmentEditor {
    focus_handle: FocusHandle,
    store: Option<SecretStore>,
    collection: Option<Collection>,
    active: Option<PathBuf>,
    target: Target,
    name: Entity<InputState>,
    variables: Entity<EditorState>,
    secret_rows: Vec<SecretRow>,
    /// Saved secrets removed in this session; their values are deleted on save.
    removed_secrets: Vec<String>,
    new_secret: Entity<InputState>,
    /// Bumped on every target load so late store lookups for an old target are ignored.
    generation: u64,
    dirty: bool,
    error: Option<String>,
}

impl EnvironmentEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(t!("env.name_placeholder").to_string()));
        let variables = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("yaml")
                .placeholder("base_url: https://api.example.com")
        });
        let new_secret = cx.new(|cx| InputState::new(window, cx).placeholder(t!("secrets.new_name_placeholder").to_string()));
        cx.subscribe(&name, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.update_dirty(cx);
            }
        })
        .detach();
        cx.subscribe(&variables, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.update_dirty(cx);
            }
        })
        .detach();
        cx.subscribe_in(&new_secret, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.add_secret(window, cx);
            }
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            store: None,
            collection: None,
            active: None,
            target: Target::Defaults,
            name,
            variables,
            secret_rows: Vec::new(),
            removed_secrets: Vec::new(),
            new_secret,
            generation: 0,
            dirty: false,
            error: None,
        }
    }

    pub fn root(&self) -> Option<&Path> {
        self.collection.as_ref().map(|c| c.root.as_path())
    }

    pub fn set_store(&mut self, store: SecretStore, window: &mut Window, cx: &mut Context<Self>) {
        self.store = Some(store);
        self.check_stored(window, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub fn variables_focus_handle(&self, cx: &App) -> FocusHandle {
        self.variables.focus_handle(cx)
    }

    #[cfg(test)]
    pub fn new_secret_focus_handle(&self, cx: &App) -> FocusHandle {
        self.new_secret.focus_handle(cx)
    }

    #[cfg(test)]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    #[cfg(test)]
    pub fn name_value(&self, cx: &App) -> SharedString {
        self.name.read(cx).value()
    }

    /// Re-applies strings set at construction after the interface language changes. Strings
    /// built during render update on their own.
    pub fn relocalize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.name
            .update(cx, |s, cx| s.set_placeholder(t!("env.name_placeholder").to_string(), window, cx));
        self.new_secret
            .update(cx, |s, cx| s.set_placeholder(t!("secrets.new_name_placeholder").to_string(), window, cx));
        cx.notify();
    }

    pub fn set_active(&mut self, active: Option<PathBuf>, cx: &mut Context<Self>) {
        self.active = active;
        cx.notify();
    }

    pub fn open(&mut self, collection: Collection, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.collection = Some(collection);
        self.load_target(target, window, cx);
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.collection = None;
        self.secret_rows.clear();
        self.removed_secrets.clear();
        self.dirty = false;
        self.error = None;
        cx.notify();
    }

    /// Takes a freshly reloaded copy of the collection. Unsaved edits are kept; otherwise
    /// the fields are refreshed in case the files changed.
    pub fn update_collection(&mut self, collection: Collection, window: &mut Window, cx: &mut Context<Self>) {
        if self.root() != Some(&collection.root) {
            return;
        }
        self.collection = Some(collection);
        let target_exists = match &self.target {
            Target::Defaults => true,
            Target::Environment(path) => self.environment(path).is_some(),
        };
        if !target_exists {
            self.load_target(Target::Defaults, window, cx);
        } else if !self.dirty {
            self.load_target(self.target.clone(), window, cx);
        }
        cx.notify();
    }

    fn environment(&self, path: &Path) -> Option<&EnvironmentFile> {
        self.collection.as_ref()?.environment(path).map(|e| &e.file)
    }

    fn saved(&self) -> Option<Saved> {
        let collection = self.collection.as_ref()?;
        Some(match &self.target {
            Target::Defaults => Saved {
                name: collection.file.name.clone(),
                variables: collection.file.variables.clone(),
                secrets: collection.file.secrets.clone(),
            },
            Target::Environment(path) => {
                let env = self.environment(path)?;
                Saved { name: env.name.clone(), variables: env.variables.clone(), secrets: env.secrets.clone() }
            }
        })
    }

    fn scope_for(target: &Target) -> String {
        match target {
            Target::Defaults => DEFAULTS_SCOPE.into(),
            Target::Environment(path) => SecretRef::environment_scope(path),
        }
    }

    fn secret_ref(&self, target: &Target, name: &str) -> SecretRef {
        let id = self.collection.as_ref().and_then(|c| c.file.id.clone()).unwrap_or_default();
        SecretRef::new(id, Self::scope_for(target), name)
    }

    fn secret_label(&self, scope_name: &str, name: &str) -> String {
        let collection = self.collection.as_ref().map(|c| c.file.name.as_str()).unwrap_or_default();
        secret_store::label(collection, scope_name, name)
    }

    fn load_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.target = target;
        self.generation += 1;
        let saved = self.saved().unwrap_or(Saved { name: String::new(), variables: Variables::new(), secrets: vec![] });
        self.name.update(cx, |s, cx| s.set_value(saved.name, window, cx));
        self.variables
            .update(cx, |s, cx| s.set_value(variables_to_text(&saved.variables), window, cx));
        self.secret_rows = saved
            .secrets
            .into_iter()
            .map(|name| self.new_row(name, None, false, window, cx))
            .collect();
        self.removed_secrets.clear();
        self.dirty = false;
        self.error = None;
        self.check_stored(window, cx);
        cx.notify();
    }

    fn new_row(
        &self,
        name: String,
        value: Option<String>,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SecretRow {
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).masked(true);
            if let Some(value) = &value {
                state = state.default_value(value.clone());
            }
            state
        });
        let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.update_dirty(cx);
            }
        });
        SecretRow {
            name,
            input,
            stored: if is_new { Stored::NotSet } else { Stored::Checking },
            revealed: None,
            is_new,
            shown: false,
            _subscription: subscription,
        }
    }

    /// Asks the store which saved secrets have a value on this machine.
    fn check_stored(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let generation = self.generation;
        let refs: Vec<_> = self
            .secret_rows
            .iter()
            .filter(|row| !row.is_new)
            .map(|row| (row.name.clone(), self.secret_ref(&self.target, &row.name)))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    let mut results = Vec::new();
                    for (name, secret) in refs {
                        let stored = match store.get(&secret).await {
                            Ok(Some(_)) => Stored::Set,
                            Ok(None) => Stored::NotSet,
                            Err(e) => {
                                eprintln!("checking secret {name}: {e:#}");
                                Stored::Unknown
                            }
                        };
                        results.push((name, stored));
                    }
                    results
                })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                for (name, stored) in results {
                    if let Some(row) = this.secret_rows.iter_mut().find(|r| r.name == name && !r.is_new) {
                        row.stored = stored;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn pending_value(row: &SecretRow, cx: &App) -> Option<String> {
        let value = row.input.read(cx).value().to_string();
        (!value.is_empty() && row.revealed.as_deref() != Some(value.as_str())).then_some(value)
    }

    /// Whether anything differs from what is saved. Computed from the inputs rather than
    /// cached, so a save right after a keystroke never misses the edit.
    fn is_modified(&self, cx: &App) -> bool {
        let Some(saved) = self.saved() else {
            return false;
        };
        let name = self.name.read(cx).value().trim().to_string();
        let variables_changed = match variables_from_text(&self.variables.read(cx).value()) {
            Ok(vars) => vars != saved.variables,
            Err(_) => true,
        };
        let names: Vec<_> = self.secret_rows.iter().map(|r| r.name.clone()).collect();
        name != saved.name
            || variables_changed
            || names != saved.secrets
            || self.secret_rows.iter().any(|row| Self::pending_value(row, cx).is_some())
    }

    fn update_dirty(&mut self, cx: &mut Context<Self>) {
        self.dirty = self.is_modified(cx);
        // Only clear errors while typing; new ones are shown when saving.
        if variables_from_text(&self.variables.read(cx).value()).is_ok() {
            self.error = None;
        }
        cx.notify();
    }

    fn fail(&mut self, message: impl Into<String>, cx: &mut Context<Self>) -> bool {
        self.error = Some(message.into());
        cx.notify();
        false
    }

    /// Saves pending edits. Returns false, leaving the edits in place, if they are invalid.
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.is_modified(cx) {
            self.dirty = false;
            return true;
        }
        let Some(collection) = self.collection.clone() else {
            return true;
        };
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return self.fail(t!("env.name_empty"), cx);
        }
        let variables = match variables_from_text(&self.variables.read(cx).value()) {
            Ok(variables) => variables,
            Err(e) => return self.fail(e, cx),
        };
        if let Some(row) = self.secret_rows.iter().find(|row| variables.contains_key(&row.name)) {
            return self.fail(t!("secrets.both_variable_and_secret", name = row.name), cx);
        }
        let pending: Vec<_> = self
            .secret_rows
            .iter()
            .filter_map(|row| Self::pending_value(row, cx).map(|value| (row.name.clone(), value)))
            .collect();
        let Some(store) = self.store.clone() else {
            if !pending.is_empty() || !self.removed_secrets.is_empty() {
                return self.fail(t!("secrets.store_unavailable_retry"), cx);
            }
            return self.write_files(collection, name, variables, cx);
        };

        let target = self.target.clone();
        let removed = std::mem::take(&mut self.removed_secrets);
        if !self.write_files(collection, name.clone(), variables, cx) {
            self.removed_secrets = removed;
            return false;
        }

        // The collection may have just been given an id; build refs from the updated file.
        let scope_label = if target == Target::Defaults { DEFAULTS_LABEL.to_string() } else { name };
        let file = self.collection.as_ref().map(|c| c.file.clone()).unwrap_or_else(|| CollectionFile::new(""));
        let scope = Self::scope_for(&target);
        let sets: Vec<_> = pending
            .into_iter()
            .map(|(var, value)| SecretWrite::new(&file, &scope, &scope_label, &var, value))
            .collect();
        let deletes: Vec<_> = removed
            .iter()
            .filter(|var| !self.secret_rows.iter().any(|row| &row.name == *var))
            .map(|var| self.secret_ref(&target, var))
            .collect();

        for row in &mut self.secret_rows {
            if sets.iter().any(|write| write.secret.name == row.name) {
                row.stored = Stored::Set;
                row.revealed = None;
                row.shown = false;
                row.input.update(cx, |s, cx| {
                    s.set_value("", window, cx);
                    s.set_masked(true, window, cx);
                });
            }
            row.is_new = false;
        }

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { store.apply(&sets, &deletes).await })
                .await;
            if let Err(e) = result {
                this.update(cx, |_, cx| {
                    cx.emit(EnvironmentEditorEvent::Error(t!("secrets.could_not_store", error = format!("{e:#}")).to_string()))
                })
                .ok();
            }
        })
        .detach();
        true
    }

    /// Writes the YAML for the current target (and the collection file, if it needed an id).
    fn write_files(&mut self, mut collection: Collection, name: String, variables: Variables, cx: &mut Context<Self>) -> bool {
        let secrets: Vec<String> = self.secret_rows.iter().map(|r| r.name.clone()).collect();
        let root = collection.root.clone();
        let assigned_id = !secrets.is_empty() && collection.file.ensure_id().1;
        let result = match &self.target {
            Target::Defaults => {
                collection.file.name = name;
                collection.file.variables = variables;
                collection.file.secrets = secrets;
                storage::save_collection_file(&root, &collection.file)
            }
            Target::Environment(path) => {
                let env = EnvironmentFile { name, variables, secrets };
                let id_saved = if assigned_id {
                    storage::save_collection_file(&root, &collection.file)
                } else {
                    Ok(())
                };
                id_saved.and_then(|()| storage::write_yaml(path, &env))
            }
        };
        match result {
            Ok(()) => {
                // Keep our copy current until the workspace's reload arrives.
                if let Some(current) = &mut self.collection {
                    current.file.id = collection.file.id.clone();
                }
                self.dirty = false;
                self.error = None;
                cx.emit(EnvironmentEditorEvent::Changed(root));
                cx.notify();
                true
            }
            Err(e) => {
                cx.emit(EnvironmentEditorEvent::Error(t!("request.could_not_save", error = format!("{e:#}")).to_string()));
                false
            }
        }
    }

    fn select(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if target != self.target && self.save(window, cx) {
            self.load_target(target, window, cx);
        }
    }

    fn create(&mut self, file: EnvironmentFile, copy_from: Option<Target>, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save(window, cx) {
            return;
        }
        let Some(root) = self.root().map(Path::to_path_buf) else {
            return;
        };
        match storage::create_environment(&root, &file) {
            Ok(path) => {
                let target = Target::Environment(path);
                if let (Some(from), Some(store)) = (copy_from, self.store.clone()) {
                    let copies: Vec<_> = file
                        .secrets
                        .iter()
                        .map(|var| {
                            (self.secret_ref(&from, var), self.secret_ref(&target, var), self.secret_label(&file.name, var))
                        })
                        .collect();
                    cx.spawn_in(window, async move |this, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                for (from, to, label) in &copies {
                                    if let Some(value) = store.get(from).await? {
                                        store.set(to, label, &value).await?;
                                    }
                                }
                                anyhow::Ok(())
                            })
                            .await;
                        this.update_in(cx, |this, window, cx| match result {
                            Ok(()) => this.check_stored(window, cx),
                            Err(e) => cx.emit(EnvironmentEditorEvent::Error(
                                t!("secrets.could_not_copy", error = format!("{e:#}")).to_string(),
                            )),
                        })
                        .ok();
                    })
                    .detach();
                }
                // Events are delivered after this update, so the fields fill in once the
                // workspace reloads the collection and calls `update_collection`.
                cx.emit(EnvironmentEditorEvent::Changed(root));
                self.load_target(target, window, cx);
                self.name.focus_handle(cx).focus(window, cx);
            }
            Err(e) => cx.emit(EnvironmentEditorEvent::Error(format!("{e:#}"))),
        }
    }

    fn duplicate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save(window, cx) {
            return;
        }
        if let Some(saved) = self.saved() {
            let file = EnvironmentFile {
                name: t!("env.copy_name", name = saved.name).to_string(),
                variables: saved.variables,
                secrets: saved.secrets,
            };
            self.create(file, Some(self.target.clone()), window, cx);
        }
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Environment(path) = self.target.clone() else {
            return;
        };
        let Some(root) = self.root().map(Path::to_path_buf) else {
            return;
        };
        let Some(saved) = self.saved() else {
            return;
        };
        let secret_refs: Vec<_> = saved.secrets.iter().map(|var| self.secret_ref(&self.target, var)).collect();
        let name = saved.name;
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (weak, path, root, secret_refs) = (weak.clone(), path.clone(), root.clone(), secret_refs.clone());
            let message = if secret_refs.is_empty() {
                t!("env.delete_message", path = path.display()).to_string()
            } else {
                t!("env.delete_message_with_secrets", path = path.display(), count = secret_refs.len()).to_string()
            };
            dialog
                .title(t!("env.delete_title", name = name).to_string())
                .w(px(420.))
                .content(move |content, _, _| content.child(message.clone()))
                .footer(crate::ui::dialog_footer(Some(t!("env.delete").to_string()), ButtonVariant::Danger))
                .on_ok(move |_, window, cx| {
                    let result = storage::delete_file(&path);
                    let secret_refs = secret_refs.clone();
                    weak.update(cx, |this, cx| match result {
                        Ok(()) => {
                            if let Some(store) = this.store.clone() {
                                cx.background_executor()
                                    .spawn(async move {
                                        for secret in &secret_refs {
                                            if let Err(e) = store.delete(secret).await {
                                                eprintln!("deleting secret {}: {e:#}", secret.name);
                                            }
                                        }
                                    })
                                    .detach();
                            }
                            cx.emit(EnvironmentEditorEvent::Deleted { root: root.clone(), path: path.clone() });
                            this.load_target(Target::Defaults, window, cx);
                        }
                        Err(e) => cx.emit(EnvironmentEditorEvent::Error(format!("{e:#}"))),
                    })
                    .ok();
                    true
                })
        });
    }

    // MARK: Secret rows

    fn validate_secret_name(&self, name: &str, cx: &App) -> Result<(), String> {
        if name.is_empty() {
            return Err(t!("secrets.name_empty").to_string());
        }
        if name.contains(['{', '}', ':']) || name.chars().any(char::is_whitespace) {
            return Err(t!("secrets.name_invalid", name = name).to_string());
        }
        if self.secret_rows.iter().any(|row| row.name == name) {
            return Err(t!("secrets.name_taken_secret", name = name).to_string());
        }
        if variables_from_text(&self.variables.read(cx).value()).is_ok_and(|vars| vars.contains_key(name)) {
            return Err(t!("secrets.name_taken_variable", name = name).to_string());
        }
        Ok(())
    }

    fn add_secret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.new_secret.read(cx).value().trim().to_string();
        if let Err(e) = self.validate_secret_name(&name, cx) {
            self.fail(e, cx);
            return;
        }
        let row = self.new_row(name, None, true, window, cx);
        row.input.focus_handle(cx).focus(window, cx);
        self.secret_rows.push(row);
        self.new_secret.update(cx, |s, cx| s.set_value("", window, cx));
        self.error = None;
        self.update_dirty(cx);
    }

    fn remove_secret(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.secret_rows.len() {
            return;
        }
        let row = self.secret_rows.remove(index);
        if !row.is_new {
            self.removed_secrets.push(row.name);
        }
        self.update_dirty(cx);
    }

    fn move_variable_to_secret(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let mut variables = match variables_from_text(&self.variables.read(cx).value()) {
            Ok(variables) => variables,
            Err(e) => {
                self.fail(e, cx);
                return;
            }
        };
        let Some(value) = variables.shift_remove(&name) else {
            return;
        };
        self.variables
            .update(cx, |s, cx| s.set_value(variables_to_text(&variables), window, cx));
        let row = self.new_row(name, Some(value), true, window, cx);
        self.secret_rows.push(row);
        self.update_dirty(cx);
    }

    fn reveal_secret(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.secret_rows.get_mut(index) else {
            return;
        };
        if row.revealed.is_some() || !row.input.read(cx).value().is_empty() {
            row.shown = !row.shown;
            let masked = !row.shown;
            row.input.update(cx, |s, cx| s.set_masked(masked, window, cx));
            return;
        }
        let name = row.name.clone();
        let Some(store) = self.store.clone() else {
            return;
        };
        let secret = self.secret_ref(&self.target, &name);
        let generation = self.generation;
        cx.spawn_in(window, async move |this, cx| {
            let value = cx.background_executor().spawn(async move { store.get(&secret).await }).await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                match value {
                    Ok(Some(value)) => {
                        if let Some(row) = this.secret_rows.iter_mut().find(|r| r.name == name) {
                            row.revealed = Some(value.clone());
                            row.shown = true;
                            row.input.update(cx, |s, cx| {
                                s.set_value(value, window, cx);
                                s.set_masked(false, window, cx);
                            });
                        }
                    }
                    Ok(None) => {}
                    Err(e) => cx.emit(EnvironmentEditorEvent::Error(
                        t!("secrets.could_not_read", error = format!("{e:#}")).to_string(),
                    )),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // MARK: Rendering

    fn render_list(&self, collection: &Collection, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let row = |id: usize, label: String, target: Target, active: bool, cx: &mut Context<Self>| {
            let selected = self.target == target;
            h_flex()
                .id(("environment", id))
                .px_2()
                .py_1()
                .gap_2()
                .rounded_md()
                .cursor_pointer()
                .text_sm()
                .when(selected, |s| s.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                .hover(|s| s.bg(theme.sidebar_accent))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .when(active, |s| s.child(div().text_xs().text_color(theme.success).child(t!("env.active").to_string())))
                .on_click(cx.listener(move |this, _, window, cx| this.select(target.clone(), window, cx)))
        };

        let mut list = v_flex()
            .w(px(220.))
            .flex_none()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(collection.file.name.clone()),
            )
            .child(row(0, t!("env.collection_defaults").to_string(), Target::Defaults, false, cx))
            .child(div().px_2().pt_2().pb_1().text_xs().text_color(theme.muted_foreground).child(t!("env.environments").to_string()));
        for (ix, env) in collection.environments.iter().enumerate() {
            let active = self.active.as_ref() == Some(&env.path);
            list = list.child(row(ix + 1, env.file.name.clone(), Target::Environment(env.path.clone()), active, cx));
        }
        if collection.environments.is_empty() {
            list = list.child(div().px_2().text_sm().text_color(theme.muted_foreground).child(t!("env.none_yet").to_string()));
        }
        list.child(
            h_flex().child(
                Button::new("new-environment")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .label(t!("env.new").to_string())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.create(EnvironmentFile::new(t!("env.new")), None, window, cx);
                    })),
            ),
        )
    }

    fn render_secrets(&self, variables: Option<&Variables>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let description = match &self.store {
            Some(store) => store.kind().describe(),
            None => t!("secrets.connecting").to_string(),
        };

        let sensitive: Vec<String> = variables
            .into_iter()
            .flatten()
            .filter(|(name, value)| looks_sensitive_name(name) && !value.is_empty() && !value.contains("{{"))
            .map(|(name, _)| name.clone())
            .collect();

        let mut rows = v_flex().id("secret-rows").gap_1().max_h(px(240.)).overflow_y_scroll();
        for (ix, row) in self.secret_rows.iter().enumerate() {
            let has_input = !row.input.read(cx).value().is_empty();
            let (status, color) = match (row.stored, has_input) {
                (_, true) if row.revealed.is_none() => (t!("secrets.status_will_save"), theme.warning),
                (Stored::Checking, _) => (t!("secrets.status_checking"), theme.muted_foreground),
                (Stored::Set, _) => (t!("secrets.status_set"), theme.success),
                (Stored::NotSet, _) => (t!("secrets.status_not_set"), theme.danger),
                (Stored::Unknown, _) => (t!("secrets.status_unavailable"), theme.danger),
            };
            let can_reveal = row.stored == Stored::Set || has_input;
            rows = rows.child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(180.)).flex_none().truncate().text_sm().font_family("monospace").child(row.name.clone()))
                    .child(div().flex_1().min_w_0().child(secret_input(&row.input).small()))
                    .child(div().w(px(150.)).flex_none().text_xs().text_color(color).child(status.to_string()))
                    .child(
                        Button::new(("reveal-secret", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Eye)
                            .tooltip(t!("secrets.show_or_hide").to_string())
                            .disabled(!can_reveal)
                            .on_click(cx.listener(move |this, _, window, cx| this.reveal_secret(ix, window, cx))),
                    )
                    .child(
                        Button::new(("remove-secret", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t!("secrets.remove").to_string())
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_secret(ix, cx))),
                    ),
            );
        }

        v_flex()
            .gap_2()
            .pt_2()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("secrets.title").to_string()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(description)),
            )
            .when(!sensitive.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(t!("secrets.looks_like_credentials").to_string())
                        .children(sensitive.into_iter().enumerate().map(|(ix, name)| {
                            Button::new(("move-to-secret", ix))
                                .small()
                                .warning()
                                .label(t!("secrets.move_variable", name = name).to_string())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.move_variable_to_secret(name.clone(), window, cx)
                                }))
                        })),
                )
            })
            .when(self.secret_rows.is_empty(), |this| {
                this.child(div().text_xs().text_color(theme.muted_foreground).child(t!("secrets.none_yet").to_string()))
            })
            .child(rows)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(180.)).flex_none().child(text_input(&self.new_secret).small()))
                    .child(
                        Button::new("add-secret")
                            .small()
                            .icon(IconName::Plus)
                            .label(t!("secrets.add").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.add_secret(window, cx))),
                    ),
            )
    }
}

impl Focusable for EnvironmentEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EnvironmentEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(collection) = self.collection.as_ref() else {
            return div().into_any_element();
        };
        let is_defaults = self.target == Target::Defaults;
        // Parsed once per frame and shared by the hint and the secrets section.
        let parsed = variables_from_text(&self.variables.read(cx).value()).ok();

        let hint = if is_defaults {
            t!("env.hint_defaults").to_string()
        } else {
            let own = parsed.clone().unwrap_or_default();
            let inherited: Vec<_> = collection
                .file
                .variables
                .keys()
                .chain(collection.file.secrets.iter())
                .filter(|k| !own.contains_key(*k) && !self.secret_rows.iter().any(|r| &r.name == *k))
                .cloned()
                .collect();
            if inherited.is_empty() {
                t!("env.hint_overrides").to_string()
            } else {
                t!("env.hint_inherited", names = inherited.join(", ")).to_string()
            }
        };

        h_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &SaveEnvironment, window, cx| {
                this.save(window, cx);
            }))
            .size_full()
            .items_start()
            .p_3()
            .gap_4()
            .child(self.render_list(collection, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(text_input(&self.name))
                                    .when(is_defaults, |s| {
                                        s.child(
                                            div()
                                                .pt_1()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(t!("env.collection_name").to_string()),
                                        )
                                    }),
                            )
                            .when(!is_defaults, |s| {
                                s.child(
                                    Button::new("duplicate-environment")
                                        .ghost()
                                        .label(t!("env.duplicate").to_string())
                                        .on_click(cx.listener(|this, _, window, cx| this.duplicate(window, cx))),
                                )
                                .child(
                                    Button::new("delete-environment")
                                        .ghost()
                                        .label(t!("env.delete").to_string())
                                        .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx))),
                                )
                            })
                            .child(
                                Button::new("save-environment")
                                    .primary()
                                    .label(t!("env.save").to_string())
                                    .tooltip(t!("env.save_shortcut").to_string())
                                    .disabled(!self.dirty)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("close-environments")
                                    .ghost()
                                    .label(t!("env.done").to_string())
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(EnvironmentEditorEvent::Close))),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("env.variables_hint").to_string()),
                    )
                    .child(code_editor(&self.variables).flex_1().min_h(px(120.)))
                    .child(match &self.error {
                        Some(error) => div().text_sm().text_color(theme.danger).child(error.clone()),
                        None => div().text_xs().text_color(theme.muted_foreground).child(hint),
                    })
                    .child(self.render_secrets(parsed.as_ref(), cx)),
            )
            .into_any_element()
    }
}
