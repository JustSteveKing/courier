mod credentials;
mod environment_editor;
mod http;
mod i18n;
mod import;
mod model;
mod omarchy_theme;
mod paths;
mod request_editor;
mod secret_store;
mod settings;
mod storage;
mod workspace;

use gpui_kit::component::Root;
use gpui_kit::*;

use crate::paths::{APP_ID, AppPaths};
use crate::settings::AppSettings;
use crate::workspace::Workspace;

rust_i18n::i18n!("locales", fallback = "en");

fn main() {
    let paths = match AppPaths::from_env() {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("could not determine XDG directories: {e:#}");
            std::process::exit(1);
        }
    };

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.set_global(AppSettings::load(&paths));
            i18n::apply(i18n::resolve(AppSettings::get(cx).language.as_deref()));
            omarchy_theme::init(cx);
            request_editor::init(cx);
            workspace::palette::init(cx);
            environment_editor::init(cx);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(800.)),
                    cx,
                ))),
                app_id: Some(APP_ID.into()),
                // Let Hyprland draw the frame, so the theme's border gradient applies.
                window_decorations: Some(WindowDecorations::Server),
                ..Default::default()
            };
            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| Workspace::new(paths, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
