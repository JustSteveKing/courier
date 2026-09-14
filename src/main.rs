mod credentials;
mod environment_editor;
mod http;
mod i18n;
mod import;
mod model;
mod omarchy_theme;
mod paths;
mod project;
mod request_editor;
mod response_cache;
mod secret_store;
mod settings;
mod storage;
mod workspace;

use gpui_kit::component::Root;
use gpui_kit::*;

use crate::paths::{APP_ID, AppPaths};
use crate::settings::AppSettings;
use crate::workspace::{Launch, Workspace};

rust_i18n::i18n!("locales", fallback = "en");

fn main() {
    let paths = match AppPaths::from_env() {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("could not determine XDG directories: {e:#}");
            std::process::exit(1);
        }
    };

    let launch = match launch_from_args() {
        Ok(launch) => launch,
        Err(message) => {
            println!("{message}");
            return;
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
                let view = cx.new(|cx| Workspace::new(paths, launch, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}

/// `courier [PROJECT]`: opens PROJECT, or the project containing the current directory.
fn launch_from_args() -> Result<Option<Launch>, String> {
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        Some(arg) if arg == "-h" || arg == "--help" => Err(
            "Usage: courier [PROJECT]\n\nOpens PROJECT's .courier collection (offering to create one), or the\nproject containing the current directory."
                .into(),
        ),
        Some(arg) => {
            let dir = std::path::PathBuf::from(arg);
            let dir = dir.canonicalize().map_err(|e| format!("courier: {}: {e}", dir.display()))?;
            Ok(Some(Launch { dir, explicit: true }))
        }
        None => Ok(std::env::current_dir().ok().map(|dir| Launch { dir, explicit: false })),
    }
}
