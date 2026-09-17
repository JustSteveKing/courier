# Courier

Native Linux API client (Postman/Yaak-style) in Rust on gpui-kit. README.md is the product;
AGENTS.md is how scripts and agents drive it; this file is how to work on the code.

## Commands

`make` lists everything. The ones that matter:

```sh
make check                  # fmt-check, clippy and the whole test suite, as CI would
make test                   # cargo test --workspace (app UI tests + core + cli)
make test-one T=name        # one test, with its output
make run PROJECT=~/Work/my-api
make restart                # rebuild and restart the running app
make run THEME=tokyo-night  # preview an Omarchy theme
```

Clippy must be clean; rustfmt.toml sets max_width 120.

After moving the repo, `cargo clean -p courier` (tests locate files via `env!("CARGO_MANIFEST_DIR")`).

## Layout

Two crates: **`crates/courier-core`** is the engine with no UI (model, storage, project, paths, secrets, credentials, cookies, http/transport, chain, response cache, graphql schema/assist, template assist, importers); **`crates/courier-cli`** is the command line (`send`/`run`/`list`/`envs`/`completions`, on `runner.rs` in core; the app's `main.rs` forwards those subcommands to it); the root **`courier`** crate is the GPUI app. The app re-exports core modules in `main.rs`, so `crate::model` works in app code. Keep GPUI out of core; test-only helpers there sit behind the `test-support` feature.


- `workspace.rs` (+ `workspace/palette.rs`): window root: sidebar, projects, imports, dialogs, command palette, tabs (`open_tabs` holds paths; one `RequestEditor` serves them all, since it already keeps responses and filters per request)
- `request_editor.rs`, `environment_editor.rs`, `runner_view.rs`: the main panes (`MainView` picks one)
- Runs: `runner.rs` in core (shared with the CLI), `runner_view.rs` for the live results; the workspace builds the context in `run_setup`
- `model.rs` (YAML types, no I/O), `storage.rs` (load/save collections), `project.rs` (`.courier/` discovery)
- `digest.rs` (HTTP Digest; like sigv4 it runs after resolve, and it asks the server for a challenge first)
- `jwt.rs` (signing JSON Web Tokens: HMAC, RSA, ECDSA) and `sigv4.rs` (AWS Signature v4; `apply` runs *after* `Request::resolve`, since the signature covers the finished request)
- `oauth.rs` (OAuth 2.0 tokens: fetch, refresh, cache in the secret store keyed by a fingerprint of the settings; `authorize` swaps `Auth::OAuth2` for the header before a request resolves)
- `secret_store.rs` (keyring / encrypted-file fallback), `credentials.rs` (spotting and hoisting literal credentials)
- Request settings: `model::RequestSettings` (collection → folder `.folder.yaml` → request, merged by `overlay`, resolved with `resolve`), `transport::ClientOptions`/`client_for` (cached clients), `settings_form.rs` (dialog)
- `chain.rs` (template functions: `response()`/`response_header()` chaining, `uuid()` etc.; evaluated in `resolve_in_background` before `Request::resolve`)
- `response_cache.rs` (last response per request + tidy), `http.rs` (resolving a request, incl. GraphQL and upload bodies: `Request::resolve_in` takes the project dir, since upload paths are relative to it), `transport.rs` (HTTP/SSE/WebSocket on a tokio runtime; dropping the `Handle` cancels; a timed DNS resolver and a connector layer report `Phases` through a task-local, which becomes `response_cache::Timing`)
- `request_editor/sse.rs`, `request_editor/ws.rs`: live views for event streams and WebSockets
- `git.rs` in core: shells out to `git` for status, branch and diffs (read-only); the workspace keeps a `git` map per collection, refreshed in the background on load, save and file changes
- `export.rs` in core: collection → Postman v2.1 / OpenAPI 3.1, request history → HAR; tests round-trip each through our own importers
- `dotenv.rs` in core: reading a project's `.env` files (listed in `Collection.env_files`, picked like an environment, watched for changes in `workspace::watch_env_files`)
- `body_view.rs` in core: what a response body is (from Content-Type, then sniffing), XML pretty-printing and a small XPath; the editor picks its viewer from it
- `grpc.rs` in core: descriptors from `.proto` (protox) or reflection, a tonic codec over `DynamicMessage`, and all four call shapes; `grpc::test_support` runs a real server for tests in either crate. `request_editor/grpc.rs` is the pane
- `graphql.rs` (introspection → `Schema`, `SchemaCache`), `graphql/assist.rs` (completions, hover and validation from query text; pure, unit-tested), `request_editor/schema.rs` (fetch, Schema tab, editor providers)
- `keymap.rs`: every shortcut in one table (action name, keys, context, description), overridden by `keymap.yaml` in the config dir; modules register actions in `init` but bind nothing, and `keymap::apply` runs after them (tests call it in `setup` too)
- `import/` (`mod.rs`: shared `CollectionImport` + writers and format detection; curl, Postman, `openapi.rs`, `asyncapi.rs`, `har.rs`, `insomnia.rs`, `spec.rs` for `$ref`s and schema examples; `COURIER_IMPORT_FILES=… cargo test real_files -- --ignored --nocapture` tries real specs), `omarchy_theme.rs`, `i18n.rs`, `settings.rs`, `paths.rs`, `ui.rs`, `encoding.rs`

## Rules

- **Secret values never touch YAML, logs, the response cache or any plain file.** Cookie jars count as secrets: they're saved in the secret store (`cookies.rs`).
- **Secret values never touch YAML, logs or the response cache.** Collections store secret names; values go through `SecretStore` (`SecretWrite` + `apply`). Imports hoist literal credentials into secrets.
- **Every user-visible string is `t!("key")`** with an entry in `locales/*.yml` for `en`, `es`, `de` and `fr` (same `%{placeholders}`). Tests fail on missing keys. Keep low-level error details (`anyhow` context) in English.
- **Build inputs, editors and dialog footers with `crate::ui`** (`text_input`, `secret_input`, `code_editor`, `readonly_editor`, `textarea`, `dialog_footer`); bare `Input::new`/`Editor::new`/`Textarea::new` fail a test because GPUI Kit's own menus aren't translated.
- **Per-machine data lives in XDG dirs** via `AppPaths` (config: settings; state: open projects; cache: responses, schemas; data: secrets fallback and the scratchpad collection at `data/scratchpad/.courier`). Only `.courier/` goes in projects. The scratchpad is always `collections[0]`, never in `open_projects`, can't be closed; use `projects()` when you mean project collections.
- **Never block the UI thread on I/O that scales with collection size or network**; use `cx.background_executor()`.
- **Tests never touch the real keyring**: the workspace uses `SecretStore::in_memory()` under `cfg(test)`. The one real-keyring test is `#[ignore]`.

## Testing gotchas

- UI tests use `#[gpui_kit::test]` with the shared `setup`/`open_workspace` helpers in `workspace.rs` tests.
- A module that `use gpui_kit::*` exports GPUI's `test` macro, which shadows `#[test]`; test modules there add `use core::prelude::v1::test;` or import gpui-kit names explicitly.
- Events from `window.press`/`click` are delivered when the `update_window` closure ends; split steps that depend on them into separate `update_window` calls.
- UI tests that talk to `transport` need `cx.executor().allow_parking()` (done in `setup`), since events arrive from the tokio thread.
- Elements a test clicks or finds need `.test_support()` (buttons have it already).
- GPUI's test scheduler rejects wake-ups from foreign threads (e.g. oo7's file backend); test such code outside GPUI tests.
