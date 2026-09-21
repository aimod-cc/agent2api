# Agent2API · Multi-Provider Local Gateway

[简体中文](./README.md) | **English**

Wraps the login state of several AI desktop clients into a local **OpenAI-compatible API gateway**, exposing a single `base_url` and bundling multi-provider account management, model management (enable / disable / delete / alias), content redaction, egress proxying and request reporting — plus a ready-to-run Tauri desktop app. Any OpenAI client that accepts a custom `base_url` can call these providers' model quota through `http://127.0.0.1:3065/v1` — no API key, no client source changes needed.

```
OpenAI client / any SDK
        │  POST /v1/chat/completions   (OpenAI-compatible, SSE)
        ▼
  Agent2API gateway (in-process Rust service)   ← local 127.0.0.1:3065
  model mapping · account candidate chain (global priority) · 429 fallback · egress proxy · content redaction
        │  HTTPS (the model name decides which provider is called)
        ├──▶ workbuddy  copilot.tencent.com (China) / www.workbuddy.ai (Global)
        ├──▶ raccoon    xiaohuanxiong.com/api/web/llm/v2 · Authorization: Bearer <JWT>
        ├──▶ catpaw     ai.catpaw.meituan.com · Cookie: X-Passport-Token=… + user-uid
        │                (its own conversation session protocol)
        ├──▶ autoclaw   autoglm-acceleration-api.zhipuai.cn/autoclaw-proxy/proxy/autoclaw
        │                X-Authorization: Bearer <token> (OpenAI-compatible)
        └──▶ qoder      api3.qoder.sh (Global) / gateway.qoder.com.cn (China)
                         COSY self-signed headers (not Bearer) · envelope-style SSE (custom encoding and signing)
```

> **This project is for learning and discussion only.** It reuses the login state of your own accounts through a local reverse proxy; forwarding requests in the shape of a non-official client may violate the upstream services' terms of service, and any risk (including rate limiting or account bans) is borne by the user. Commercial use and circumventing billing are prohibited. See [Usage Notice](#usage-notice) and [LICENSE](./LICENSE).
>
> This is a personal, local-purpose proxy tool. It is unaffiliated with Tencent (WorkBuddy), Meituan (CatPaw), SenseTime (Raccoon), Zhipu (AutoClaw/autoglm) or Alibaba (Qoder) and their official products; every interface shape comes from observing each vendor's desktop client traffic, and upstream may change at any time.

---

## Table of Contents

- [Quick Start](#quick-start)
- [Data Storage](#data-storage)
- [Gateway API](#gateway-api)
- [Project Layout](#project-layout)
- [Development & Build](#development--build)
- [Usage Notice](#usage-notice)
- [License](#license)

---

## Quick Start

Download the installer from Releases (NSIS, Simplified Chinese, installs to `C:\Program Files\Agent2API` by default, and needs administrator approval during setup), then launch it — **no Node or any other runtime required**:

> When upgrading from the 1.x "install for current user" layout (`%LOCALAPPDATA%\<product name>`), the new version cleans up that old installation on first launch: it first confirms the directory really holds this product's main executable, then removes the directory, the Start Menu / desktop shortcuts, the uninstall registry entry and any dead run-at-login registration; if the old directory is still in use (cannot be deleted) or is not this product, it is skipped. This cleanup runs in release builds only — running `tauri dev` during development will not touch the official build installed on your machine. The data directory is unaffected (migration copies).

1. First launch starts the local gateway (port 3065) inside the app process and opens the main window. If the 1.x data directory `~/.workbuddy-proxy` is detected, it is **copied wholesale** to `~/.agent2api` (the old directory is kept, so you can roll back). Account and history import is described in [Data Storage](#data-storage) above: if JSON / JSONL data files from an older version are found, a dialog appears at startup and waits for you to press "Upgrade" — after that, the old account data from all providers (the old gateway account file plus each vendor's desktop login state) is imported as well.
2. Click "Login / Add account" on the Report or Accounts page and **pick a provider in the dialog** (WorkBuddy / Raccoon / CatPaw / AutoClaw / Qoder), then finish that vendor's login or fill in its credentials. WorkBuddy only supports web login (embedded window or system browser); Raccoon supports web login, pasting a token, and "import desktop login state from this machine"; CatPaw supports web login, pasting credentials, and "import desktop login state from this machine"; AutoClaw supports SMS code login, pasting credentials, and "import desktop login state from this machine"; Qoder supports web login (both the Global and China sites) and a personal access token (PAT) — web login and the Global / China sites are one and the same device-authorization flow, so whichever site you pick is the site you sign in to. Importing desktop login state reuses the desktop client's own login-state file directly: no token is stored in the account record, and the gateway follows as soon as the client signs in again (Qoder has no such option).
3. Set your OpenAI client's `base_url` to `http://127.0.0.1:3065/v1` and put anything in `api_key` (for example `sk-local`; the server does not check it while authentication is disabled).

Closing the window only minimizes to the tray by default, and the gateway keeps forwarding in the background; to quit for real, right-click the tray icon and choose "Exit".

All five providers' accounts sit in one **global queue**: priority is globally unique (the smaller number is tried first), "Set as preferred" moves an account to the head of the queue, and new accounts go to the tail. The list does not mark which account a request is currently using. When forwarding, candidates are tried in ascending priority order, skipping accounts that are disabled, do not offer that model, or are in a rate-limit cooldown for that model — so "which provider goes first" is decided by account priority alone, with no second layer of provider routing priority. When an account hits a 429 on a model, that account × model pair is marked for cooldown and the request falls back to the next candidate; only when every candidate is unavailable is the last real error passed through.

### Verification

Once the gateway is up, use curl to confirm connectivity (put any name that actually exists in `GET /v1/models` into `model`):

```bash
curl http://127.0.0.1:3065/health
curl http://127.0.0.1:3065/v1/models

curl http://127.0.0.1:3065/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

Client example (Python SDK):

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:3065/v1", api_key="sk-local")
resp = client.chat.completions.create(
    model="deepseek-v4.1-flash",     # whichever provider owns it — see GET /v1/models
    messages=[{"role": "user", "content": "Hello"}],
)
print(resp.choices[0].message.content)
```

---

## Data Storage

Everything lives in **a single SQLite database**: `~/.agent2api/agent2api.db` (the config directory can be overridden with the `AGENT2API_PROXY_HOME` environment variable). Inside, data is split by purpose — `accounts`, `logs` (system events), `requests` / `request_daily` (per-request records and daily aggregates), `debug_traffic` (raw upstream payloads captured in debug mode), and `kv` (gateway config plus assorted small state). Settings → General → Data Storage shows the database path, its size, and the row count of each table.

The database runs in WAL mode, so while the app is running you will also see `agent2api.db-wal` and `agent2api.db-shm` next to it. Include them when backing up (or quit the app first — it checkpoints the WAL back into the main file on exit).

> **Upgrading from an older version**: earlier versions scattered data across 8 JSON / JSONL files (`accounts.json`, `config.json`, `logs.jsonl`, `requests.jsonl`, `request-daily.jsonl`, `debug-traffic.jsonl`, `desensitize.json`, `desktop-settings.json`). On first launch the new version **detects** them and shows a dialog explaining that storage has moved to SQLite; the import only starts after you press "Upgrade" in that dialog. Choosing "Later" skips the import for this run (accounts and history stay unavailable, and the dialog appears again on the next launch).
>
> After a successful import the old files are **renamed** to `name.migrated` (for example `accounts.json.migrated`) and kept in place as backups — they are **never deleted**. You can open them at any time to roll back or cross-check your data; rename one back and restart to be prompted to upgrade again.

---

## Gateway API

Outward-facing there is only the OpenAI-compatible chat path (by default `http://127.0.0.1:3065`):

| Method | Path | Description |
| --- | --- | --- |
| POST | `/v1/chat/completions` | Chat. `stream: true` streams SSE straight through; `stream: false` is aggregated by the gateway and returned as one JSON body |
| GET | `/v1/models` | Aggregated model list (OpenAI `list` format, with context length and capability flags; `owned_by` is the provider that actually serves it) |
| GET | `/health` | Health check (whether login state is configured, upstream address, token expiry) |

- **Model names use the upstream's original names by default**; the gateway adds no prefix. The list is the aggregate across providers: when two providers share a model name, the entry keeps the one earlier in registry order (`owned_by` records the provider actually serving it), but which provider a request goes to is decided by **account priority** — the candidate chain is "every account offering that model, ordered by global priority", tried one by one, and only when all fail is the last real error passed through.
- A model you name must actually exist in the catalog, otherwise the gateway returns 400 (`code: "model_not_found"`) with a near-name suggestion — silently swapping `deepseek-v4.1-flash` for another model would cause hard-to-notice incidents like "requesting A but actually running B", so there is no silent fallback.
- **The model management page can define mappings**: give an upstream model an external alias (alias → target); when a downstream request uses the alias, the gateway rewrites it to the target model before forwarding. The alias also appears in `/v1/models` as its own entry (`is_default` is always false). An alias may point to only one target and must not collide with any upstream model id. Disabled or deleted models do not appear in `/v1/models`, and requesting one returns 400 `model_not_found` (deleting only hides it from the list; it can be restored from the "Deleted" filter on the management page).
- **The list contains chat models only** (internal completion / tool models, image and video models never appear in the outward catalog), and it **advertises only providers that currently have usable login state**.
- Only the chat path is implemented: completion, embeddings, image and video paths have no forwarding implementation (they return 404).
- On upstream rate limiting (HTTP 429) the error body carries `type: "rate_limit_exceeded"` together with `reset_at` (a timestamp) and `reset_at_text` (local-time text).

The desktop UI's own `/api/*` management endpoints (account CRUD, model management, gateway keys, proxy, redaction, logs, reports, updates) are an internal contract that evolves with the interface and is not documented here.

---

## Project Layout

Both the gateway and the desktop app live under `desktop-tauri/`: the backend is a Rust in-process HTTP server under `src-tauri/`, the frontend is plain HTML/CSS/JS under `ui/`.

```
agent2api/
├─ desktop-tauri/
│  ├─ src-tauri/src/
│  │  ├─ server/                 Gateway implementation (Rust, in-process HTTP server)
│  │  │  ├─ mod.rs               Service assembly: ServerState, start, stop, startup migration
│  │  │  ├─ http.rs              Route table, CORS, API key middleware, body limits
│  │  │  ├─ config.rs / logging.rs / logs_store.rs / errors.rs
│  │  │  ├─ config_migration.rs  1.x config directory migration (~/.workbuddy-proxy → ~/.agent2api, first startup step)
│  │  │  ├─ request_stats.rs + request_stats/   Statistics time windows, writes, aggregation and trimming
│  │  │  ├─ core/
│  │  │  │  ├─ providers/        ★ Multi-provider layer (the heart of this work)
│  │  │  │  │  ├─ mod.rs        ProviderKind (the five providers) + PROVIDERS registry + id lookups
│  │  │  │  │  ├─ adapter.rs    ProviderAdapter trait + adapter_for + implemented_kinds
│  │  │  │  │  ├─ router.rs     Model name → set of candidate providers (aggregate catalog)
│  │  │  │  │  ├─ catalog.rs    Aggregate model catalog (list merging / same-name dedup / availability)
│  │  │  │  │  ├─ refresh_flight.rs  Single-flight dedup for credential refresh
│  │  │  │  │  ├─ workbuddy.rs  WorkBuddy adapter (header set / system injection / 6004 / 11128)
│  │  │  │  │  ├─ raccoon/      Raccoon: mod / models / credentials / jwt / oauth / balance
│  │  │  │  │  ├─ catpaw/       CatPaw: adapter (is_stateful) / conversation (turn state machine) /
│  │  │  │  │  │                turn_executor / prepare / decision (turn decisions) / fingerprint /
│  │  │  │  │  │                registry/ (session registry: table and handles / account identity / invalidation) /
│  │  │  │  │  │                messages / blocks / tools / openai (translation layer) /
│  │  │  │  │  │                upstream_http / image_compress / models / credentials / balance
│  │  │  │  │  ├─ autoclaw/     Zhipu autoglm: adapter / credentials / refresh / crypto / models /
│  │  │  │  │  │                balance / login (SMS code) / checkin (daily check-in task)
│  │  │  │  │  └─ qoder/        Qoder: adapter / endpoints (both sites) / oauth (device authorization) /
│  │  │  │  │                   auth / cosy (COSY signing and body encoding) / protocol (envelope decoding) /
│  │  │  │  │                   chat (session-style forwarding) / stream / machine (PKCE and machine id) /
│  │  │  │  │                   credentials / refresh / models / balance
│  │  │  │  ├─ upstream/        Forwarding orchestration: global account queue loop (provider_loop) + request body
│  │  │  │  │                   handling (payload) + SSE passthrough/aggregation + usage side-channel extraction
│  │  │  │  ├─ account_store/   Account storage (global priority, rate-limit cooldown, per-vendor add and import)
│  │  │  │  ├─ models/          Model catalog internals (built-in WorkBuddy list + /v3/config refresh)
│  │  │  │  ├─ model_rules.rs   Model management rules (disable / hide / mapping alias)
│  │  │  │  ├─ api_keys.rs      Gateway key list (multiple keys, any enabled one passes)
│  │  │  │  ├─ auth.rs / auth_http.rs / login.rs   Sessions, egress transport, headless login
│  │  │  │  │                    (login/ holds the vendor-specific flows: CatPaw's
│  │  │  │  │                    loopback callback, Qoder's device authorization)
│  │  │  │  ├─ routing.rs / billing/   Account routing (global priority + rate-limit cooldown) / points check-in ops
│  │  │  │  ├─ proxies.rs / clash.rs / egress.rs   Egress proxies and a per-exit cached Client
│  │  │  │  ├─ desensitize/     Redaction engine and word lists (applied per provider + role)
│  │  │  │  ├─ credential_maintenance.rs  Batch refresh of expired / soon-to-expire credentials
│  │  │  │  ├─ usage_query.rs     Balance / points queries (concurrent across accounts + the snapshot taken by the scheduled run)
│  │  │  │  ├─ scheduled_tasks.rs  Interval-based scheduled task registry and dispatch loop (toggle / interval /
│  │  │  │  │                       last result; configured under scheduledTasks in config.json)
│  │  │  │  └─ account_transfer.rs + account_transfer/ / auto_checkin.rs / update/
│  │  │  │                       Import/export (with identity normalization) / scheduled check-in / software updates
│  │  │  └─ api/                 Per-route handlers (health/session/accounts/accounts_usage/
│  │  │                          chat/models/keys/model_manage/stats/logs/billing/
│  │  │                          desensitize/auto-checkin/scheduled-tasks/update/…)
│  │  ├─ lib.rs                  App entry point (config directory migration → settings → tray → main window → start backend)
│  │  ├─ backend.rs              In-process server lifecycle
│  │  ├─ legacy_install.rs       Cleanup of the old "current user" install (directory / shortcuts / uninstall entry / autostart; release only)
│  │  ├─ gateway.rs              Shell-side HTTP client for the management API
│  │  ├─ login.rs / commands.rs  Login window and polling, invoke commands exposed to the frontend
│  │  ├─ login_profile.rs        Each web login gets its own temporary WebView2 data directory (deleted when done)
│  │  ├─ bridge.rs               Bridge script injected as window.workbuddyDesktop
│  │  └─ update.rs / settings.rs / state.rs / tray.rs
│  ├─ ui/                        Frontend (plain HTML/CSS/JS, no framework)
│  └─ src-tauri/tauri.conf.json  Bundle configuration (NSIS)
├─ build/make-icon.mjs           Generates the app icon source image
└─ package.json                  Build script entry points (tauri:dev / tauri:build / build:icon)
```

---

## Development & Build

### Requirements

- Rust >= 1.77 and the Tauri 2 toolchain (to compile the desktop app itself; Windows also needs the WebView2 runtime)
- Node.js >= 18.17 (only to run `npm run tauri:*` and frontend build scripts such as `build/make-icon.mjs`; the desktop app does not depend on Node at runtime and ships no Node artifacts)

### Common scripts

```bash
npm run tauri:install      # Install desktop dependencies (same as npm --prefix desktop-tauri install)
npm run tauri:dev          # Launch the desktop app in dev mode (with hot reload)
npm run tauri:build        # Build the desktop installer

npm run build:icon         # Generate the icon source image (run after changing the icon design, then run tauri icon)
```

The root project has no runtime dependencies; `package.json` only provides the shortcut script entry points above. The build artifact is `target/release/bundle/nsis/Agent2API_<version>_x64-setup.exe` (currently about 3.0 MB; `src-tauri/.cargo/config.toml` points cargo's `target-dir` at the project root's `target/`).

---

## Usage Notice

### For learning and discussion only

This project is a hands-on exercise in HTTP reverse proxying, SSE streaming passthrough, multi-upstream protocol adaptation and desktop packaging (Tauri), and is **for personal learning and research only**. It is not an official product and has no affiliation with, endorsement from or sponsorship by Tencent and WorkBuddy / CodeBuddy, Meituan and CatPaw, SenseTime and Raccoon, Zhipu and AutoClaw / autoglm, or Alibaba and Qoder.

### About the reverse-proxy behaviour

What this project implements is a local reverse proxy: it reuses your own accounts' login state on your own machine and forwards requests to the official upstream gateways. It does not crack or bypass any payment or permission check — the quota it uses always comes from what your own account already has. That said, forwarding in the shape of a non-official client **may violate the upstream services' user agreements or terms of use**; whether to use it, and every consequence that follows (including but not limited to rate limiting, risk-control flags, account suspension or bans), is borne by the user.

### Prohibited uses

Do not use this project for any commercial purpose, for redistributing it for profit, for bulk account operation, for circumventing upstream billing or quota limits, or for any activity that violates local laws and regulations. To call these model services in production or commercial settings, use the official channels and official APIs.

### Credentials and data risk

This project stores account credentials (`accessToken` / `refreshToken`, etc.) as **plain text** in the local configuration directory (by default `~/.agent2api/`), and files produced by the export feature contain plain-text credentials as well. Keep them safe: never commit them to a public repository, upload them to cloud storage or share them with others. Losses caused by leaked credentials are borne by the user.

### No warranty and rights notice

This project is provided "as is"; the author makes no promise about its availability, stability, security or fitness for a particular purpose. Upstream interfaces may change at any time and the project may stop working and go unmaintained at any time. The full terms are in [LICENSE](./LICENSE). The interface shapes, protocol fields and other information in this project come from observing and organizing publicly visible network traffic of the official clients; all related trademarks and services belong to their respective owners. If a rights holder believes this project is inappropriate, please contact the author and it will be adjusted or removed promptly.

---

## License

This project is released under the [MIT License](./LICENSE); you may use, modify and distribute it freely as long as the copyright notice is retained.

One caveat: the LICENSE file carries a **Usage Notice** after the MIT text, whose clause 3 **adds restrictions on top of** MIT (no commercial use, no reselling redistributions, no bulk account operation). This project is therefore **not** pure MIT — **the MIT terms and the Usage Notice together form the complete license**, and where the two reach different conclusions on the same act, the stricter one governs. That is also why `Cargo.toml` points `license-file` at the LICENSE file instead of declaring the SPDX identifier `"MIT"`.
