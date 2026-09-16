//! The command palette (Ctrl+Shift+P): jump to any request, run workspace actions, switch
//! environments, and change settings.

use std::rc::Rc;

use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{ActiveTheme as _, IconName, WindowExt as _};
use gpui_kit::*;
use rust_i18n::t;

use super::{RootAction, Workspace};
use crate::i18n::{self, LANGUAGES};
use crate::import::ImportFormat;
use crate::model::RequestKind;
use crate::omarchy_theme;
use crate::settings::AppSettings;

gpui_kit::actions!(
    workspace,
    [
        OpenCommandPalette,
        NewScratchRequest,
        CloseTab,
        NextTab,
        PreviousTab,
        PasteCurl,
        ShowShortcuts,
        FocusSidebar,
        FocusUrl,
        FocusBody,
        SelectNext,
        SelectPrevious,
        OpenSelected,
        CollapseSelected,
        ExpandSelected
    ]
);

/// Registers the actions; the keys themselves come from `crate::keymap`.
pub fn init(_cx: &mut App) {}

type Run = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

pub(super) struct Entry {
    pub(super) label: SharedString,
    keywords: Vec<SharedString>,
    icon: Option<IconName>,
    checked: bool,
    run: Run,
}

impl Entry {
    fn new(
        label: impl Into<SharedString>,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            keywords: Vec::new(),
            icon: None,
            checked: false,
            run: Rc::new(run),
        }
    }

    /// Also match the English wording, so muscle memory works in any language.
    fn english(mut self, key: &str) -> Self {
        self.keywords.push(t!(key, locale = "en").to_string().into());
        self
    }

    fn keywords(mut self, words: impl IntoIterator<Item = String>) -> Self {
        self.keywords.extend(words.into_iter().map(SharedString::from));
        self
    }

    fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
}

pub(super) struct Group {
    pub(super) label: SharedString,
    pub(super) entries: Vec<Entry>,
}

impl Workspace {
    pub(super) fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let groups = Rc::new(self.palette_groups(cx));
        let state = cx.new(|cx| CommandState::new(window, cx));
        let weak = cx.entity().downgrade();

        window.open_dialog(cx, {
            let state = state.clone();
            move |dialog, _, _| {
                let (state, groups, weak) = (state.clone(), groups.clone(), weak.clone());
                dialog
                    .w(px(640.))
                    .close_button(false)
                    .margin_top(px(80.))
                    .content(move |content, _, _| {
                        let mut command = Command::new(&state)
                            .placeholder(t!("palette.placeholder").to_string())
                            .max_h(px(440.))
                            .bordered(false)
                            .empty(|_, _, cx| {
                                div()
                                    .py_6()
                                    .text_center()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(t!("palette.empty").to_string())
                            });
                        for group in groups.iter() {
                            let items = group.entries.iter().map(|entry| {
                                let mut item = CommandItem::new()
                                    .label(entry.label.clone())
                                    .keywords(entry.keywords.clone())
                                    .checked(entry.checked);
                                if let Some(icon) = entry.icon.clone() {
                                    item = item.icon(icon);
                                }
                                item
                            });
                            command = command.group(CommandGroup::new().label(group.label.clone()).items(items));
                        }
                        let (groups, weak) = (groups.clone(), weak.clone());
                        content.child(
                            command
                                .on_confirm(move |index, window, cx| {
                                    let Some(entry) = groups.get(index.section).and_then(|g| g.entries.get(index.row))
                                    else {
                                        return;
                                    };
                                    let run = entry.run.clone();
                                    window.close_dialog(cx);
                                    weak.update(cx, |this, cx| run(this, window, cx)).ok();
                                })
                                .on_cancel(|window, cx| window.close_dialog(cx)),
                        )
                    })
            }
        });
        state.update(cx, |state, cx| state.focus(window, cx));
    }

    pub(super) fn palette_groups(&self, cx: &App) -> Vec<Group> {
        let mut groups = vec![
            self.request_entries(),
            self.action_group(cx),
            self.environment_entries(cx),
            settings_entries(cx),
        ];
        groups.retain(|g| !g.entries.is_empty());
        groups
    }

    fn request_entries(&self) -> Group {
        let mut entries = Vec::new();
        for collection in &self.collections {
            for entry in collection.requests() {
                let collection_label = self.collection_label(collection);
                let label = std::iter::once(collection_label.as_str())
                    .chain(entry.folders.iter().copied())
                    .chain([entry.request.name.as_str()])
                    .collect::<Vec<_>>()
                    .join(" › ");
                let path = entry.path.to_path_buf();
                entries.push(
                    Entry::new(label, move |this, window, cx| {
                        this.select_request(path.clone(), window, cx)
                    })
                    .keywords([entry.request.method.clone(), entry.request.url.clone()]),
                );
            }
        }
        Group {
            label: t!("palette.group.requests").to_string().into(),
            entries,
        }
    }

    fn action_group(&self, cx: &App) -> Group {
        let mut entries: Vec<Entry> = RequestKind::ALL
            .into_iter()
            .map(|kind| {
                let label = t!("ws.in_scratchpad", action = super::new_request_label(kind));
                Entry::new(label, move |this, window, cx| {
                    this.new_scratch_request_of(kind, window, cx)
                })
                .english("ws.new_scratch_request")
                .keywords(["quick".into(), "scratchpad".into(), "request".into()])
            })
            .collect();
        entries.extend([
            Entry::new(t!("ws.new_project_ellipsis"), |this, window, cx| {
                this.new_project(window, cx)
            })
            .english("ws.new_project_ellipsis")
            .keywords(["create".into(), "collection".into(), "folder".into()])
            .icon(IconName::Plus),
            Entry::new(t!("ws.open_project_ellipsis"), |this, window, cx| {
                this.open_project(window, cx)
            })
            .english("ws.open_project_ellipsis")
            .keywords(["folder".into(), "collection".into()])
            .icon(IconName::FolderOpen),
            Entry::new(t!("ws.new_project_from_postman"), |this, window, cx| {
                this.new_project_from(ImportFormat::Postman, window, cx)
            })
            .english("ws.new_project_from_postman"),
            Entry::new(t!("ws.new_project_from_openapi"), |this, window, cx| {
                this.new_project_from(ImportFormat::OpenApi, window, cx)
            })
            .english("ws.new_project_from_openapi")
            .keywords(["swagger".into(), "spec".into()]),
            Entry::new(t!("ws.new_project_from_asyncapi"), |this, window, cx| {
                this.new_project_from(ImportFormat::AsyncApi, window, cx)
            })
            .english("ws.new_project_from_asyncapi")
            .keywords(["websocket".into(), "spec".into()]),
        ]);
        if let Some(root) = self.active_collection(cx).map(|c| c.root.clone()) {
            let with_root = |key: &'static str, run: RootAction| {
                let root = root.clone();
                Entry::new(t!(key), move |this, window, cx| run(this, root.clone(), window, cx)).english(key)
            };
            if let Some(path) = self.editor().read(cx).path().cloned() {
                for (key, include_secrets) in [("ws.copy_as_curl", false), ("ws.copy_as_curl_with_secrets", true)] {
                    let path = path.clone();
                    entries.push(
                        Entry::new(t!(key), move |this, window, cx| {
                            this.copy_as_curl(path.clone(), include_secrets, window, cx)
                        })
                        .english(key)
                        .keywords(["export".into(), "clipboard".into()]),
                    );
                }
            }
            for kind in RequestKind::ALL {
                let root = root.clone();
                entries.push(
                    Entry::new(super::new_request_label(kind), move |this, window, cx| {
                        this.new_request_of(root.clone(), kind, window, cx)
                    })
                    .icon(IconName::Plus),
                );
            }
            entries.extend([
                with_root("ws.manage_environments_ellipsis", Workspace::manage_environments).icon(IconName::Settings),
                with_root("ws.import_curl", Workspace::import_curl_dialog).icon(IconName::SquareTerminal),
                with_root("ws.import_file_here", Workspace::import_file_into),
                with_root("ws.import_postman_environment", Workspace::import_postman_environment),
                with_root("ws.reload_from_disk", |this, root, window, cx| {
                    this.reload_collection(&root, window, cx)
                }),
                with_root("ws.show_in_file_manager", |_, root, _, cx| cx.reveal_path(&root)),
                with_root("ws.close_collection", |this, root, window, cx| {
                    this.close_collection(&root, window, cx)
                }),
            ]);
        }
        Group {
            label: t!("palette.group.actions").to_string().into(),
            entries,
        }
    }

    fn environment_entries(&self, cx: &App) -> Group {
        let mut entries = Vec::new();
        if let Some(collection) = self.active_collection(cx) {
            let active = self.state.active_environments.get(&collection.root);
            entries.push(
                Entry::new(
                    t!("palette.use_environment", name = t!("ws.no_environment")),
                    |this, window, cx| this.choose_environment(Some(0), window, cx),
                )
                .checked(active.is_none()),
            );
            for (ix, env) in collection.environments.iter().enumerate() {
                entries.push(
                    Entry::new(
                        t!("palette.use_environment", name = env.file.name),
                        move |this, window, cx| this.choose_environment(Some(ix + 1), window, cx),
                    )
                    .english("palette.use_environment")
                    .checked(active == Some(&env.path)),
                );
            }
        }
        Group {
            label: t!("palette.group.environments").to_string().into(),
            entries,
        }
    }
}

fn settings_entries(cx: &App) -> Group {
    let settings = AppSettings::get(cx);
    let system = i18n::system_language();
    let system_name = LANGUAGES
        .iter()
        .find(|(c, _)| *c == system)
        .map(|(_, n)| *n)
        .unwrap_or("English");
    let mut entries = vec![
        Entry::new(
            t!(
                "settings.language",
                name = t!("settings.language_system", name = system_name)
            ),
            |this, window, cx| this.set_language(None, window, cx),
        )
        .english("settings.language")
        .checked(settings.language.is_none()),
    ];
    for (code, name) in LANGUAGES {
        entries.push(
            Entry::new(t!("settings.language", name = name), move |this, window, cx| {
                this.set_language(Some(code.to_string()), window, cx)
            })
            .english("settings.language")
            .keywords([code.to_string()])
            .checked(settings.language.as_deref() == Some(code)),
        );
    }
    let remember = settings.remember_responses;
    entries.push(
        Entry::new(t!("settings.remember_responses"), move |this, window, cx| {
            this.set_remember_responses(!remember, window, cx)
        })
        .english("settings.remember_responses")
        .keywords(["cache".into(), "history".into()])
        .checked(remember),
    );
    entries.push(
        Entry::new(t!("settings.clear_responses"), |this, window, cx| {
            this.clear_saved_responses(window, cx)
        })
        .english("settings.clear_responses")
        .keywords(["cache".into()]),
    );
    let follow = settings.follow_omarchy_theme;
    entries.push(
        Entry::new(t!("settings.follow_omarchy"), move |_, _, cx| {
            AppSettings::update(cx, |s| s.follow_omarchy_theme = !follow);
            omarchy_theme::set_following(!follow, cx);
        })
        .english("settings.follow_omarchy")
        .keywords(["theme".into()])
        .checked(follow),
    );
    for kind in RequestKind::ALL {
        let current = settings.request_colors.get(kind);
        for color in super::color_choices(kind) {
            entries.push(
                Entry::new(
                    t!(
                        "colors.palette",
                        kind = super::kind_name(kind),
                        color = super::color_name(color)
                    ),
                    move |this, _, cx| this.set_request_color(kind, color, cx),
                )
                .english("colors.palette")
                .keywords(["colour".into(), "color".into(), "label".into()])
                .checked(color == current),
            );
        }
    }
    entries.push(
        Entry::new(t!("settings.open_file"), |_, _, cx| {
            let path = AppSettings::path(cx);
            if !path.exists() {
                AppSettings::save_now(cx);
            }
            cx.open_with_system(&path);
        })
        .english("settings.open_file")
        .icon(IconName::Settings),
    );
    Group {
        label: t!("palette.group.settings").to_string().into(),
        entries,
    }
}
