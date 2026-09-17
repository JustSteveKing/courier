# Courier for agents

Courier is an API client whose collections are plain YAML files and whose command line runs
them without a window. That makes it usable by a coding agent in two ways: **write requests
as files**, and **run them from the terminal**, reading the JSON report back.

Nothing here needs the app to be open. The app and the CLI are the same binary and the same
engine, so a request behaves identically in both.

---

## The shape of a collection

A project is any folder containing `.courier/`:

```text
my-api/.courier/
  collection.yaml            name, id, default variables, secret names
  environments/staging.yaml  variables and secret names for one environment
  auth/login.yaml            one file per request; folders are directories
  users/list-users.yaml
```

`collection.yaml`:

```yaml
version: 1
id: 5f1e...                  # keep this: it links the collection to its secrets
name: My API
variables:
  base_url: https://api.example.com
secrets:                     # names only, never values
- api_token
```

A request file:

```yaml
name: Create user            # what the sidebar and the CLI call it
method: POST
url: '{{base_url}}/users'
headers:
- name: Content-Type
  value: application/json
- name: X-Debug               # switched off, kept for later
  value: '1'
  enabled: false
body:
  type: json                 # json | xml | text | form-urlencoded | multipart | file
  content: |-
    { "name": "{{name}}" }
auth:                        # omit to inherit from the folder or collection
  type: bearer
  token: '{{api_token}}'
checks:
- status == 201
- $.id exists
order: 2                     # sidebar position within its folder
```

Rules worth knowing before writing one:

- **Never put a secret value in these files.** Put the *name* in `collection.yaml`'s
  `secrets:` and reference it as `{{name}}`. Values come from the keyring, or from
  `COURIER_SECRET_<NAME>` environment variables.
- `{{variables}}` resolve from the collection, then the environment, then `--var`.
- A file's name doesn't matter to Courier; `name:` is what identifies a request to a person
  and to `courier send`.
- Folders are directories. A `.folder.yaml` inside one carries that folder's auth and
  settings; it is optional.

## Running requests

```sh
courier list -p my-api                         # every request, with its path and method
courier envs -p my-api                         # environments that can be picked
courier send "Create user" -p my-api -e staging
courier run -p my-api                          # the whole collection, in order
courier run users -p my-api --bail             # one folder, stopping at the first failure
```

`send` takes a request's name or its path within the collection (`users/list-users`).
`run` takes a folder or request, or nothing for everything.

Useful flags: `-e/--env NAME`, `--var NAME=VALUE` (repeatable), `--timeout SECONDS`,
`--no-keyring`, `--bail`, `--report pretty|json|junit`, `-o FILE`, `-i/--include` (send
only: print the status line and headers before the body).

**Exit codes:** `0` everything passed · `1` a check failed or a request errored · `2` a
setup mistake, such as an unknown environment or a missing collection.

**Streams are skipped by a run**: WebSocket, Server-Sent Events and gRPC requests report as
skipped with a reason rather than hanging a pipeline.

## Reading results

`--report json` is the one to parse:

```sh
courier run -p my-api --report json
```

```json
{
  "collection": "My API",
  "results": [
    {
      "request": "users/list-users.yaml",
      "name": "List users",
      "method": "GET",
      "status": "passed",
      "response_status": 200,
      "elapsed_ms": 34,
      "checks": [{ "check": "status == 200", "passed": true, "actual": "200" }]
    }
  ],
  "summary": { "passed": 1, "failed": 0, "errors": 0, "skipped": 0, "elapsed_ms": 34, "stopped_early": false }
}
```

`status` is `passed`, `failed`, `error` (with a `message`) or `skipped` (with a `reason`).
`--report junit` writes what CI systems already understand.

`courier send` puts **only the response body on stdout**, and the status line, timing and
check results on stderr, so it pipes cleanly:

```sh
courier send "List users" -p my-api | jq '.[0].id'
```

## Secrets in an automated environment

```sh
COURIER_SECRET_API_TOKEN=... courier run -p my-api --no-keyring
```

Every name in `secrets:` can be supplied this way: uppercase it and replace anything that
isn't a letter or digit with `_`. `--no-keyring` keeps Courier from trying to open a keyring
that isn't there — on a headless machine, do use it.

Do not write secret values into the collection, into a `.env` that is committed, or into a
report. Courier masks sensitive response headers in its cache; it cannot mask what you put
in a file yourself.

## Writing a request from a script

There is no "add request" command yet; write the YAML. A minimal, valid request is:

```sh
mkdir -p my-api/.courier
cat > my-api/.courier/collection.yaml <<'YAML'
version: 1
id: 11111111-2222-3333-4444-555555555555
name: My API
variables:
  base_url: https://api.example.com
YAML
cat > my-api/.courier/health.yaml <<'YAML'
name: Health
method: GET
url: '{{base_url}}/health'
checks:
- status == 200
YAML
courier run -p my-api --report json
```

Courier picks the files up the next time it reads the collection; a running app notices when
the folder changes.

## Chaining, when one request needs another's answer

```yaml
name: Get me
method: GET
url: '{{base_url}}/users/{{ response("Login", "$.id") }}'
headers:
- name: Authorization
  value: Bearer {{ response("Login", "$.token") }}
```

`response("Name", "$.json.path")` and `response_header("Name", "Header")` send that request
first if it has no answer yet, and reuse it afterwards. Add `"always"` for every time, or a
maximum age like `"5m"`. Within a `courier run`, the responses are shared, so a login at the
top of a folder covers everything below it.

## Turning other formats into a collection

The importers are part of the engine, so anything Courier can import is one step away from
being a collection in your project: Postman, OpenAPI 3 / Swagger 2, AsyncAPI, Insomnia (v4
and v5), HAR recordings, and curl commands. In the app that's **New project** or **Import
here**; from a script, the honest answer today is that import is UI-only — write the YAML
directly, which is simple enough.

## What Courier will not do

- **It will not commit anything.** Git awareness in the app is read-only.
- **It will not write your `.env` files.** They're read as environments, never edited.
- **It will not put secret values in a file.** If you need a value in CI, pass it in.

## Working on Courier itself

```sh
make check        # fmt, clippy and the full test suite, as CI would run it
make test-one T=name_of_test
```

See [CLAUDE.md](CLAUDE.md) for the code layout and the house rules (translations for every
user-visible string, secrets never in plain files, UI tests for every feature).
