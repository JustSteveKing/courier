mod auth_form;
mod environment_editor;
mod i18n;
mod keymap;
mod omarchy_theme;
mod request_editor;
mod runner_view;
mod settings;
mod settings_form;
mod ui;
mod workspace;

// The engine lives in `courier-core`; these keep `crate::model` and friends working here.
pub(crate) use courier_core::{
    body_view, chain, checks, cookies, credentials, digest, dotenv, export, git, graphql, grpc, http, import, jwt,
    model, oauth, paths, project, response_cache, runner, secret_store, sigv4, storage, template_assist, transport,
};

use gpui_kit::component::Root;
use gpui_kit::*;

use crate::paths::{APP_ID, AppPaths};
use crate::settings::AppSettings;
use crate::workspace::{Launch, Workspace};

rust_i18n::i18n!("locales", fallback = "en");

fn main() {
    // `courier run …`, `courier send …` and friends work without opening a window.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| courier_cli::SUBCOMMANDS.iter().any(|command| arg == *command))
    {
        std::process::exit(courier_cli::main_from(std::env::args_os()));
    }

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
            // Every action is registered by now, so the keymap can bind them.
            keymap::apply(&paths, cx);

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
            "Usage: courier [PROJECT]\n       courier <send|run|list|envs|completions> …\n\nOpens PROJECT's .courier collection (offering to create one), or the\nproject containing the current directory.\n\nThe subcommands work in a terminal or CI without a window; see `courier run --help`.\n(To open a folder with one of those names, write it as ./run.)"
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
