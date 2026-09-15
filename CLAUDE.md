# Courier

Native Linux API client (Postman/Yaak-style) in Rust on gpui-kit. See README.md for the product; this file is how to work on it.

## Commands

```sh
cargo test                  # all tests, including headless UI tests
cargo clippy --all-targets  # must be clean
cargo fmt                   # rustfmt.toml: max_width 120
cargo run -- <project-dir>
COURIER_THEME_DIR=/usr/share/omarchy/themes/<name> cargo run   # preview a theme
```

After moving the repo, `cargo clean -p courier` (tests locate files via `env!("CARGO_MANIFEST_DIR")`).

## Layout

- `workspace.rs` (+ `workspace/palette.rs`): window root: sidebar, projects, imports, dialogs, command palette
- `request_editor.rs`, `environment_editor.rs`: the two main panes
- `model.rs` (YAML types, no I/O), `storage.rs` (load/save collections), `project.rs` (`.courier/` discovery)
- `secret_store.rs` (keyring / encrypted-file fallback), `credentials.rs` (spotting and hoisting literal credentials)
- `response_cache.rs` (last response per request + tidy), `http.rs` (resolving a request, incl. GraphQL bodies), `transport.rs` (HTTP/SSE/WebSocket on a tokio runtime; dropping the `Handle` cancels)
- `request_editor/sse.rs`, `request_editor/ws.rs`: live views for event streams and WebSockets
- `graphql.rs` (introspection → `Schema`, `SchemaCache`), `graphql/assist.rs` (completions, hover and validation from query text; pure, unit-tested), `request_editor/schema.rs` (fetch, Schema tab, editor providers)
- `import/` (`mod.rs`: shared `CollectionImport` + writers and format detection; curl, Postman, `openapi.rs`, `asyncapi.rs`, `spec.rs` for `$ref`s and schema examples; `COURIER_IMPORT_FILES=… cargo test real_files -- --ignored --nocapture` tries real specs), `omarchy_theme.rs`, `i18n.rs`, `settings.rs`, `paths.rs`, `ui.rs`, `encoding.rs`

## Rules

- **Secret values never touch YAML, logs or the response cache.** Collections store secret names; values go through `SecretStore` (`SecretWrite` + `apply`). Imports hoist literal credentials into secrets.
- **Every user-visible string is `t!("key")`** with an entry in `locales/*.yml` for `en`, `es`, `de` and `fr` (same `%{placeholders}`). Tests fail on missing keys. Keep low-level error details (`anyhow` context) in English.
- **Build inputs, editors and dialog footers with `crate::ui`** (`text_input`, `secret_input`, `code_editor`, `readonly_editor`, `textarea`, `dialog_footer`); bare `Input::new`/`Editor::new`/`Textarea::new` fail a test because GPUI Kit's own menus aren't translated.
- **Per-machine data lives in XDG dirs** via `AppPaths` (config: settings; state: open projects; cache: responses; data: secrets fallback). Only `.courier/` goes in projects.
- **Never block the UI thread on I/O that scales with collection size or network**; use `cx.background_executor()`.
- **Tests never touch the real keyring**: the workspace uses `SecretStore::in_memory()` under `cfg(test)`. The one real-keyring test is `#[ignore]`.

## Testing gotchas

- UI tests use `#[gpui_kit::test]` with the shared `setup`/`open_workspace` helpers in `workspace.rs` tests.
- A module that `use gpui_kit::*` exports GPUI's `test` macro, which shadows `#[test]`; test modules there add `use core::prelude::v1::test;` or import gpui-kit names explicitly.
- Events from `window.press`/`click` are delivered when the `update_window` closure ends; split steps that depend on them into separate `update_window` calls.
- UI tests that talk to `transport` need `cx.executor().allow_parking()` (done in `setup`), since events arrive from the tokio thread.
- Elements a test clicks or finds need `.test_support()` (buttons have it already).
- GPUI's test scheduler rejects wake-ups from foreign threads (e.g. oo7's file backend); test such code outside GPUI tests.
