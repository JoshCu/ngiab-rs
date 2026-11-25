# ngiab-rs

Rust (edition 2024) ratatui TUI and CLI that runs bmi-driver and then rs-route on a NextGen
(NGIAB) data folder, on a `local`, `docker` or `podman` backend, and plots the results. User-facing
docs are in `README.md`. How an agent should *use* the tool is in `SKILL.md`. The plan and its remaining
work are in `tasks.md`; its **Decisions** and **Contracts** sections are binding. Keep its checkboxes
accurate when you finish an item.

## Commands

```sh
cargo build                       # dev build; needs system libnetcdf (or add --features static)
cargo test                        # unit tests; no network (net::tests::network_smoke is #[ignore])
cargo clippy --all-targets        # must stay warning-free
cargo fmt                         # tree is rustfmt-clean (default config)
cargo xwin check --target x86_64-pc-windows-msvc --all-targets   # Windows type check (cargo-xwin)
cargo build --release --features static   # static netcdf/hdf5: cmake >= 3.26, set CMAKE_POLICY_VERSION_MINIMUM=3.5 with cmake 4
```

Run all of fmt, clippy, test and the xwin check before committing.

## Layout (`src/`)

- `main.rs`: dispatch, `run_main(RunArgs)`, `run_tui`, and the single `suspended()` helper
  (suspend TUI, run a closure, resume). The shell, the installer and `models update` all go through it.
- `cli.rs`: clap `Cli { cmd: Option<Cmd>, run: RunArgs }` with `args_conflicts_with_subcommands`. Every
  run flag goes on `RunArgs`. Also `cli::confirm`, the shared y/N prompt.
- `app.rs` / `ui.rs`: TUI state machine (Home / Run / Eval screens, popups) and drawing.
- `backend.rs`: `BackendKind`, `Detected::detect()` (the docker/podman probes are slow, so the
  `self`/`models` commands must not call it), `Availability`, `Launcher`, `CommandSpec { env }`,
  `in_container()` and `LOCAL_SUPPORTED`.
- `plan.rs`: steps (`Stage`, bmi-driver, rs-route) and tool args. `auto.rs` is the `--auto` runner.
  `pty.rs` runs a step in a pseudo-terminal.
- `preflight.rs`: checks shown on Home and in `--auto`, and output cleanup (`safe_to_clean`).
- `paths.rs`: `PathMap`. Container paths are POSIX **strings**, matched on component boundaries.
  On local, `/data` and `/ngen/ngen/data` map to the data folder, and the bundle prefixes
  (`/dmod`, `/ngen/ngen/extern`) map to `~/.ngiab/container`.
- `stage.rs`: local runs write rewritten configs to `<data>/.ngiab-rs/stage/` and pass
  `--config`. Also handles write-back of bmi-driver's mapping prompt into the user's realization, with a `.bak`.
- `models.rs`: `Manifest` and `models update|revert|status`, using a lock, crash recovery and the
  `container`/`.new`/`.bak`/`.tmp` swaps.
- `selfupdate.rs`: `self update`, built on `self-replace`. `installer.rs`: `backend list|install|record`
  and `offer()`, the logic that decides which setup to offer.
- `net.rs`: the **only** HTTP client (ureq 3 + rustls), with GitHub release lookup, `.part` downloads,
  SHA256SUMS checks and safe tar/zip extraction. `home.rs`: `~/.ngiab` layout (`NGIAB_HOME`), `InstallInfo`.
- `config.rs`: realization parsing and the persisted UI state. `eval.rs`: reading NetCDF/CSV outputs for
  the Eval screen.

## Invariants (don't break these)

- Never modify the user's `config/` during a run. The only exception is accepted mapping write-back,
  which goes through `stage::write_back` and keeps a `.bak`. Output cleanup never touches `config/`,
  `forcings/`, `eval/` or `.ngiab-rs/`.
- In `~/.ngiab`, only `container*`, `install.json` and `.lock` belong to us. The rest belongs to
  NGIAB_data_preprocess. `home::preprocessor_file()` is always `~/.ngiab/preprocessor`.
- The default image is pinned to `docker.io/joshcu/ngiab-rs:{CARGO_PKG_VERSION}`. Docker tags have no `v`.
- Windows builds natively but only supports docker/podman. Gate Unix-only code with `cfg(unix)`, and
  Linux/glibc-only code with `cfg(all(target_os="linux", target_env="gnu"))`.
- `--auto` and in-container runs never prompt.
- The model bundle never contains Python. Python BMI and SUMMA are unsupported on every backend.

## Testing against real data

Test data lives in `~/ngiab_preprocess_output/<run>/`, but **only ever use copies** (e.g. in
`/tmp`). Point `NGIAB_HOME` at a temp dir for anything that writes model files. To get a bundle locally:
`scripts/build-model-bundle.sh awiciroh/ngiab:<tag> models-test /tmp/bundle`, then
`NGIAB_HOME=/tmp/h ngiab-rs models update --file /tmp/bundle/ngiab-models-x86_64-unknown-linux-gnu.tar.gz`.
`NGIAB_RS_DOWNLOAD_URL` points the installers and `backend install` at a local server for testing.

## Release / CI

- `.github/workflows/release.yml` runs on `v*` tags. `models.yml` runs on `models-YYYY.MM.DD` tags.
  Both have a `workflow_dispatch` dry run.
- `TOOL_VERSIONS` pins bmi-driver and rs_route. Keep the Dockerfile `ARG` defaults in sync; a unit
  test checks this.
- `ARG NGIAB_BASE` in the Dockerfile pins the upstream image that the bundle and the image are built from.
- `install.sh` (POSIX sh, shellcheck-clean, tested under dash) and `install.ps1` (env vars only, no
  `param()`) are release assets.
- Release steps are in the README under "Releasing".

Commit messages: short imperative subject, scoped where useful (`models: …`, `ci: …`).
