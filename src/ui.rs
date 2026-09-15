//! Component constructors with the app's translations applied. GPUI Kit's own input menu
//! and dialog buttons carry strings in only Chinese and Italian, so every input, editor and
//! confirm dialog is built here rather than with the bare component constructors (a test
//! enforces this).

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{Cancel, Confirm, DialogFooter};
use gpui_kit::component::input::{
    Copy, Cut, Editor, EditorState, Input, InputState, Paste, SelectAll, Textarea, TextareaState,
};
use gpui_kit::component::native_menu::NativeMenu;
use gpui_kit::{App, Entity, Focusable, IntoElement, ParentElement as _, Window};
use rust_i18n::t;

/// A single-line text input.
pub fn text_input(state: &Entity<InputState>) -> Input {
    Input::new(state).context_menu(edit_menu(EditMenu::Editable))
}

/// A masked input for a secret value: no Cut or Copy in its menu.
pub fn secret_input(state: &Entity<InputState>) -> Input {
    Input::new(state).context_menu(edit_menu(EditMenu::Secret))
}

/// An editable multi-line code editor.
pub fn code_editor(state: &Entity<EditorState>) -> Editor {
    Editor::new(state).context_menu(edit_menu(EditMenu::Editable))
}

/// A one-line editor for text that gets highlighting, such as a URL with `{{variables}}`.
pub fn single_line_editor(state: &Entity<EditorState>) -> Editor {
    Editor::new(state).context_menu(edit_menu(EditMenu::Editable))
}

/// A read-only code editor, e.g. for responses.
pub fn readonly_editor(state: &Entity<EditorState>) -> Editor {
    Editor::new(state)
        .readonly(true)
        .context_menu(edit_menu(EditMenu::ReadOnly))
}

pub fn textarea(state: &Entity<TextareaState>) -> Textarea {
    Textarea::new(state).context_menu(edit_menu(EditMenu::Editable))
}

/// Cancel and confirm buttons for a dialog, in the interface language. They dispatch the
/// same actions as Escape and Enter, so the dialog's `on_ok` / `on_cancel` handle both.
/// (A plain GPUI Kit `Dialog` renders no buttons unless given a footer.)
pub fn dialog_footer(ok_label: Option<String>, variant: ButtonVariant) -> impl IntoElement {
    DialogFooter::new()
        .child(
            Button::new("dialog-cancel")
                .label(t!("common.cancel").to_string())
                .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
        )
        .child(
            Button::new("dialog-ok")
                .label(ok_label.unwrap_or_else(|| t!("common.ok").to_string()))
                .with_variant(variant)
                .on_click(|_, window, cx| window.dispatch_action(Box::new(Confirm { secondary: false }), cx)),
        )
}

#[derive(Clone, Copy, PartialEq)]
enum EditMenu {
    Editable,
    ReadOnly,
    /// Masked values: no Cut or Copy, so a secret can't reach the clipboard from here.
    Secret,
}

/// A right-click menu for an input or editor, in the interface language.
fn edit_menu(kind: EditMenu) -> impl Fn(NativeMenu, &mut Window, &mut App) -> NativeMenu + 'static {
    move |mut menu, _, cx| {
        if kind == EditMenu::Editable {
            menu = menu.menu(t!("edit.cut"), Box::new(Cut));
        }
        if kind != EditMenu::Secret {
            menu = menu.menu(t!("edit.copy"), Box::new(Copy));
        }
        if kind != EditMenu::ReadOnly {
            menu = menu.menu_with_disabled(t!("edit.paste"), cx.read_from_clipboard().is_none(), Box::new(Paste));
        }
        menu.separator().menu(t!("edit.select_all"), Box::new(SelectAll))
    }
}

/// Focuses `input` once a dialog that was just opened has taken focus itself.
pub fn focus_in_dialog<T: Focusable>(input: &Entity<T>, window: &mut Window, cx: &mut App) {
    let input = input.clone();
    window.defer(cx, move |window, cx| input.focus_handle(cx).focus(window, cx));
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    /// Bare `Input::new`, `Editor::new` or `Textarea::new` outside this module would show
    /// GPUI Kit's untranslated context menu.
    #[test]
    fn inputs_are_built_through_ui() {
        let mut offenders = Vec::new();
        let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.ends_with("ui.rs") {
                    continue;
                }
                let text = fs::read_to_string(&path).unwrap();
                for pattern in ["Input::new(", "Editor::new(", "Textarea::new("] {
                    for (i, _) in text.match_indices(pattern) {
                        let before = text[..i].chars().next_back();
                        if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                            offenders.push(format!("{}: {pattern}", path.display()));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "use crate::ui constructors:\n{}",
            offenders.join("\n")
        );
    }
}
