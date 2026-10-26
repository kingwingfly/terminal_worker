# Terminal Worker: agent handoff

This file is deliberately named `AGENT.md` as requested by the owner. Read it with `README.md` before changing the project.

## Scope and ownership

- Repository: `/run/media/louis/Work/rust/terminal_worker`. This is its own Rust/npm workspace.
- The earlier presentation repository is `/run/media/louis/Work/lab_meeting/2026-10-28`; it owns the story, slides, and Bevy game. Do not move its content back here or modify it for ordinary terminal maintenance.
- The terminal is a standalone R2 file manager with a simulated shell. Commands never execute a host shell.
- Keep project-owned interface text, errors, manuals, comments, tests, sample content, and documentation in English. Preserve Unicode path support; non-ASCII test cases are intentional.
- Keep Wrangler configuration in TOML. Worker/crate name is `terminal-worker`, core crate `terminal-worker-core`, UI crate `terminal-worker-ui`, bucket `terminal-worker-files`.
- The owner currently wants local `wrangler dev`, not a cloud deployment. Do not request Cloudflare credentials or deploy without a new instruction.
- Inspect Git status before editing. At this handoff, project files are untracked and no baseline commit has been created. Do not assume changes belong exclusively to you or auto-commit without a request.

## Build and run

See README for prerequisites and commands. The known toolchain uses `worker-build 0.8.5` and `wasm-bindgen-cli 0.2.129`; verify compatibility with Cargo.lock before changing either.

- `npm ci`, `npm run setup`, `npm run build:web`, `npm run dev`.
- `npm run dev` binds 127.0.0.1:8787, the port direct Wrangler takes from `wrangler.toml`; `seed` and the tests default to it too. Inspect listening ports before launching or stopping processes; the owner may run a separate instance.
- Frontend changes require a build. Wrangler watches Worker/core sources. Restart only the relevant local server if newly generated chunks return 404 despite existing on disk.
- Do not build the same Worker output concurrently from multiple Wrangler instances or content projects. Their `.tmp` build directories are shared.
- The build discovers Cargo's actual target directory with cargo metadata; do not assume a repository-local target directory.
- Generated assets are in dist; Worker bundles are in crates/worker/build. Edit source, then rebuild.
- `npm run seed` installs English samples without replacing existing content. It is local-only and accepts TERMINAL_PORT and TERMINAL_PASSWORD.

## Credentials and live data

- `.dev.vars`, `.local-password`, and `.wrangler` are private local state, not source. Never print or commit secrets.
- ADMIN_PASSWORD initializes root once. The everyday root password has already been changed and may differ from `.dev.vars` or `.local-password`. Do not reset it for testing.
- SESSION_SECRET is a password pepper; changing it breaks existing password verification. Restore it with the database.
- Nothing is deployed (2026-10-25). The owner said local state is disposable: `.wrangler` may be deleted during tests, and storage changes need no migration from earlier formats. Still prefer an isolated `--persist-to` directory for test instances, and never print or commit secrets.
- R2 bytes without the Durable Object's tables are not a complete filesystem backup. Do not bypass the initialization marker to repair a missing authority.
- Localization changes the application and bundled examples; existing user-uploaded R2 paths/content remain user data.

## Architecture and invariants

- Core: pure Rust path resolution, the shell grammar (pipelines, xargs items, fd placeholders: `shell.rs`), text filters (`text.rs`), fd matching (`find.rs`), fuzzy completion, ranges, and the permission rules as provided methods of the `Store` trait (`permissions.rs`). `MemStore` implements it for tests; the worker's `Db` over SQLite. New file commands belong there, as validate-then-mutate methods, with MemStore tests.
- Frontend: Leptos tabs (`lib.rs`); each tab is a `shell::Shell` that runs pipelines stage by stage. Server commands go through web/app.js's `accountCommand` (or the batch endpoint, for xargs and fd -x); text filters, xargs, fd -x/-X and `mv`/`cp -i` prompts run in the page. Browser bridge in web/app.js; chunked uploads in web/upload.mjs. CodeMirror loads lazily.
- Previews render in Rust: crates/frontend/src/viewer.rs mounts each into the viewer's content through `window.terminalPreview.show(content, path, kind, version)`, and app.js calls what it returns to release it; the mount happens only if `content.dataset.version` still names that open. app.js keeps the dialog shell, fullscreen, games, and focus handling. Markdown is sanitized with ammonia; maths is substituted after sanitizing.
- The viewer's search (`crates/frontend/src/search.rs`, the `Searchable` wrapper of text, code, diff and Markdown previews) matches Rust regular expressions over the DOM's text nodes and highlights them with the CSS Custom Highlight API, never changing the preview's markup; it searches again when the markup changes (code highlighting arrives later). It shows `#viewer-search` in the viewer bar while mounted, and its Esc must not close the viewer.
- crates/preview builds six web workers (scripts/build.mjs): PDF (hayro), diagram (merman, size profile), and four code groups (arborium). Grammars are C: building needs clang with wasm32 and llvm-ar. Each worker file must stay under Cloudflare's 25 MiB asset limit; split a language group if one grows past it. The upload allowlist (core `kind`) must cover every previewable extension; a frontend test checks the highlighted languages.
- The AUTHORITY Durable Object named filesystem keeps all metadata in SQLite tables (`crates/worker/src/db.rs`). Every request's decisions and writes run in one `db::transact`, a hand-bound `transactionSync`: **never await inside it**. Do async work (reading the body, password hashing, rate limits) before, and re-check in the transaction what it relied on. A reply of 400 or more rolls back unless it is marked `keep()` (cleanups).
- **File bytes never pass through the authority.** The Worker (`files.rs`) asks it to authorize (`/internal/read`, `/internal/upload/begin|chunk`), streams R2 natively (no copying through wasm, `FixedLengthStream` for known lengths), then asks it to commit, which checks again. Downloads therefore never wait on uploads.
- Files have a fixed chunk size (8 MiB for chunked uploads; a single-request PUT is one chunk), so chunk i starts at i × chunk_size; the readable length of an uploading file is its contiguous committed prefix. A new file's node exists while it uploads; an overwrite swaps at completion only if the node still shows the file it replaces.
- Nodes refer to immutable file ids; copies share them. A store frees a file when no node refers to it, by putting its key prefix `chunks/<file>/` in `garbage` in the same transaction; the alarm lists and deletes everything under it (also objects written but never committed) and expires uploads idle for 24 hours. Home moves, mv and cp change metadata only.
- Every chunk write has its own R2 key (`chunks/<file>/<index>-<random>`, from `api::chunk_key`), and a chunk is committed once: a write sent again, late, or after completion is discarded, never overwriting bytes a reader, copy or completed file uses.
- The initialization marker (`system/terminal-authority-v2`) fails closed if the authority's storage disappears.
- Archive jobs (`jobs.rs`, `runner.rs`): zip/unzip/tar go through `/api/command`, which validates them and snapshots their inputs in one transaction, pins the files they read (`job_files`, which garbage collection respects), then wakes an `ArchiveRunner` (one per job, named by its id). The runner works from its alarm in bounded steps, keeps its plan in its own SQLite, and asks the authority for everything through `/internal/job/*`, as the job's actor (re-derived each time; root only if started with sudo). Its container (`crates/archiver`) only reads and writes archives on its disk; bytes between R2 and it are streamed, never copied through wasm.
- Archive output must stay deterministic (pinned mtime, owners from the snapshot, miniz_oxide at a fixed level, fixed gzip header bytes): a resumed `zip`/`tar -c` rebuilds and compares chunk SHA-256s with what is stored, and starts the upload over on any difference. A change to the archiver that changes bytes is fine only because of that check; keep it.
- Extraction follows Linux: members written in place one by one, unsafe ones (`..`, links, devices, duplicates) skipped with a warning (core `archive::member_path`), a cancelled or interrupted member's upload cancelled so no partial file remains. unzip's replace question is asked once, for all conflicts, before writing (`state = question`).
- Owner/group/other select exactly one class, without bit union or fallback. Root uid 0 retains complete control. Check all ancestor x bits before inspecting protected contents.
- Authorize HEAD, Range, and conditional requests before returning data or 304. Keep private/no-store, Vary Cookie, nosniff, and file-response sandbox/CSP behavior.
- File creation needs parent w+x; overwriting needs file w; deletion needs parent w+x. Recursive operations validate the entire affected tree before mutation.
- Some HTTP error classification currently matches English message fragments. Preserve 401/403/404/409 semantics when editing messages; permission/API tests cover these paths. A future typed-error refactor must preserve behavior.

## Required account semantics

These were explicitly chosen by the owner; do not silently replace them with full Linux semantics.

- guest cannot sudo. Normal sudo callers must belong to **wheel**, not a sudo/sudoers group. Root keeps full control.
- `sudo COMMAND` elevates only that request. The five-minute credential cache is used only for explicit sudo requests, with membership rechecked each time.
- `su [-] USER` switches accounts using the target password; root may switch to an enabled account without it.
- `sudo -u USER` switches the browser account and enters its home. It does not accept a trailing command.
- `exit` and `logout` revoke the session and return directly to guest at /. No nested account stack.
- Permission checks must never implicitly log guest or a normal account in as root.
- `passwd --force` skips only the 12-character minimum. It still requires a nonempty password, at most 512 bytes, and normal identity/authorization checks.
- Account changes revoke old sessions; a self-password change issues a new current session. Locking revokes sessions. Unlocking does not resurrect them.

## UX requirements and fragile areas

- Main text is 20px; terminal gets most of the workspace. Responsive side-by-side/stacked layouts and collapsible file panel are intentional.
- Restrained colors, line icons, highlighted prompts, and ls-style listings are intentional. Terminal scrollbars are hidden, but scrolling must work.
- Completion is fish-like: cycle original fuzzy candidates, reverse with Shift+Tab, browse files with empty input, preserve quoting and Unicode, discard stale asynchronous results.
- Every command has -h / --help and shares web/manual.mjs with man. Help must run before authentication or side effects. Respect -- when interpreting help flags.
- Ctrl+C clears current input/completion/history navigation, and (owner's choice, 2026-10-25) stops the tab's running command between steps: the request in flight finishes, nothing more runs; an upload run by that tab is cancelled; a pending prompt gets no answer.
- Commands follow Linux semantics where this terminal can (owner's choice): mv and cp overwrite by default (-n, -i), multi-path commands go on past a failing path and report it, unzip/tar extract in place. New commands print nothing on success unless -v; existing confirmations ("Created", "Removed", "Updated") stay, as tests rely on them.
- Tabs share one account (one HttpOnly session cookie); each has its own cwd, history, transcript and cancel flag, and only the active tab's input carries `id="command"`. `.busy` checks in app.js must use the active form's.
- `touch` supports multiple files, -c, --, and extensionless names. Never truncate existing content or change its permissions; no file timestamps are maintained.
- `vim` is a floating ordinary editor, not Vim emulation. Preserve drafts on save conflicts, enforce ETag/create-only conditions, and retain explicit discard confirmation.
- **Focus regression:** after a modal closes, Chromium can report the terminal input as active while native text input remains unavailable. Authentication and editor close events share deferred focus restoration that blurs/refocuses when necessary. Do not reintroduce immediate focus calls inside editor cleanup.
- Focus tests must type with page.keyboard and execute a command after closing, without fill(), click(), or Tab. toBeFocused() alone misses the bug. Cover Esc, close buttons, and discard flows; do not steal focus while another dialog is open.
- Scroll after committed DOM output, not only after the echoed command. Preserve the MutationObserver/requestAnimationFrame approach.
- Fullscreen uses the preview surface inside the dialog. Preserve error feedback and fullscreen teardown.
- Games are optional trusted static assets selected by registered manifests, not arbitrary uploaded executable code. Preserve Object.hasOwn registry validation and audio cleanup before iframe removal.

## Validation and handoff

Use targeted checks for small changes; permission/auth changes need API integration coverage.

```bash
npm test
cargo fmt --all --check
cargo check --workspace --target wasm32-unknown-unknown
npm run build:web
```

For the full browser suite, start a local Wrangler instance with a fresh temporary --persist-to directory and an unused port, seed it with TERMINAL_PORT, then run with TERMINAL_TEST_URL. Tests read the bootstrap password from .dev.vars. The README gives an example using 8790. Tests that sign in must use their own `CF-Connecting-IP` (as the existing ones do), or they use up the shared login rate limit (12 per minute) and later tests fail at sign-in.

Playwright needs its browser installed. On this Linux host, DISPLAY is removed and Chromium uses headless/SwiftShader launch options; preserve that setup. tests/editor-focus.spec.js mocks storage and can run without login. tests/permissions.spec.js checks authorization independently of UI behavior.

After changing English text, check source and filenames for Han characters and rebuild generated assets. Do not scan secret values into logs or rewrite third-party dependencies. Preserve non-ASCII Unicode regression coverage.

Report what changed, what was tested, and any real limitation. Do not claim deployment, a commit, or a successful full suite unless it actually occurred.

## Verification at this handoff (2026-10-25)

- Frontend build, cargo fmt --check, and workspace wasm32 cargo check passed.
- All 16 unit tests passed (14 Rust core tests and 2 JavaScript faststart tests).
- All 27 browser/API tests passed in Chromium against freshly seeded, isolated local state on port 8790.
- The 49 first-party source files, their filenames, and generated application JavaScript/HTML/config were scanned; no Han text remains. Greek/accented path cases retain Unicode coverage.
- Seed filenames are now welcome.md, readme.txt, and demo.slides.md; generated sample audio is media/signal.wav. Existing uploads in everyday local state were not renamed or rewritten.
- No remote deployment or Git commit was performed. The temporary validation server/state were removed after testing; the regular local dev server then used port 8788 (now 8787).

## Rust previews (2026-10-25)

- Previews moved from JavaScript (marked, DOMPurify, reveal.js, PDF.js) to Rust: `crates/frontend/src/{preview,viewer,workers}.rs` and the `crates/preview` web workers. Those four npm packages and `web/pdf.mjs` are gone; CodeMirror stays for `vim`.
- The core's `kind` now knows every previewable extension and an `archive` kind, so these files can be uploaded; it takes the extension from the file name, so extensionless names like `Makefile` match.
- Verified: cargo fmt --check, workspace wasm32 cargo check, 15 core + 7 frontend + 15 preview-worker Rust tests, 2 JavaScript tests, all 28 browser/API tests on an isolated instance (port 8795, fresh state), and the fullscreen config in Chromium and Firefox (6 tests).
- The six worker files are 5–21 MB each (code_apps is the largest at 20.9 MB), under the 25 MiB asset limit. No deployment or commit was made.

## Storage v2, uploads and the shell (2026-10-25)

- Approved plan: `/home/ai/.claude/plans/if-i-plan-to-hashed-patterson.md` (storage in the Durable Object's SQLite, chunked resumable uploads, tabs and pipes, then archives in a Cloudflare Container). Phases 1–3 are done; phase 4 (archives) is described below.
- Verified on an isolated instance (port 8797, fresh state): cargo fmt --check, workspace wasm32 check, 30 core + 2 JavaScript unit tests, and all 35 browser/API tests, including the new storage, uploads and shell suites. No deployment or commit.
- Local Cloudflare Containers work with rootless Podman (the owner added `ai:165536:65536` to /etc/subuid and /etc/subgid): `npm run dev` (`scripts/dev.mjs`) starts `podman system service` with `--cgroup-manager=cgroupfs` (no systemd user session here), sets `WRANGLER_DOCKER_BIN` to `scripts/podman-docker.sh` (drops `--load`/`--provenance`, passes the piped Dockerfile through a file) and `WRANGLER_DOCKER_HOST`, and names the egress sidecar with its registry (`MINIFLARE_CONTAINER_EGRESS_IMAGE=docker.io/...`). Containers are named `workerd-terminal-worker-ArchiveRunner-<id>` (plus a `-proxy` sidecar).

## Archives (2026-10-25)

- Phase 4 is done: zip, unzip, tar and jobs, run by `ArchiveRunner` with the `crates/archiver` container. The cargo target directory here is `/home/ai/target`, not the repository's.
- Verified on an isolated instance (port 8793; another project's wrangler took 8797): cargo fmt --check, workspace wasm32 check, 33 core + 4 archiver + 8 frontend + 2 JavaScript unit tests, and all 38 browser/API tests, including the new archive suite (round trips, questions, -o/-n/-k, hostile members, Ctrl+C, a closed tab's job). By hand: killing the container in the middle of a 36 MB zip, the job resumed and the archive was intact, contents equal.
- The owner chose 8787 as the one local port: `npm run dev`, `wrangler.toml`, `seed`, the Playwright configs and the tests now agree. No deployment.
