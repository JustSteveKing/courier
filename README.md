# Courier

A fast, native API client for Linux, in the spirit of Postman and Yaak, without Electron or Tauri. Built with Rust and [GPUI](https://www.gpui.rs/) via [gpui-kit](https://github.com/longbridge/gpui-kit).

- **A scratchpad for quick requests** (<kbd>Ctrl</kbd>+<kbd>N</kbd>), always in the sidebar without creating a project; move a request into a project when it earns a place.
- **Requests live with your code.** A project's requests and environments are plain YAML in a `.courier/` folder inside the project, so they're reviewed and versioned in git like everything else.
- **Secrets stay out of git.** Collections store secret *names*; values go in your desktop keyring (GNOME Keyring, KWallet, KeePassXC), or an encrypted file when there isn't one.
- **Feels at home on Linux.** XDG directories, desktop portals for file pickers, and on [Omarchy](https://omarchy.org) it follows your theme and font live.
- **Request settings:** redirects, timeouts, TLS verification, extra CA and client certificates (mTLS), proxy, and HTTP over Unix sockets (Docker, Podman, systemd), set on a collection, folder or request.
- **Everyday tools:** query params beside the URL, auth (Basic, Bearer, API key) inherited from folders and collections, a cookie jar per collection, response history, JSONPath filtering and Copy as curl.
- **Checks:** assertions per request (`status == 200`, `$.id exists`, `header Content-Type contains json`, `time < 500`) with a pass/fail tab, and **Add check** from the JSONPath filter.
- **Request chaining:** use another request's response with `{{ response("Login", "$.token") }}` or `{{ response_header("Login", "Location") }}`; Courier sends it first when it has no response yet (add `"always"`, or a maximum age like `"5m"`, to control re-sending). Also `{{ uuid() }}`, `{{ timestamp() }}` and `{{ now() }}`. Right-click a request for **Copy response reference**.
- **Beyond plain HTTP:** streamed responses with Cancel, Server-Sent Events as a live, filterable event list with reconnect, WebSockets with a message timeline and saved message templates, and GraphQL queries with variables, plus a browsable schema, autocomplete, hover docs and mistakes underlined as you type, from introspection.
- **Start from what you have:** New project creates a blank collection or one from a Postman collection, an OpenAPI 3 / Swagger 2 spec (a request per operation with example bodies, an environment per server) or an AsyncAPI 2/3 spec (a WebSocket request per channel with message templates). You can also paste a curl command or import a Postman environment. Literal tokens are moved into secrets automatically.
- **Command palette** (<kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>P</kbd>) for requests, actions, environments and settings.
- **Languages:** English, Español, Deutsch, Français.

## Install

Requirements (Arch package names; other distros have equivalents):

- Build: `rustup` (stable Rust 1.88 or newer), `base-devel`
- Run: `wayland`, `libxkbcommon`, `vulkan-icd-loader` plus a Vulkan driver (`vulkan-radeon`, `vulkan-intel` or `nvidia-utils`), `xdg-desktop-portal` with a backend (e.g. `xdg-desktop-portal-gtk`)
- Optional: `gnome-keyring` or `kwallet` for secrets, `fontconfig` for the Omarchy font

```sh
./install.sh            # builds a release binary, installs to ~/.local
./install.sh --uninstall
```

This installs `~/.local/bin/courier`, a launcher entry and an icon, so Courier shows up in your app menu. Set `PREFIX` to install elsewhere.

## Usage

```sh
courier                 # opens the project containing the current directory, if any
courier ~/Work/my-api   # opens that project (offers to create .courier/ if it has none)
```

Or use **Open project…** in the app.

| Shortcut | Action |
| --- | --- |
| <kbd>Ctrl</kbd>+<kbd>Enter</kbd> | Send the request |
| <kbd>Ctrl</kbd>+<kbd>S</kbd> | Save the request or environment |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>P</kbd> | Command palette |

### Reading responses

The response pane follows the content type. JSON is pretty-printed and filtered with JSONPath, as before. XML and SOAP arrive indented and filter with a small XPath — `//entry/title/text()`, `//entry[2]/@id`, `*` for any element, namespaces ignored. Images render inline. HTML shows its source with **Open in browser**. Anything else that isn't text shows as a hex dump. **Save body…** writes the bytes exactly as they arrived to a file you pick.

### Paste a curl command anywhere

Paste one into the URL bar and it fills the open request — method, URL, headers and body — keeping its name. Paste it anywhere else in the window (`ctrl-v` outside a text field) and it becomes a new request next to the one you have open, with any credentials it carries hoisted into secrets. The import dialog is still in the collection menu for when you'd rather paste into a box.

### Tabs

Each request you open gets a tab; opening one that's already open moves to it. `ctrl-w` closes the current tab, `ctrl-tab` and `ctrl-shift-tab` move between them, and the strip only appears once a second request is open. Tabs come back where you left them after a restart, and a tab whose request is renamed or moved follows it.

### Finding and arranging requests

The box at the top of the sidebar filters as you type, matching a request's name, method or URL — `post pets` and `/v2/` both work — and opens whatever holds a match. Drag a request onto a folder or collection to move it there, or onto another request to drop it straight after that one; the order is saved with the requests, so it survives a reload and a checkout.

### File uploads

Pick a body kind next to **Body**: JSON, XML, text, form, **multipart form** or **file**.

- Multipart takes one part per line — `name: value`, or `name: @photos/rex.png` for a file, `#` to leave one out. **Choose a file…** adds a line for you.
- File sends that file's bytes as the whole body, with the content type guessed from its name.
- Paths are relative to the project folder, so a collection still works on someone else's machine, and `{{variables}}` work in part values. Copy as curl gives you `-F` and `--data-binary` to match.

### Runs

Right-click a collection or a folder and choose **Run**. The requests go out in sidebar order, sharing responses so `{{ response() }}` chaining works, and each one's status, time and checks appear as it finishes. **Stop on failure** stops at the first failed check, **Stop** ends a run in progress, and clicking a result opens that request with the response the run got. WebSocket and event-stream requests are skipped.

### From a terminal or CI

The same binary sends requests and runs checks without a window:

```sh
courier list                                  # requests in the project here
courier send auth/login -e staging            # body to stdout, status and checks to stderr
courier run -e staging --var base_url=http://localhost:8080
courier run smoke --bail --report junit -o report.xml
courier completions fish > ~/.config/fish/completions/courier.fish
```

`run` sends a folder (or the whole collection) in order, so `response()` chaining works, and exits 1 if any check fails or a request errors. Secrets come from the keyring; in CI set `COURIER_SECRET_<NAME>` (e.g. `COURIER_SECRET_API_TOKEN`) and pass `--no-keyring`. Cookies set during a run carry over to later requests but aren't saved. WebSocket and event-stream requests are skipped.

## What lives where

```text
my-api/.courier/                 committed with the project
  collection.yaml                name, id, default variables, secret names
  environments/local.yaml        environment variables and secret names
  users/list-users.yaml          one file per request; folders are directories

~/.config/courier/settings.yaml  language, theme, response history (per machine)
~/.local/state/courier/          open projects, active environments
~/.cache/courier/responses/      last response per request (sensitive headers masked)
~/.local/share/courier/          encrypted secrets fallback, only without a keyring
```

A request file:

```yaml
name: Create user
method: POST
url: '{{base_url}}/users'
headers:
- name: Authorization
  value: Bearer {{api_token}}
body:
  type: json
  content: |-
    { "name": "Ada" }
```

Variables use `{{name}}`. An environment overrides the collection defaults by name; secrets are resolved from the keyring only when a request is sent.

## Development

```sh
cargo run -- ~/Work/my-api
cargo test
cargo clippy --all-targets
cargo fmt
```

Tests include headless UI tests that drive the real app (clicks, typing, dialogs) without a display. `COURIER_THEME_DIR=/usr/share/omarchy/themes/<name>` previews an Omarchy theme without changing your desktop.

## License

[MIT](LICENSE)
