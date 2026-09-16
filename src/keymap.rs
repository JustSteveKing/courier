//! Every keyboard shortcut in one place, and a `keymap.yaml` to change them.
//!
//! Bindings live here rather than beside their actions so that one file can list them, one
//! file can override them, and the shortcuts sheet can show what is actually in effect.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui_kit::{App, KeyBinding};
use rust_i18n::t;

use crate::paths::AppPaths;

/// One shortcut: the action's registered name, the keys, and where it applies.
pub struct Shortcut {
    /// As registered by `actions!`, e.g. `request_editor::SendRequest`.
    pub action: &'static str,
    pub keys: &'static str,
    /// The key context it applies in; `None` means anywhere in the window.
    pub context: Option<&'static str>,
    /// Translation key for what it does.
    pub description: &'static str,
}

pub const DEFAULTS: &[Shortcut] = &[
    Shortcut {
        action: "request_editor::SendRequest",
        keys: "ctrl-enter",
        context: Some("RequestEditor"),
        description: "keys.send",
    },
    Shortcut {
        action: "request_editor::SendRequest",
        keys: "secondary-enter",
        // Text inputs bind their own "secondary enter"; this deeper context wins inside the
        // editor's fields, so the shortcut works from any of them.
        context: Some("RequestEditor > Input"),
        description: "keys.send",
    },
    Shortcut {
        action: "request_editor::SaveRequest",
        keys: "ctrl-s",
        context: Some("RequestEditor"),
        description: "keys.save",
    },
    Shortcut {
        action: "environment_editor::SaveEnvironment",
        keys: "ctrl-s",
        context: Some("EnvironmentEditor"),
        description: "keys.save_environment",
    },
    Shortcut {
        action: "workspace::OpenCommandPalette",
        keys: "ctrl-shift-p",
        context: None,
        description: "keys.palette",
    },
    Shortcut {
        action: "workspace::NewScratchRequest",
        keys: "ctrl-n",
        context: None,
        description: "keys.new_scratch",
    },
    Shortcut {
        action: "workspace::CloseTab",
        keys: "ctrl-w",
        context: None,
        description: "keys.close_tab",
    },
    Shortcut {
        action: "workspace::NextTab",
        keys: "ctrl-tab",
        context: None,
        description: "keys.next_tab",
    },
    Shortcut {
        action: "workspace::PreviousTab",
        keys: "ctrl-shift-tab",
        context: None,
        description: "keys.previous_tab",
    },
    Shortcut {
        action: "workspace::PasteCurl",
        keys: "ctrl-v",
        context: Some("Workspace"),
        description: "keys.paste_curl",
    },
    Shortcut {
        action: "workspace::ShowShortcuts",
        keys: "f1",
        context: None,
        description: "keys.shortcuts",
    },
];

/// Where the overrides live.
pub fn file(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("keymap.yaml")
}

/// `action: keys` pairs read from `keymap.yaml`. An empty value unbinds the default.
fn overrides(paths: &AppPaths) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(file(paths)) else {
        return BTreeMap::new();
    };
    match serde_norway::from_str::<BTreeMap<String, String>>(&text) {
        Ok(map) => map,
        Err(e) => {
            eprintln!("ignoring keymap.yaml: {e}");
            BTreeMap::new()
        }
    }
}

/// What each action is bound to now: the default, or the override.
pub fn effective(paths: &AppPaths) -> Vec<(&'static Shortcut, String)> {
    let overrides = overrides(paths);
    DEFAULTS
        .iter()
        .map(|shortcut| {
            let keys = overrides
                .get(shortcut.action)
                .cloned()
                .unwrap_or_else(|| shortcut.keys.to_string());
            (shortcut, keys)
        })
        .collect()
}

/// Binds every shortcut. Actions must already be registered, so this runs after each
/// module's `init`.
pub fn apply(paths: &AppPaths, cx: &mut App) {
    let mut bindings = Vec::new();
    for (shortcut, keys) in effective(paths) {
        if keys.trim().is_empty() {
            continue; // Unbound on purpose.
        }
        let action = match cx.build_action(shortcut.action, None) {
            Ok(action) => action,
            Err(e) => {
                eprintln!("no action named {}: {e}", shortcut.action);
                continue;
            }
        };
        let context = match shortcut.context.map(gpui_kit::KeyBindingContextPredicate::parse) {
            Some(Ok(predicate)) => Some(std::rc::Rc::new(predicate)),
            Some(Err(e)) => {
                eprintln!("bad key context for {}: {e}", shortcut.action);
                continue;
            }
            None => None,
        };
        match KeyBinding::load(&keys, action, context, false, None, cx.keyboard_mapper().as_ref()) {
            Ok(binding) => bindings.push(binding),
            Err(e) => eprintln!("\"{keys}\" isn't a usable shortcut for {}: {e}", shortcut.action),
        }
    }
    cx.bind_keys(bindings);
}

/// Writes a keymap.yaml listing every shortcut, commented out, for editing. Existing files
/// are left alone.
pub fn write_example(paths: &AppPaths) -> std::io::Result<PathBuf> {
    let path = file(paths);
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(&paths.config_dir)?;
    let mut text = String::from(
        "# Courier shortcuts. Remove the # to change one, or set it to \"\" to unbind it.\n\
         # Keys look like ctrl-shift-p, alt-enter, f1.\n\n",
    );
    for shortcut in DEFAULTS {
        text.push_str(&format!(
            "# {}\n# {}: {}\n",
            t!(shortcut.description),
            shortcut.action,
            shortcut.keys
        ));
    }
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use core::prelude::v1::test;

    use super::*;

    #[test]
    fn overrides_replace_defaults_and_empty_unbinds() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::under(dir.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            file(&paths),
            "request_editor::SendRequest: alt-enter\nworkspace::NewScratchRequest: \"\"\n",
        )
        .unwrap();

        let effective = effective(&paths);
        let keys_for = |action: &str| {
            effective
                .iter()
                .filter(|(shortcut, _)| shortcut.action == action)
                .map(|(_, keys)| keys.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys_for("request_editor::SendRequest"),
            ["alt-enter", "alt-enter"],
            "both contexts follow the override"
        );
        assert_eq!(keys_for("workspace::NewScratchRequest"), [""], "empty means unbound");
        assert_eq!(
            keys_for("request_editor::SaveRequest"),
            ["ctrl-s"],
            "the rest are untouched"
        );
    }

    #[test]
    fn writes_an_example_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::under(dir.path());
        let path = write_example(&paths).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# request_editor::SendRequest: ctrl-enter"), "{text}");
        assert!(
            text.lines().all(|line| line.trim().is_empty() || line.starts_with('#')),
            "everything is commented out, so the file changes nothing until edited"
        );

        std::fs::write(&path, "mine\n").unwrap();
        write_example(&paths).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "mine\n",
            "an existing file is kept"
        );
    }

    #[test]
    fn every_shortcut_has_a_description() {
        for shortcut in DEFAULTS {
            assert!(
                shortcut.description.starts_with("keys."),
                "{} needs a keys.* description",
                shortcut.action
            );
            assert!(!shortcut.keys.is_empty(), "{} has no default keys", shortcut.action);
        }
    }
}
