# Glimpse Plugin API v1

A plugin is a folder containing `plugin.toml` and its program. Any language can use
JSON lines over stdin/stdout. The lightweight Rust SDK is `glimpse-plugin-sdk`, under
`crates/glimpse-plugin-sdk`; it has no GTK or inference dependency.

```toml
id = "my-provider"
name = "My Provider"
version = "0.1.0"
api_version = 1
exec = ["python3", "main.py"]
capabilities = ["search", "preview", "settings"]
needs = ["python3"]

[permissions]
network = false
selected_sources = false
read = []
```

Executables are relative to the package or a basename on the system PATH. Arguments
are arrays; no shell evaluation occurs. `file_types = ["note"]` declares extensions
for reader plugins. `selected_sources = true` requests the currently selected source
folders; they are listed before enabling and the grant does not expand when folders
are later added. `read` can request explicit selected source paths. The host masks
hidden paths, credentials, generated folders, and explicit exclusions.

A helper receives hello, one operation, and shutdown per invocation. Processes are
bounded and restarted between requests, so store durable settings in
`$GLIMPSE_STORAGE`. Diagnostics belong on stderr. Helpers must not emit other stdout.

```json
{"api_version":1,"id":0,"op":"hello"}
{"api_version":1,"id":1,"op":"search","args":{"query":"uuid","limit":10}}
{"api_version":1,"id":2,"op":"shutdown"}
```

Reply to hello with `{"id":0,"api_version":1,"capabilities":["search"]}`.
Operations reply with `{"id":1,"result":...}` or `{"id":1,"error":"message"}`.
The maximum reply is 1 MiB; ordinary operations have a two-second deadline, readers
have ten seconds. Helpers receive 512 MiB address space and ten CPU seconds.
Disabling, removing, or changing grants invalidates and cancels outstanding work.

## Extension points

- **Search / commands:** `search` receives query and limit. Return an array of
  `{id,title,subtitle,action}`. The host displays at most ten results per plugin.
- **Actions:** `copy` carries `text`; `open` carries a granted `path`; `url` carries
  HTTP(S) `url` and requires network consent. `activate` carries a plugin `token`.
  The host calls `activate` only after user selection; its reply must be a concrete
  copy/open/url action. All plugin computation stays in its sandbox.
- **Previews:** `preview` receives `result_id`, title, and query. Return `{text}`.
  The host renders bounded plain text, not plugin markup or code.
- **Readers:** `extract` receives `path`, with only that source file mounted for the
  invocation. Return up to 128 `{text,page?,seconds?,line?}` segments totaling at
  most 512 KiB. Pages and lines are one-based; seconds are finite and nonnegative.
  Reader text joins filename/content search and the semantic indexing pipeline.
- **Settings:** `settings` returns `{rows:[...]}` with entry, switch, choice, or
  slider rows. Rows carry key/title/value; choice adds ID/label option pairs; slider
  adds min/max/step. `configure` receives key and typed value. The plugin validates
  and saves it. The Rust SDK supplies `SettingsPage` and `Setting` types.

```json
[{"id":"uuid","title":"new UUID","subtitle":"Copy","action":{"kind":"copy","text":"..."}}]
```

Glimpse owns rendering, sorting, keyboard navigation, scope validation, and host
activation. Plugins cannot supply GTK widgets, shell commands, or privilege elevation.
No sandbox availability means no plugin enablement.

## Examples and development

`examples/hello` is the bundled Quick Tools provider. `examples/notes-reader`
demonstrates a custom format and a native switch. `examples/rust-plugin` uses the SDK:

```sh
cargo build --release --manifest-path apps/glimpse/examples/rust-plugin/Cargo.toml
cp apps/glimpse/examples/rust-plugin/target/release/text-tools apps/glimpse/examples/rust-plugin/text-tools
nixe-glimpse plugins install-local apps/glimpse/examples/rust-plugin
```

For rapid development, use `plugins link-local DIRECTORY`. It is explicit and starts
disabled; edits are read by subsequent invocations. Bump manifest versions and enable
again after manifest changes. Production installs copy the package, reject symlinks,
and never run installation scripts. Rollback restores a previous package disabled,
so its permissions can be reviewed again.

Catalog entries contain ID/name/description, repository, immutable commit hash, and
optional subfolder. The built-in catalog works offline; remote refreshes and updates
are user actions. Catalog entries can be added without changing the runtime API.
