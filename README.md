# Courier

A fast, native API client for Linux, in the spirit of Postman and Yaak, without Electron or Tauri. Built with Rust and [GPUI](https://www.gpui.rs/) via [gpui-kit](https://github.com/longbridge/gpui-kit).

Your requests are plain YAML files inside the project they belong to, so they are reviewed and versioned like the rest of your code. Secrets never go near them. It starts in about 90 ms and idles around 100 MB.

**Speaks** HTTP · Server-Sent Events · WebSocket · GraphQL · gRPC

---

## Contents

- [Install](#install) · [First request](#first-request) · [Projects and the scratchpad](#projects-and-the-scratchpad)
- [Writing requests](#writing-requests): [variables and environments](#variables-and-environments) · [secrets](#secrets) · [auth](#auth) · [bodies and uploads](#bodies-and-uploads) · [settings](#request-settings)
- [Sending](#sending): [responses](#reading-responses) · [timing](#where-the-time-went) · [checks](#checks) · [chaining](#chaining-requests) · [runs](#running-a-folder-or-collection)
- [Protocols](#protocols): [SSE](#server-sent-events) · [WebSocket](#websockets) · [GraphQL](#graphql) · [gRPC](#grpc)
- [Moving around](#moving-around): [tabs](#tabs) · [search and drag](#finding-and-arranging-requests) · [keyboard](#keyboard) · [git](#git-awareness)
- [From a terminal or CI](#from-a-terminal-or-ci) · [Import](#importing) · [Export](#exporting)
- [Files on disk](#files-on-disk) · [Development](#development)

---

## Install

Requirements (Arch package names; other distributions have equivalents):

- **Build:** `rustup` (stable Rust 1.88 or newer), `base-devel`
- **Run:** `wayland`, `libxkbcommon`, `vulkan-icd-loader` plus a Vulkan driver (`vulkan-radeon`, `vulkan-intel` or `nvidia-utils`), `xdg-desktop-portal` with a backend such as `xdg-desktop-portal-gtk`
- **Optional:** `gnome-keyring`, `kwallet` or KeePassXC for secrets; `fontconfig` for the Omarchy font; `git` for the change marks; `protoc` is *not* needed for gRPC

```sh
./install.sh              # builds a release binary and installs into ~/.local
./install.sh --uninstall
PREFIX=/opt/courier ./install.sh
```

That installs `~/.local/bin/courier`, a launcher entry and an icon, so Courier appears in your app menu.

## First request

```sh
courier                   # opens the project containing the current directory, if any
courier ~/Work/my-api     # opens that project, offering to create .courier/ if it has none
```

Press <kbd>Ctrl</kbd>+<kbd>N</kbd> for a scratchpad request, type a URL, press <kbd>Ctrl</kbd>+<kbd>Enter</kbd>. Or paste a curl command straight into the URL bar — method, headers and body come with it.

## Projects and the scratchpad

A **project** is any folder with a `.courier/` directory in it. That directory holds the collection: one YAML file per request, folders as directories, environments in `environments/`. It belongs in git.

The **scratchpad** is always in the sidebar, needs no project, and is where quick one-off requests live. Drag a request out of it into a project when it earns a place.

---

## Writing requests

The URL bar takes `{{variables}}`, and query parameters are editable beside it — edit either and the other follows. Headers are one `Name: value` per line, with `#` to switch one off.

### Variables and environments

Variables come from three places, each overriding the one before:

1. the collection's defaults, in `collection.yaml`
2. the environment you pick (`environments/*.yaml`), or one of the project's own `.env` files
3. `--var name=value` on the command line

A project's `.env`, `.env.local`, `.env.staging` and so on appear in the environment picker after Courier's own environments. They are read, never written: Courier will not edit your `.env`, and editing it yourself updates the open request straight away. `.env.example` and friends are skipped.

Template functions work anywhere a variable does: `{{ uuid() }}`, `{{ timestamp() }}`, `{{ now() }}`.

### Secrets

Collections store secret **names**; the values live in your desktop keyring (GNOME Keyring, KWallet, KeePassXC), or in an encrypted file when there is no keyring. A secret is referenced like any variable, `{{api_token}}`, and is only resolved when a request is actually sent.

Paste a token into a header and Courier offers to move it into a secret for you. Imports do it automatically. There is a test in the suite whose whole job is to fail if a secret value ever reaches a file.

### Auth

Set auth on a **request**, a **folder** or the **collection**; anything below inherits it unless it says otherwise.

| Kind | Notes |
| --- | --- |
| Basic, Bearer, API key | Header or query parameter |
| **Digest** | The request goes out twice: once for the challenge, then signed. MD5 and SHA-256, with or without `qop` |
| **OAuth 2.0** | Client credentials, authorization code with PKCE (opens your browser, catches the redirect on a loopback port), device code. Tokens are cached in the keyring and refreshed before they expire |
| **JWT** | Signed per request from your claims: HS256/384/512 with a shared secret, RS256/384/512 or ES256 with a PEM private key. Claims take `{{variables}}` |
| **AWS Signature v4** | AWS and the S3-compatible services (MinIO, R2, B2), including session tokens |

### Bodies and uploads

Pick a body kind beside **Body**: JSON, XML, text, form, **multipart** or **file**.

- Multipart takes one part per line — `name: value`, or `name: @photos/rex.png` for a file, `#` to leave one out. **Choose a file…** adds a line for you.
- File sends that file's bytes as the whole body, with the content type guessed from its name.
- Paths are relative to the project, so a collection still works on someone else's machine.

### Request settings

Per request, folder or collection: follow redirects and how many, timeout, TLS verification, an extra CA certificate, a client certificate for mTLS, proxy (or none), and **HTTP over a Unix socket** for Docker, Podman and systemd:

```
unix:///run/docker.sock:/v1.45/containers/json
```

---

## Sending

### Reading responses

The response pane follows the content type.

- **JSON** — pretty-printed, filtered with JSONPath (`$.items[0].id`), and **Add check** turns the filtered value into an assertion.
- **XML** — indented, filtered with a small XPath: `//entry/title/text()`, `//entry[2]/@id`, `*` for any element. Namespace prefixes are ignored.
- **Images** render inline; **HTML** shows its source with **Open in browser**; anything else binary shows as a hex dump.
- **Save body…** writes the exact bytes to a file.

Every request keeps a **history** of its recent responses, and the last one is restored when you reopen it.

### Where the time went

The **Timing** tab breaks a response down into DNS, connecting (TCP and the TLS handshake), waiting for the server and downloading, as a waterfall, with the address reached and the body size. A request that reused a pooled connection says so.

### Checks

Assertions live with the request, one per line, `#` to switch one off:

```
status == 200
$.id exists
$.items[0].name == Rex
header Content-Type contains json
time < 500
```

They run after every send, pass or fail in their own tab, and run identically in the CLI.

### Chaining requests

Use another request's response anywhere a variable goes:

```
{{ response("Login", "$.token") }}
{{ response_header("Login", "Location") }}
```

Courier sends that request first if it has no response yet. Add `"always"` to send it every time, or a maximum age like `"5m"`. Right-click a request for **Copy response reference**.

### Running a folder or collection

Right-click a collection or folder → **Run**. Requests go out in sidebar order, sharing responses so chaining works, and each result appears as it finishes with its status, time and checks. **Stop on failure** bails at the first failure; clicking a result opens that request with the response the run got.

---

## Protocols

### Server-Sent Events

A request that asks for `text/event-stream` gets a live event list with filtering, the last event id, and reconnect.

### WebSockets

A `ws://` or `wss://` URL gets a message timeline with everything sent and received, plus saved message templates to send again.

### GraphQL

A query editor with variables and an operation name. Courier introspects the schema, then offers autocomplete, hover documentation, a browsable schema tab, and underlines fields the schema doesn't have.

### gRPC

New request → **New gRPC call**, then point it at `grpc://host:port` (`grpcs://` for TLS). Courier reads what the server offers through its **reflection** service, or from **`.proto` files** you name (one per line, relative to the project). `protoc` is not needed and nothing is generated ahead of time.

Pick a method, write the message as JSON, send. Unary calls answer once; streaming calls — server, client or both ways — fill the message timeline as replies arrive, with the closing status and trailers at the end. Headers go out as metadata.

---

## Moving around

### Tabs

Each request you open gets a tab; opening one that's already open moves to it. Tabs come back after a restart and follow a request through a rename.

### Finding and arranging requests

The box above the sidebar filters as you type, matching a request's name, method or URL. Drag a request onto a folder or collection to move it, or onto another request to place it after that one — the order is saved with the requests, so it survives a checkout.

### Keyboard

| Shortcut | Action |
| --- | --- |
| <kbd>Ctrl</kbd>+<kbd>Enter</kbd> | Send |
| <kbd>Ctrl</kbd>+<kbd>S</kbd> | Save |
| <kbd>Ctrl</kbd>+<kbd>N</kbd> | New scratchpad request |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>P</kbd> | Command palette |
| <kbd>Ctrl</kbd>+<kbd>1</kbd> / <kbd>2</kbd> / <kbd>3</kbd> | Sidebar · URL · body |
| <kbd>Ctrl</kbd>+<kbd>Tab</kbd> · <kbd>Ctrl</kbd>+<kbd>W</kbd> | Next tab · close tab |
| <kbd>F1</kbd> | Shortcuts sheet |

In the sidebar the arrow keys walk the tree: right opens a folder or steps into it, left closes it or steps out, <kbd>Enter</kbd> opens a request. Every shortcut can be rebound in `~/.config/courier/keymap.yaml`.

### Git awareness

When the project is in git, the sidebar marks what differs from the last commit — `+` new, `●` changed, `−` deleted — folders carry a dot when something inside them changed, and the branch shows in the header. **Show changes** opens a request's diff against HEAD. Courier only reads; committing stays in your own tools.

---

## From a terminal or CI

The same binary sends requests and runs checks without a window.

```sh
courier list                                   # requests in the project here
courier envs
courier send auth/login -e staging             # body to stdout, status and checks to stderr
courier run -e staging --var base_url=http://localhost:8080
courier run smoke --bail --report junit -o report.xml
courier completions fish > ~/.config/fish/completions/courier.fish
```

`run` sends a folder, or the whole collection, in order, with chaining and checks. **Exit codes:** 0 when everything passed, 1 when a check failed or a request errored, 2 for setup mistakes such as an unknown environment.

Secrets come from the keyring. In CI, set `COURIER_SECRET_<NAME>` (for example `COURIER_SECRET_API_TOKEN`) and pass `--no-keyring`. Cookies set during a run carry over to later requests but aren't saved. WebSocket, SSE and gRPC requests are skipped by a run.

## Importing

**New project** (or **Import here** on an existing collection) starts from what you already have:

| From | What comes across |
| --- | --- |
| **Postman** collection | Requests, folders, variables, auth, scripts reported as warnings |
| **OpenAPI 3 / Swagger 2** | A request per operation with example bodies, an environment per server |
| **AsyncAPI 2/3** | A WebSocket request per channel with message templates |
| **Insomnia** v4 or v5 | Requests, folders, environments, parameters, auth |
| **HAR** recording | One request per method and path, browser noise and cookies left out, first host lifted into `base_url` |
| **curl** | Paste anywhere: into the URL bar it fills the open request, elsewhere it makes a new one |

Literal tokens found on the way are moved into secrets. Anything that couldn't be carried across is reported rather than dropped silently.

## Exporting

Right-click a collection to export it as a **Postman collection (v2.1)** or an **OpenAPI 3.1 skeleton** — paths, parameters and your bodies as examples with a rough schema. Right-click a request for **Export history as HAR**, which includes the timing breakdowns. Secret values never leave the keyring: exports carry the `{{names}}`.

---

## Files on disk

```text
my-api/.courier/                 committed with the project
  collection.yaml                name, id, default variables, secret names
  environments/local.yaml        environment variables and secret names
  users/list-users.yaml          one file per request; folders are directories

~/.config/courier/settings.yaml  language, theme, response history (per machine)
~/.config/courier/keymap.yaml    your shortcut changes
~/.local/state/courier/          open projects, open tabs, active environments
~/.cache/courier/responses/      last responses per request (sensitive headers masked)
~/.local/share/courier/          scratchpad, and the encrypted secrets fallback
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
checks:
- status == 201
- $.id exists
```

## Development

```sh
make            # list every target
make run        # cargo run
make test       # the whole workspace
make check      # fmt, clippy and tests, as CI would
```

Every push runs formatting, clippy and the full suite in CI; tagging `v0.1.0` builds release tarballs and puts them on a GitHub release.

See [AGENTS.md](AGENTS.md) for using Courier from scripts and coding agents, and [CLAUDE.md](CLAUDE.md) for how the code is laid out.

Tests include headless UI tests that drive the real app — clicks, typing, dialogs — with no display, and real local servers for HTTP, TLS, WebSocket and gRPC. `COURIER_THEME_DIR=/usr/share/omarchy/themes/<name> make run` previews an Omarchy theme without changing your desktop.

## License

[MIT](LICENSE)
