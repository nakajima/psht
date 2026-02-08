# psht

A minimal PaaS that deploys apps via `git push` into Incus containers. Users push code over SSH, psht detects the app type, creates a container, and starts the app.

## Architecture

```
main.rs     â Entrypoint: parses SSH_ORIGINAL_COMMAND or argv into Command enum, dispatches
git.rs      â Bare repo management, post-receive hook installation, exec into git-receive-pack/git-upload-pack
commands.rs â Deploy orchestration (checkout â detect â container â proxy â start), ps, logs, stop
detect.rs   â App type detection by marker files (Cargo.toml, package.json, etc.), Procfile parsing
container.rs â Incus container operations via IncusCommand builder
```

## Conventions

- **Errors:** `Result<T, String>` everywhere. No custom error types.
- **No `.unwrap()` in production code.** Tests may unwrap.
- **Testability:** Functions that need isolation take explicit params (e.g. `ensure_repo_in(app, base)` wraps `ensure_repo(app)` which reads the env). The `_in`/`_at` variant is what tests call.
- **Container naming:** `psht-{appname}` (see `container_name()`).
- **Port allocation:** Deterministic hash of app name, range 3001â4000 (`allocate_port()`).
- **CLI builder:** `IncusCommand` uses a builder pattern (`.arg()`, `.args()`, `.run()`, `.output()`). Tests verify built args without executing.

## Supported app types

Detected by marker file in priority order: Rust (`Cargo.toml`), Node (`package.json`), Python (`requirements.txt` / `Pipfile`), Go (`go.mod`), Static (`index.html`). A `Procfile` with a `web:` line overrides the default start command.

## Testing

- Tests are inline in each module (`#[cfg(test)] mod tests`).
- Use `tempfile::tempdir()` for filesystem isolation.
- Never add `#[ignore]` to tests.
- Write a failing test before fixing a bug or adding behavior.
- `cargo test` must pass before any task is considered done.

## Build commands

```
cargo test     # Run all tests
cargo check    # Type-check without building
```

## Adding things

- **New app type:** Add variant to `AppType` in `detect.rs`, add marker file to the `markers` slice, implement `default_start_command()` and `install_command()`, add a detection test.
- **New SSH command:** Add variant to `Command` enum in `main.rs`, handle in `parse_ssh_command()` and `parse_args()`, dispatch in `run()`, implement in `commands.rs`.
- **New container operation:** Add a function in `container.rs` using the `incus()` builder, add a test that verifies the built command args.
