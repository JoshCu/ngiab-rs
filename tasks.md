# ngiab-rs task plan

This plan merges the old `install+ci.md` (installers, cross-platform releases, self-update, model management, local-backend path glue) and `notes.md` (TUI and evaluation improvements). Both originals are reproduced verbatim in the appendix.

**Start here in a new session:**

1. Read **Decisions** and **Contracts**. They are the single source of truth. Every section below has been written to match them; if anything disagrees, the contracts win.
2. Follow **Implementation order**. Each step names the section with the detailed checkbox tasks:
   - **A** CI/release
   - **B** Install/update
   - **C** Local backend
   - **D** TUI/eval
3. File:line references are against commit `4f40a51` ("initial prototype"). Re-check them once the code moves.

---

## Decisions (made by the user)

| Topic | Decision |
| --- | --- |
| CI | GitHub Actions with hand-rolled workflows (not cargo-dist). `release.yml` runs on `v*` tags. `models.yml` runs on `models-*` tags. Both also have a `workflow_dispatch` `dry_run`. |
| Release targets | `x86_64-unknown-linux-gnu` (built in manylinux_2_28), `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-pc-windows-msvc`. **No** Linux aarch64 binaries. Every build leg runs `cargo test --locked`. There is no PR-check workflow; it's optional later. |
| Windows | Native binary, docker/podman backends only. Unix-only code goes behind `cfg(unix)`. `install.ps1` follows uv's style (`irm … \| iex`). |
| macOS | docker/podman only, because the model libraries are Linux ELF. Unsigned binaries are fine for now; install.sh clears the quarantine xattr. |
| Local backend | Linux x86_64 with glibc ≥ 2.34 only. Older distros (Ubuntu 20.04, Debian 11, RHEL 8) use docker/podman, which is accepted. Model files come from a **CI-published tarball**, so no container runtime is needed. |
| Model bundle scope | **Strict.** It contains `/dmod/datasets`, `/dmod/shared_libs` (plus bundled libgfortran/libquadmath) and the **Rust LSTM weights only** (`/ngen/ngen/extern/lstm/trained_neuralhydrology_models`). It never contains `.venv`, `__pycache__`, `site-packages`, `*.py` or any Python BMI; CI fails if any appear. Python BMI models and SUMMA are unsupported on every backend. |
| Model release cadence | **Separate tags `models-YYYY.MM.DD`** (plus `.N` for a second release on the same day). They are date-versioned, not semver, and never marked as the GitHub "latest" release. |
| Python in the image | **Dropped.** Remove `python3.11` from the Dockerfile runtime stage, and prune its `extern/lstm` copy to the weights, so the image matches the bundle. |
| Docker image | `joshcu/ngiab-rs` for amd64 and arm64 on Docker Hub, tagged `X.Y.Z`, `X.Y` and `latest`. `latest` is not moved for prereleases. Docker tags have no `v` prefix. |
| Default image tag | Pinned to `docker.io/joshcu/ngiab-rs:{CARGO_PKG_VERSION}`. install.sh pulls the matching tag, and `self update` prints a hint to re-pull. |
| Staging dir (local) | `<data>/.ngiab-rs/stage/`, wiped on every run. Output cleanup never touches it. |
| `/data` on local | **Always** maps to the folder the user picked (e.g. `~/ngiab_preprocess_output/7_year_provo`). `/ngen/ngen/data` maps the same way. |
| Mapping write-back | On local runs, bmi-driver's "add these mappings?" result is applied to the user's real `config/realization.json`, reverse-mapped to container paths, after saving `realization.json.bak`. This matches docker behaviour. |
| "Light up the bottom menu" | Footer key hints. After a run, the next actions (`e`, `q`, `r`, `s`) are highlighted. While a run is active they are dimmed. |
| Browse | A native OS folder dialog (`rfd`, xdg-portal backend on Linux, on by default) whenever a desktop session exists. Otherwise, and on any failure, an **in-TUI directory browser**. |

## Contracts (single source of truth)

- **Repo slugs.** GitHub `JoshCu/ngiab-rs` and Docker Hub `joshcu/ngiab-rs`. There is no git remote yet, so confirm the GitHub slug when one is created. In code: `const REPO`, overridable with `NGIAB_RS_REPO`.
- **Binary release assets** (`v*` tags):
  - Archives: `ngiab-rs-<triple>.tar.gz`, or `.zip` for Windows. There is no version in the name, so `releases/latest/download/…` always works.
  - Archive layout: the root holds `ngiab-rs[.exe]`, `README.md` and `LICENSE`, with no wrapping folder.
  - Also attached: `install.sh` and `install.ps1`.
  - Checksums: one `SHA256SUMS` covering every asset, generated with `sha256sum *`.
- **Model release assets** (`models-YYYY.MM.DD` tags, `make_latest: false`):
  - `ngiab-models-x86_64-unknown-linux-gnu.tar.gz`
  - `ngiab-models-x86_64-unknown-linux-gnu.json`: the manifest, plus `tar_sha256`.
  - `SHA256SUMS`

  To find the newest models release, list releases through the API and take the highest `models-*` tag.
- **Dockerfile:**
  - Declare `ARG NGIAB_BASE=awiciroh/ngiab:<tag>` before the first `FROM`.
  - Add `FROM ${NGIAB_BASE} AS ngiab`.
  - Change the build stage to `FROM ngiab AS build`.
  - Use `COPY --from=ngiab …` for every copy.
  - Install no Python, and copy only the LSTM weights.
  - Read tool versions from `TOOL_VERSIONS`.
  - `models.yml` reads `NGIAB_BASE` from the Dockerfile.
- **Making the bundle relocatable** (in the `models.yml` job):
  - Run `patchelf --set-rpath '$ORIGIN'` on every real `.so`.
  - Bundle libgfortran and libquadmath, but not libc or libstdc++.
  - Make `run_dir`, `train_dir` and `img_log_dir` in the LSTM `config.yml` files relative.
  - **Fail the job** on:
    - any Python artefact
    - any `/ngen/` or `/dmod/` string in a text file
    - a dangling symlink
    - a tarball over 1 GiB (expected size is 30–80 MB)
  - **Report only** (job summary):
    - `strings` hits for those paths inside `.so` files
    - `ldd` not-found libraries
    - the highest required glibc, which becomes `min_glibc`
- **Manifest** (`models::Manifest`). It sits in the tarball as `manifest.json` and is also attached as the sibling `.json`. `models update` adds `tar_sha256`, `installed_at` and `installed_from`.
  ```json
  {"schema":1,"release":"models-2026.11.23","source_image":"awiciroh/ngiab:<tag>","source_digest":"sha256:…",
   "created":"<UTC ISO8601>","target":"x86_64-unknown-linux-gnu","min_glibc":"2.34",
   "prefixes":["/dmod","/ngen/ngen/extern"],"libs":["libcfebmi.so…"],"tar_sha256":"…"}
  ```
- **Filesystem** (`src/home.rs`):
  - `ngiab_home()` is `$NGIAB_HOME`, falling back to `~/.ngiab`. `container_dir()` is `ngiab_home()/container`.
  - The container path `/X` maps to `container_dir()/X`.
  - `preprocessor_file()` is always `~/.ngiab/preprocessor`, regardless of `NGIAB_HOME`.
  - **`~/.ngiab` is shared with NGIAB_data_preprocess.** ngiab-rs only touches these entries in it: `container/`, `container.new/`, `container.tmp/`, `container.bak/`, `install.json` and `.lock`. Uninstall instructions must name only those.
  - UI state stays in `dirs::config_dir()/ngiab-rs/state.json`.
- **Install dirs:** `~/.local/bin` on Unix and `%USERPROFILE%\.local\bin` on Windows.
- **Installer interface:**
  - Flags: `--backends`, `--skip-binary` (env `NGIAB_RS_SKIP_BINARY`), `-y`, `--version`, `--install-dir` and `--no-modify-path`, each with an `NGIAB_RS_*` env var.
  - install.sh reads prompts from `/dev/tty` and never hangs without a tty.
  - install.ps1 takes **environment variables only**. It has no `param()` block, and `-File` is used only for the pinned re-run.
- **Installer re-run from the app** (defined once, in B.7):
  - Download the version-pinned `releases/download/v{ver}/install.{sh,ps1}`, verify it against `SHA256SUMS`, and run it with `--skip-binary --backends <k>`.
  - State: `want_installer: Option<Vec<BackendKind>>` and `declined_install`.
  - When a backend is `NotRunning`, show a hint to start it instead of offering a reinstall.
  - When only the bundle is missing, offer `models update`.
- **Availability** (defined in C.7):
  - `enum Availability { Available, NotInstalled(String), NotRunning(String), Unsupported(String) }`.
  - Local is `Available` only when bmi-driver, rs-route and a compatible bundle are all present.
  - Call sites: `main.rs:61`, `app.rs:253`, `backend.rs:103` and `ui.rs:191`.
- **CLI** (B.2):
  - Subcommands `self`, `models`, `backend` and `run`, using `args_conflicts_with_subcommands`, so `ngiab-rs DATA_DIR` keeps working.
  - Every run flag, including new ones, goes on `RunArgs`.
- **HTTP** (B.3): a single `src/net.rs`.
  - Crate: `ureq = { version = "3", default-features = false, features = ["rustls","json","gzip"] }`.
  - Functions: `agent()`, `download(url, dest, progress)` (writes `.part`, then renames), `github_release()` and `github_releases()`.
  - Used by `self update`, `models update` and USGS.
- **Build:**
  - `build.rs` emits `NGIAB_TARGET`.
  - Add a `LICENSE` file.
  - `TOOL_VERSIONS` pins `bmi-driver` and `rs_route` for both the Dockerfile and install.sh, installed with `cargo install --version X --locked`.
  - A local build needs only `cc`, `cmake` and `make`. _(P1 finding: rs_route 0.3.1's build.rs also compiles a Fortran kernel with `gfortran`, so building rs_route needs gfortran; the Dockerfile build stage installs gcc-gfortran. install.sh (B.5) must account for this.)_
- **Self-update** uses the `self-replace` crate. On Windows, stale `*.old` files are cleaned up at startup.
- **TUI plumbing:** one "suspend TUI → run closure → resume" helper in `main.rs`, which also toggles mouse capture. The shell, the installer and the `rfd` dialog all use it.
- **Eval files:**
  - USGS cache: `<data>/eval/ngiab-rs/usgs/`.
  - Metrics export: `<data>/eval/ngiab-rs/metrics.csv`.
  - The Python tool's `eval/` contents are read only, never overwritten.
  - Output cleanup never touches `eval/` or `.ngiab-rs/`.

## Implementation order

1. **Foundation PR** (A.1, C.6, part of C.4, B.1):
   - Windows `cfg(unix)` port
   - `build.rs`, `LICENSE`, `TOOL_VERSIONS`
   - Dockerfile: `ngiab` stage, pinned base, no Python, LSTM weights only, pinned tool versions
   - Default image pinned to the crate version
   - `CommandSpec.env`
   - `home.rs`
2. **CLI restructure plus `net.rs`** (B.2, and the helpers from B.3). This must land before any new flags are added.
3. **Workflows** (A.2–A.6):
   - Prove `release.yml` and `models.yml` with `workflow_dispatch` dry runs.
   - Then tag `models-YYYY.MM.DD` and `v0.2.0-rc1` (a prerelease).
4. **Local backend** (C.1 → C.2 → C.3 → C.4 → C.5 → C.7). Validate it against the rc models release.
5. **`models update|revert|status` and `self update`** (B.3, B.4). Test them between rc1 and rc2.
6. **Installers, installer hook and README** (B.5–B.8).
7. **TUI work** (D). It can run in parallel with steps 3–6 once step 2 has landed:
   1. D.0
   2. D.1, D.2, D.3
   3. D.4 and D.5
   4. D.6, then D.7
   5. D.8, then D.9
8. Tag **`v0.2.0`**.

**Merge hotspots.** Give each a single owner, or do the work in sequence:
- `app.rs`: `menu_items`, `request_run`, `refresh_preflight`, `cycle_backend`, `complete_path`, `on_run_key`, `on_eval_key`
- `main.rs::run_tui`
- `ui.rs`: `draw_footer` and the backend list at `ui.rs:191`

## Remaining open questions
These are minor and don't block anything. Defaults are given in brackets.

- **Picker scan depth.** [children plus one level below the base folder] (D)
- **Observation resampling.** [hourly mean of instantaneous values over (t−1h, t]] (D.8)
- **KGE variant.** [2009, which matches the existing Python tool] (D.9)
- **NWM retrospective overlay** from `eval/nwm_cache`: in scope? [no] (D)
- ~~**Initial `NGIAB_BASE` tag**~~ Resolved in P1: pinned `awiciroh/ngiab:v2.1.0`, the highest `vX.Y.Z` tag on Docker Hub on 2026-11-23 (pushed 2026-10-28, amd64+arm64, same push as `:latest`).
- **Repo slug** once the remote exists.
- **Docker `X.Y` tag on prereleases.** [not moved: `release.yml` tags a prerelease only `X.Y.Z`, so `v0.2.0-rc1` never shadows a stable `0.2`; for `0.2.0-rc.1` `${VERSION%.*}` would not even give `0.2`] A.4 literally says `X.Y` always; confirm the stricter behaviour. (A.4)
- **Test command on the build legs.** [`cargo test --release --locked --features static --target T`] The Decisions say `cargo test --locked`. Without `static` the netcdf crate needs a system libnetcdf that the runners don't have (even `cargo check` runs its build script), so no leg tests the default feature set; that is left to developers or an optional `ci.yml` (A.8). Confirm. (A.3)

---

## A. CI, releases, Docker image and model bundle (GitHub Actions)

There is no `.github/` directory and no git remote yet. Slugs: GitHub `JoshCu/ngiab-rs`, Docker Hub `joshcu/ngiab-rs`. These already match `src/main.rs:42` and `src/backend.rs:407`.

There are two hand-written workflows, one per release stream:

| Workflow | Trigger | Produces |
| --- | --- | --- |
| `.github/workflows/release.yml` | `v*` tag push, or `workflow_dispatch` with `dry_run` | binaries, `install.sh`/`install.ps1`, `SHA256SUMS`, Docker image `:X.Y.Z` (+ `:latest`) |
| `.github/workflows/models.yml` | `models-*` tag push (`models-YYYY.MM.DD`), or `workflow_dispatch` with `dry_run` | `ngiab-models-x86_64-unknown-linux-gnu.tar.gz` + `.json` manifest + `SHA256SUMS` |

There is no PR-check workflow. It is optional (A.8).

### A.1 Prerequisites in the code (before the first tag)
- [x] Do the Windows `cfg(unix)` port (C.6). The Windows leg cannot compile without it. _(`cargo xwin check --target x86_64-pc-windows-msvc --all-targets` and `cargo check --target x86_64-pc-windows-gnu --all-targets` pass)_
- [x] Add `build.rs`, which emits `cargo:rustc-env=NGIAB_TARGET={TARGET}`. `self update` uses `env!("NGIAB_TARGET")` to pick its asset.
- [x] Add a `LICENSE` file. It goes into every binary archive. _(MIT, Copyright (c) 2026 Alabama Water Institute)_
- [x] Pin the default image to `docker.io/joshcu/ngiab-rs:{CARGO_PKG_VERSION}` (`concat!("docker.io/joshcu/ngiab-rs:", env!("CARGO_PKG_VERSION"))`). _(`backend::DEFAULT_IMAGE`)_
  - Change `src/main.rs:42` and `src/backend.rs:407`.
  - Update the test at `src/backend.rs:435`.
- [x] Dockerfile changes: _(pinned `awiciroh/ngiab:v2.1.0`. Deviations: the runtime stage is `rockylinux:9.1-minimal` because the full `rockylinux:9.1` ships python3 for dnf; the build stage needs `gcc-gfortran` for rs_route. Tool versions are `ARG`s whose defaults a unit test checks against TOOL_VERSIONS.)_
  - **Pin the base.** Put `ARG NGIAB_BASE=awiciroh/ngiab:<tag>` before the first `FROM`, then add `FROM ${NGIAB_BASE} AS ngiab`. The build stage becomes `FROM ngiab AS build`, and every runtime copy uses `COPY --from=ngiab …`. Build args are not expanded inside `COPY --from`, which is why the named stage is needed.
  - **Drop Python** (user decision). Remove `python3.11` from the runtime `dnf install` and keep `libgfortran attr`.
  - **Prune the LSTM copy to the weights.** Use `COPY --from=ngiab /ngen/ngen/extern/lstm/trained_neuralhydrology_models /ngen/ngen/extern/lstm/trained_neuralhydrology_models`.
  - **Pin the tool versions.** Read them from `TOOL_VERSIONS` via `--build-arg`, or `COPY TOOL_VERSIONS` and `source` it. Then `cargo install bmi-driver --version $BMI_DRIVER_VERSION --locked --features static`, and the same for `rs_route`.
- [x] Add a `TOOL_VERSIONS` file (shell-sourceable `KEY=value`) at the repo root, e.g. `BMI_DRIVER_VERSION=0.2.1` and `RS_ROUTE_VERSION=…`. The Dockerfile and install.sh both read it, and CI passes it as build args. _(bmi-driver 0.2.1, rs_route 0.3.1: the newest on crates.io)_
- [ ] **Acceptance:** _(partial. Done: `cargo xwin check --target x86_64-pc-windows-msvc --all-targets` passes (without `static`); `cargo build --release --locked --features static` passes on Linux (inside the docker build); `docker build .` gives a 177 MB image with no python3 that ran a 31-catchment Rust LSTM realization plus rs-route via `ngiab-rs /data --auto`. Not done: macOS needs a Mac/CI. A Windows `--features static` check could not run here: under cargo-xwin, hdf5-metno-src needs `ninja` (not installed); with mingw, hdf5 built but netcdf-src needs `x86_64-w64-mingw32-g++` (not installed). That is left to the Windows CI leg.)_
  - `cargo check --target x86_64-pc-windows-msvc` passes.
  - `cargo build --release --features static` passes on Linux and macOS.
  - `docker build .` still produces a working image: an LSTM realization still runs in it, and `python3` is absent.

### A.2 Versioning and tagging
- [ ] **Binary releases.** `Cargo.toml` `version` is the source of truth. To release: _(not done: README "Releasing" section is a later phase; the steps are in the release.yml header and the proposed text is in the CI phase summary)_
  1. Bump the version.
  2. Run `cargo update -p ngiab-rs --offline`.
  3. Commit as `release vX.Y.Z`.
  4. Run `git tag vX.Y.Z && git push origin main vX.Y.Z`.

  Document these steps in a README "Releasing" section.
- [x] **Model releases** (user decision). Use separate tags `models-YYYY.MM.DD`, e.g. `models-2026.11.23`. Add a numeric suffix for a second release on the same day (`models-2026.11.23.1`). They are date-versioned, not semver. _(models.yml validates the tag format)_
  - To cut one: bump `NGIAB_BASE` in the Dockerfile (if needed), commit, then `git tag models-$(date +%Y.%m.%d) && git push origin models-…`.
  - The models workflow reads `NGIAB_BASE` from the Dockerfile at the tagged commit.
- [x] `release.yml` first job, `check-version`:
  - Fail if `${GITHUB_REF_NAME#v}` does not equal `cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version'`.
  - Output `version` and `prerelease` (true when the tag contains `-`).
- [ ] **"Latest" must always point at the newest binary release.** Installers and `self update` rely on `releases/latest/download/…`. _(configured in both workflows; not done: confirm on the first real release)_
  - `release.yml` creates stable releases with `make_latest: true` and prereleases with `prerelease: true`.
  - `models.yml` always uses `make_latest: false`.
- [x] Both workflows:
  - Have `workflow_dispatch` with a `dry_run` input. A dry run builds everything and uploads workflow artifacts, but creates no release and pushes nothing.
  - Set `permissions: contents: write` and `concurrency: ${{ github.workflow }}-${{ github.ref }}`.

### A.3 `release.yml`: binary build matrix (`build` job)
| target | runner | archive |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04`, job `container: quay.io/pypa/manylinux_2_28_x86_64` (glibc 2.28) | `.tar.gz` |
| `aarch64-apple-darwin` | `macos-14` | `.tar.gz` |
| `x86_64-apple-darwin` | `macos-15-intel` (or `macos-14` with `--target x86_64-apple-darwin` as a fallback) | `.tar.gz` |
| `x86_64-pc-windows-msvc` | `windows-2022` | `.zip` |

Why manylinux: a binary built on `ubuntu-24.04` needs glibc 2.39 and would not run on Rocky/RHEL 9 (glibc 2.34), which is typical on HPC. Inside the container, install rustup, `dnf install -y cmake` if it's missing, and check that `zip`/`tar`/`sha256sum` exist.

Each leg:
- [x] `actions/checkout@v4`, then `dtolnay/rust-toolchain@stable` with `targets: ${{ matrix.target }}`, then `Swatinem/rust-cache@v2` with `key: ${{ matrix.target }}`.
  - The cache matters: static HDF5, netCDF-C and SQLite (`rusqlite` `bundled`) build from source every time. Expect 20–40 minutes for the first run.
- [x] Set `env: CMAKE_POLICY_VERSION_MINIMUM: "3.5"`. It's harmless, and it guards against CMake 4.x rejecting the old `cmake_minimum_required` calls in the bundled C sources. _(manylinux_2_28 ships CMake 4.x, so this is needed)_
- [x] Run `cargo test --locked --target ${{ matrix.target }}`. The tests in `pty.rs` use `sh` and `/`, so gate them with `#[cfg(unix)]` (C.6). _(runs as `cargo test --release --locked --features static --target …`: without `static` the netcdf crate needs a system libnetcdf, which the runners lack; release+static also shares compiled deps with the build step. Deviation from the Decisions row, awaiting confirmation: see Remaining open questions)_
- [x] Run `cargo build --release --locked --features static --target ${{ matrix.target }}`.
  - The `static` feature means `netcdf-src` + `hdf5-metno-src`, both built through the `cmake` crate, so every runner needs CMake and a C compiler. GitHub images have both.
  - No curl, libxml2 or m4 is needed (DAP is off).
  - On MSVC, leave the CRT at the default `/MD`.
  - zlib comes from `libz-sys`, built statically.
- [x] **Link check.** Fail if anything unexpected is dynamically linked: _(scripts/check-linkage.sh. Linux also allows `libutil.so.1`, which portable-pty's `openpty` needs on glibc < 2.34; macOS also allows `/usr/lib/libobjc*` and `libc++abi`, which objc2/AppKit pull in. Only the Linux allowlist has been exercised (in manylinux_2_28 locally); the macOS and Windows allowlists are unverified until a CI dry run)_
  - **Linux:** `ldd` may show only `libc`, `libm`, `libpthread`, `libdl`, `librt`, `ld-linux` and `libgcc_s`. In particular, no GTK, because `rfd` must use the xdg-portal backend (D.3).
  - **macOS:** `otool -L` may show only `/usr/lib/libSystem*`, `libc++`, `libiconv` and `/System/Library/Frameworks/*`. AppKit is expected because of `rfd`.
  - **Windows:** use `objdump -p` from Git Bash, or `dumpbin` after `ilammy/msvc-dev-cmd@v1`; `dumpbin` is not on PATH by default. Only system DLLs and VC runtime DLLs are allowed. No `hdf5`, `netcdf` or `zlib1` DLLs.
- [x] **Smoke test:** `ngiab-rs --version` prints the tag version. _(the Linux leg's test, build, link check and smoke test were reproduced locally in `manylinux_2_28_x86_64` (CMake 4.4): 26 tests pass, linkage ok, highest symbol GLIBC_2.28. The macOS and Windows legs need a CI dry run)_
- [x] **Package** (this is part of the contract):
  - Asset names: `ngiab-rs-<triple>.tar.gz` on Unix, `ngiab-rs-<triple>.zip` on Windows.
  - The archive root holds `ngiab-rs[.exe]`, `README.md` and `LICENSE`, with no wrapping folder.
  - Upload with `actions/upload-artifact@v4`, name `bin-<triple>`.
- [x] Set `fail-fast: false`. The release job still needs every leg to succeed.
- [ ] If the x86_64 macOS build is cross-compiled on arm, HDF5's configure step may need to run test programs under Rosetta. Verify this with a dry run. _(not done: needs a CI dry run; the workflow builds natively on `macos-15-intel`)_

### A.4 `release.yml`: Docker image (`docker` and `docker-merge` jobs)
- [x] `awiciroh/ngiab` and `rockylinux:9.1` are both multi-arch (amd64 and arm64), so the Dockerfile builds for both platforms.
- [x] Use native runners, not QEMU, because static HDF5/netCDF builds under emulation take hours:
  - `{platform: linux/amd64, runner: ubuntu-24.04}`
  - `{platform: linux/arm64, runner: ubuntu-24.04-arm}`
- [x] Each leg: _(the leg also builds with `load: true` and smoke-tests that image before pushing by digest from the same cache)_
  1. Free disk space (`jlumbroso/free-disk-space`). The base image is several GB.
  2. Run `docker/setup-buildx-action@v3` and `docker/login-action@v3` (`DOCKERHUB_USERNAME`/`DOCKERHUB_TOKEN`).
  3. Run `docker/build-push-action@v6` with:
     - `platforms: ${{ matrix.platform }}`
     - `build-args` from `TOOL_VERSIONS`
     - `outputs: type=image,name=docker.io/joshcu/ngiab-rs,push-by-digest=true,name-canonical=true,push=${{ !dry_run }}`
     - `cache-from` and `cache-to: type=gha,mode=max,scope=<platform>`. If the 10 GB cache limit becomes a problem, switch to `type=registry,ref=joshcu/ngiab-rs:buildcache-<arch>`.
  4. Upload the digest as an artifact.
- [x] `docker-merge` job (skipped on dry runs): _(deviation, awaiting confirmation (see Remaining open questions): `X.Y` is, like `latest`, only moved for stable releases so an rc never shadows a stable X.Y)_
  - Run `docker buildx imagetools create` with tags `X.Y.Z` and `X.Y`, plus `latest` only when `prerelease == false`.
  - Run `imagetools inspect` to confirm both platforms are present.
  - Note: Docker tags have **no `v` prefix**. They must match `CARGO_PKG_VERSION`, because that is what the binary uses by default.
- [ ] Smoke test: `docker run --rm joshcu/ngiab-rs:X.Y.Z --version` on each arch. _(step written, not done: needs a CI run on both arches; each arch tests its locally loaded image before the push, also checking bmi-driver/rs-route and that python3 is absent; amd64 re-tests the merged tag)_
- [x] Keep `ENTRYPOINT` and `CMD` unchanged. The backend runs the tools with `--entrypoint /usr/local/bin/{bmi-driver,rs-route}` (`src/backend.rs:210`).

### A.5 `release.yml`: publish job (`release`)
- [x] `needs: [check-version, build, docker-merge]`. The image must exist before a binary that defaults to it is published. _(also needs `docker`; on dry runs the job still assembles `dist/` + `SHA256SUMS` and uploads it as the `release-dist` artifact, but publishes nothing)_
- [x] Put everything in `dist/`: download all `bin-*` artifacts and copy `install.sh` and `install.ps1` from the repo. _(the step is written, but `install.sh`/`install.ps1` are not in the repo yet (B.5/B.6): dry runs only warn, and a real `v*` tag run fails until they, or stubs, exist)_
- [x] Generate `SHA256SUMS` by running `cd dist && sha256sum * > SHA256SUMS`. This single file covers every asset, including both installers. There are no per-file sidecars.
- [x] Create the release with `softprops/action-gh-release@v2`: _(draft via softprops, then published with `gh release edit --draft=false`)_
  - Create it as a draft, upload all assets, then publish.
  - Set `prerelease` from `check-version`, and `make_latest: true` for stable releases.
  - Use `generate_release_notes: true`.
- [x] Final assets: _(the job fails if any is missing, so a real tag run is blocked on B.5/B.6 adding `install.sh` and `install.ps1`)_
  - `ngiab-rs-x86_64-unknown-linux-gnu.tar.gz`
  - `ngiab-rs-aarch64-apple-darwin.tar.gz`
  - `ngiab-rs-x86_64-apple-darwin.tar.gz`
  - `ngiab-rs-x86_64-pc-windows-msvc.zip`
  - `install.sh`, `install.ps1`
  - `SHA256SUMS`

### A.6 `models.yml`: model bundle (`models` job, `ubuntu-24.04`)
The bundle is only used by the local backend, which is Linux x86_64 only. The user needs no container runtime.
- [x] Read `NGIAB_BASE` with `sed -n 's/^ARG NGIAB_BASE=//p' Dockerfile`. Free disk space, then run `apt-get install -y patchelf`.
- [x] **Extract strictly.** Copy only these from the base image: _(in scripts/build-model-bundle.sh; source_digest is the RepoDigest of the requested repo, since re-tagged images carry several)_
  ```sh
  docker pull --platform linux/amd64 "$NGIAB_BASE"
  digest=$(docker inspect --format '{{index .RepoDigests 0}}' "$NGIAB_BASE")
  cid=$(docker create --platform linux/amd64 "$NGIAB_BASE")
  mkdir -p stage/dmod stage/ngen/ngen/extern/lstm
  docker cp "$cid:/dmod/datasets"    stage/dmod/datasets
  docker cp "$cid:/dmod/shared_libs" stage/dmod/shared_libs
  docker cp "$cid:/ngen/ngen/extern/lstm/trained_neuralhydrology_models" stage/ngen/ngen/extern/lstm/
  docker cp -L "$cid:/usr/lib64/libgfortran.so.5" stage/dmod/shared_libs/   # also libquadmath.so.0
  docker rm "$cid"
  ```
  - Bundle `libgfortran.so.5` and `libquadmath.so.0`. The GCC runtime library exception allows this, and it means the host needs no gfortran package.
  - Never bundle libc or libstdc++.
- [x] **Prune Python.** Run `find stage \( -name .venv -o -name __pycache__ -o -name site-packages -o -name '*.py' -o -name '*.pyc' \) -prune -exec rm -rf {} +`. Python BMI libraries are excluded entirely.
- [x] **Relocate** so the bundle works from any directory: _(values become `../trained_neuralhydrology_models/<run>[/train_data|/img_log]`: the shipped librust_lstm_1025.so replaces `..` with `<runs>/..`, current rust-lstm joins against `<runs>`; both resolve to the run dir. Verified: a 6-member LSTM ensemble and CFE+Noah-OWP+sloth ran with bmi-driver 0.2.1 against the extracted, read-only bundle)_
  - Run `patchelf --set-rpath '$ORIGIN'` on every regular `.so` file in `dmod/shared_libs`, skipping symlinks. This removes the build RPATHs. Libraries then resolve each other without `LD_LIBRARY_PATH`, which is still set as a belt-and-braces measure (C.4).
  - In `trained_neuralhydrology_models/*/config.yml`, rewrite `run_dir:`, `train_dir:` and `img_log_dir:` to relative values. rust-lstm resolves a relative `run_dir` against the parent of the run dirs; check that the shipped `librust_lstm_*.so` does this.
- [x] **Gates.** Each of these fails the job: _(plus a tarball-layout gate; verified to fail on an image with a `/dmod/` text file and a dangling symlink. The Python gate is wider than the prune (also `*.pyd`, `*.pyi`, `*.whl`, `*.dist-info`, `pyproject.toml`, python shebangs, …) so it can catch what the prune missed. The tarball gates run before the sibling `.json` and `SHA256SUMS` are written. A pulled image without a RepoDigest fails, and models.yml checks `source_digest` starts with `sha256:`)_
  - Any `.venv`, `__pycache__`, `site-packages`, `*.py` or `*.pyc` remains.
  - `grep -rIl '/ngen/\|/dmod/' stage` finds a text file.
  - `find stage -xtype l` finds a dangling symlink.
  - The tarball is larger than 1 GiB. Expect about 30–80 MB.
- [x] **Report only, in the job summary:** _(awiciroh/ngiab:latest: min_glibc 2.34 from librust_lstm_1025.so and libgfortran.so.5; libsummabmi misses netcdf/netcdff/sundials; build paths compiled into most model .so files)_
  - `strings` hits for `/dmod` or `/ngen` inside `.so` files. Paths compiled into binaries can't be rewritten.
  - `ldd` "not found" entries, with `LD_LIBRARY_PATH=stage/dmod/shared_libs`.
  - The highest `GLIBC_x.y` symbol across all `.so` files. Write it into the manifest as `min_glibc` (expected 2.34).
- [x] **Manifest.** Write `stage/manifest.json` and the sibling asset `ngiab-models-x86_64-unknown-linux-gnu.json` with the same content:
  ```json
  {"schema":1,"release":"models-2026.11.23","source_image":"awiciroh/ngiab:<tag>","source_digest":"sha256:…",
   "created":"<UTC ISO8601>","target":"x86_64-unknown-linux-gnu","min_glibc":"2.34",
   "prefixes":["/dmod","/ngen/ngen/extern"],"libs":["libcfebmi.so…", "…"]}
  ```
  The sibling `.json` also gets `"tar_sha256"`. The copy inside the tarball can't contain its own hash, so `models update` adds it on install.
- [x] **Package:** _(awiciroh/ngiab:latest gives an 11 MB tarball)_
  ```sh
  tar --sort=name --owner=0 --group=0 --numeric-owner -C stage -czf ngiab-models-x86_64-unknown-linux-gnu.tar.gz .
  ```
  Use gzip, not zstd, so `tar xzf` works everywhere. Then `sha256sum * > SHA256SUMS`.
- [ ] **Publish.** Create the release with `softprops/action-gh-release@v2`, tag `models-YYYY.MM.DD`, `make_latest: false`, `prerelease: false`. Assets: the tarball, the `.json` and `SHA256SUMS`. Put the `source_image` and digest in the release body. _(step written; not done: needs a real models-* tag run)_
- [ ] **Acceptance:** _(partial: the tar layout is verified, and enforced by a gate; CFE+Noah-OWP+sloth and LSTM ran locally from the extracted bundle with paths mapped by hand. Not done: `ngiab-rs models update` on a clean Ubuntu 22.04/24.04 needs B.4 and a published models release)_
  - `tar tzf` lists only `manifest.json`, `dmod/datasets/…`, `dmod/shared_libs/…` and `ngen/ngen/extern/lstm/trained_neuralhydrology_models/…`.
  - After `ngiab-rs models update` on a clean Ubuntu 22.04/24.04 x86_64 machine, a CFE + Noah-OWP + sloth folder and an LSTM folder both run locally with no config edits.
- [x] Note: bumping `NGIAB_BASE` only reaches docker/podman users on the next binary release, because the image is rebuilt only by `release.yml`. When the base changes, cut a models tag and a binary patch release together.

### A.7 Secrets and settings
- [ ] Repo secrets: _(not done: needs the GitHub repo and a Docker Hub token)_
  - `DOCKERHUB_USERNAME` (`joshcu`).
  - `DOCKERHUB_TOKEN`: a personal access token with Read & Write on `joshcu/ngiab-rs`. The Docker Hub repo must already exist.
- [x] `GITHUB_TOKEN` with `contents: write` covers both workflows' release uploads.
- [ ] Optional: a ruleset on `v*` and `models-*` tags so only maintainers can push them. _(not done: needs repo admin on GitHub)_

### A.8 Optional (not requested)
- [ ] `ci.yml` on PRs and pushes to `main`, running: _(optional, not requested)_
  - `cargo fmt --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo test`
  - `cargo check --target x86_64-pc-windows-msvc`

### A.9 Ordering
1. A.1, together with the foundation PR.
2. A.3 and A.6, proven with `workflow_dispatch` dry runs.
3. A.4.
4. A.5. Needs `install.sh` and `install.ps1` in the repo; stubs are fine for an rc.
5. Tag `models-YYYY.MM.DD` and `v0.2.0-rc1`.
6. Test `self update` and `models update` against those.

### A.10 Risks
- **Windows static netCDF.** netcdf-src (with NCZARR) plus hdf5-metno-src under MSVC is the least-tested combination. Try it in a dry run early. Fallbacks:
  - vcpkg static netcdf-c
  - `x86_64-pc-windows-gnu`
  - a netcdf build without NCZARR
- **Intel macOS runner.** `macos-15-intel` is due to be retired around 2027. After that, x86_64 macOS has to be cross-built from arm64 and relies on Rosetta.
- **manylinux and Node.** `actions/checkout` running inside `manylinux_2_28` depends on GitHub's Node runtime working with glibc 2.28. If that breaks, the fallbacks are `cargo-zigbuild --target x86_64-unknown-linux-gnu.2.28`, or building on `ubuntu-22.04` (glibc 2.35).
- **Unsigned macOS binaries.** A `curl` download has no quarantine flag, so it runs. A zip downloaded in a browser is blocked by Gatekeeper. install.sh runs `xattr -d com.apple.quarantine` anyway, best effort. This is acceptable for now.
- **Repo slug.** Confirm `github.com/JoshCu/ngiab-rs` when the remote is created.

---

## B. Install, self-update and model management

This section covers:

- `install.sh` and `install.ps1`, plus the README install block
- `ngiab-rs self update`
- `ngiab-rs models update|revert|status`
- `ngiab-rs backend list|install`, and re-running the installer when a backend is missing

Dependencies are marked as follows:

- **[A]** CI/release: asset names, `SHA256SUMS`, model releases.
- **[C]** Local backend: `Availability`, the Windows port, path mapping.

Facts relied on, checked against the code:

- `src/main.rs` has one flat `Cli` struct with an optional positional `path`. `Detected::detect()` runs first (`main.rs:52`).
- `Cargo.toml` has no HTTP, checksum, archive or semver crates.
- UI state is stored in `dirs::config_dir()/ngiab-rs/state.json` (`src/config.rs:110-140`). Nothing touches `~/.ngiab` yet.
- `~/.ngiab` is **shared with NGIAB_data_preprocess**, which keeps `preprocessor`, `hydrofabric`, `zarr_cache` and other files there. ngiab-rs only ever writes these entries in it:
  - `container/`
  - `container.new/`
  - `container.tmp/`
  - `container.bak/`
  - `install.json`
  - `.lock`

### B.1 Filesystem layout (`src/home.rs`, first)
- [x] `ngiab_home()`: `$NGIAB_HOME`, else `dirs::home_dir()/.ngiab`. _(returns `io::Result`; with neither set it errors instead of falling back to a relative `.ngiab`. The layout functions below return `io::Result` too, and `preprocessor_file()` returns `Option`)_
- [x] `container_dir()`, `container_bak_dir()`, `container_new_dir()`, `container_tmp_dir()`, `install_json()`, `lock_path()`.
- [x] `preprocessor_file()` always returns `dirs::home_dir()/.ngiab/preprocessor` and ignores `NGIAB_HOME`. Used by D.1.
- [x] Layout:
  ```
  ~/.ngiab/
    install.json        # written by ngiab-rs (installer calls `ngiab-rs backend record`)
    .lock               # models update/revert lock
    container/          # local-backend model bundle (Linux x86_64 only)
      manifest.json     # models::Manifest (+ tar_sha256, installed_at, installed_from)
      dmod/datasets/...
      dmod/shared_libs/...            # symlinks preserved, RPATH=$ORIGIN
      ngen/ngen/extern/lstm/trained_neuralhydrology_models/...
    container.bak/      # previous container/ (one level of undo)
    preprocessor, hydrofabric/, ...   # owned by NGIAB_data_preprocess - never touched
  ```
- [x] `install.json` (`struct InstallInfo`, serde): _(`bin_path` is `Option<PathBuf>`; `save()` writes atomically)_
  ```json
  {"schema":1,"ngiab_rs_version":"0.2.0","target":"x86_64-unknown-linux-gnu",
   "bin_path":"/home/u/.local/bin/ngiab-rs","install_method":"installer",
   "backends":["local","docker"],"repo":"JoshCu/ngiab-rs","installed_at":"…"}
  ```
  - `install_method` is one of `installer`, `cargo` or `container`.
  - `load()` treats a missing or unreadable file as default, the same way `config::State` does.
- [x] **Acceptance:** unit tests check that `NGIAB_HOME` is honoured, that `preprocessor_file()` ignores it, and that `InstallInfo` round-trips (including the missing-file default). _(`public_fns_read_the_environment` re-runs the test binary with `NGIAB_HOME` set to exercise the public functions)_

### B.2 CLI restructure (`src/cli.rs`, `src/main.rs`)
Land this before any other change adds flags.

- [x] Shape: _(the `self` variant is `Cmd::SelfManage` (clippy rejects a variant named like its enum); `self update`, `models update|revert|status` and `backend list|install|record` are stubs in `src/selfupdate.rs`, `src/models.rs` and `src/installer.rs` that fail with "not implemented yet")_
  ```rust
  #[derive(Parser)]
  #[command(version, about, args_conflicts_with_subcommands = true)]
  pub struct Cli { #[command(subcommand)] pub cmd: Option<Cmd>, #[command(flatten)] pub run: RunArgs }

  pub enum Cmd {
      #[command(name = "self")] SelfCmd { #[command(subcommand)] cmd: SelfCmd },   // update [--version V] [--check] [-y]
      Models  { #[command(subcommand)] cmd: ModelsCmd },  // update [--version models-…] [--file TAR] [--check] [--force] [-y] | revert [-y] | status
      Backend { #[command(subcommand)] cmd: BackendCmd }, // list | install <kinds…> | record <kinds…> (hidden)
      Run(RunArgs),                                       // explicit default action
  }
  ```
  `RunArgs` contains everything that exists today: `path`, `--auto`, `-j`, `--backend` (env `NGIAB_RS_BACKEND`), `--image` (env `NGIAB_RS_IMAGE`) and `--no-routing`. New run flags such as `--no-mouse` (D.5) also go on `RunArgs`.
- [x] `main()` dispatches with `match cli.cmd`. The current body of `main()` becomes `run_main(RunArgs)`. _(`backend` also skips `detect()` for now; B.7 decides what it needs)_
  - `self` and `models` must not call `Detected::detect()`, because the docker/podman probes can take up to 10 seconds.
- [x] A data folder named `self`, `models`, `backend` or `run` would be parsed as a subcommand. Document `ngiab-rs ./models` and `ngiab-rs run models` in the README and in the help text.
- [x] **Acceptance:** `Cli::try_parse_from` tests for: _(in `src/cli.rs`, plus `debug_assert`, rejected inputs and the help-text note)_
  - `[]`
  - `["/x"]`
  - `["/x","--auto","-j","4"]`
  - `["--auto","/x"]`
  - `["self","update","--check"]`
  - `["models","revert"]`
  - `["./models"]`
  - `["run","models"]`
  - `["backend","install","docker","podman"]`

  The Docker `CMD ["/data"]` and `/data --auto -j 8` must keep working.

### B.3 `src/net.rs` and `ngiab-rs self update` (`src/selfupdate.rs`)
- [ ] Dependencies: _(partial: `ureq` 3 (features `rustls`, `json`, `gzip` confirmed), `sha2`, `flate2`, `tar`, `semver` and `zip` are added; `zip` uses `deflate-flate2` (decompression only; `deflate` would pull in the zopfli compressor). Not done: `self-replace`, deferred to the `self update` step because rustc 1.100 warns on unused dependencies)_
  - `ureq = { version = "3", default-features = false, features = ["rustls","json","gzip"] }`. Check the exact feature names when adding it.
  - `sha2`, `flate2`, `tar`, `semver`, `self-replace`.
  - `zip` (deflate only), under `[target.'cfg(windows)'.dependencies]`.
  - Do not use the `self_update` crate. It pulls in reqwest and tokio, and has its own naming conventions.
- [x] `net.rs`. This is the single HTTP module; `self update`, `models update` and USGS (D.8) all use it. _(also `get()`, `get_json()`, `get_text()`, `part_path()`, `expected_sha256()`, `tag_version()`, `latest_download_url()`, `release_download_url()` and `latest_tag_via_redirect()`. `extract_tar_gz` checks every entry before `unpack` (no `..`, absolute paths, links leaving the tree or special files), because `tar` alone strips a leading `/` and silently skips `..` entries. Unit tests cover the pure parts; the `#[ignore]`d `net::tests::network_smoke` was run against cli/cli and CIROH-UA/rs_route)_
  - `agent()` sets:
    - `User-Agent: ngiab-rs/<ver>`.
    - A timeout.
    - `Authorization: Bearer $GITHUB_TOKEN`, but only for `api.github.com`, and only if the variable is set. Unauthenticated requests are limited to 60 an hour.
  - `github_release(repo, Latest | Tag(t))` returns `Release { tag_name, assets: [{name, browser_download_url, size}] }`.
  - `github_releases(repo)` returns the paginated list. `models update` needs it to find the newest `models-*` release.
  - `download(url, dest, progress)` streams to `dest.part`, then renames it into place. Progress goes to stderr as a single `\r` line.
  - `sha256_file(path)`.
  - `verify_sha256sums(sums_text, asset_name, file)` checks a file against `<hex>  <name>` lines.
  - `extract_tar_gz(archive, into)` uses `tar::Archive::unpack`, which rejects `..` and absolute paths and preserves symlinks. `extract_zip` does the same for Windows.
  - Rate-limit fallback for binaries: `https://github.com/{repo}/releases/latest/download/<asset>`. The newest tag can be read from the `Location` header of the `/releases/latest` redirect.
- [x] `const REPO: &str = "JoshCu/ngiab-rs"`, overridable with `NGIAB_RS_REPO` for testing forks. _(`home::REPO` and `home::repo()`; `InstallInfo::default` uses it)_
- [ ] The asset name is `ngiab-rs-{env!("NGIAB_TARGET")}.{tar.gz|zip}` **[A]**. The binary is at the archive root.
- [ ] `self update [--version V] [--check] [-y]`:
  1. Refuse to update in these cases:
     - **Running in a container.** Factor `in_container()` out of `Detected::detect` and reuse it here. Message: "pull the image instead".
     - **A cargo install.** That is, the canonical exe path is under `~/.cargo/bin`, or `install_method == "cargo"`. Message: "use cargo install --force".
     - **The exe's directory is not writable.**
  2. Resolve the target release: the newest, or the one given by `--version`, with or without a leading `v`. Compare it with `CARGO_PKG_VERSION` using `semver`.
     - If it is not newer and no version was asked for, print "up to date" and exit 0.
     - `--check` exits 0 when up to date and 10 when an update is available.
  3. Download into a temp dir **in the same directory as the exe**, so the final rename is atomic. Verify against the release's `SHA256SUMS`, extract, and `chmod 0755` on Unix.
  4. Smoke test: run `<new> --version` (5-second timeout) and require the expected version. This catches glibc mismatches.
  5. Replace the binary with `self_replace::self_replace(&new)`.
     - On Windows this leaves `ngiab-rs.exe.old`.
     - Every start then deletes any stale `*.old` file next to the exe (Windows only, best effort).
  6. Update `install.json`.
  7. Print hints:
     - "The docker/podman image for this version is `joshcu/ngiab-rs:X.Y.Z`; it is pulled on next use, or run `docker pull …`" (the default image is pinned, see A.1).
     - If a model bundle is installed and a newer `models-*` release exists: "new model files available: run `ngiab-rs models update`". Don't run it automatically.
- [ ] Edge cases:
  - Leftover `.part` files are cleaned up on the next run.
  - A checksum mismatch aborts without touching the exe.
  - If the Windows exe is locked (for example by antivirus), report it and suggest re-running install.ps1.
  - A downgrade via `--version` needs confirmation unless `-y` is given.
- [ ] **Acceptance:**
  - v0.2.0-rc1 updates to rc2 on Linux, both macOS architectures and Windows.
  - A corrupted `SHA256SUMS` leaves the exe unchanged.
  - The `--check` exit codes behave as specified.

### B.4 `ngiab-rs models update | revert | status` (`src/models.rs`)
- [ ] `models::Manifest` follows the contract schema in A.6. Fields: `schema`, `release`, `source_image`, `source_digest`, `created`, `target`, `min_glibc`, `prefixes`, `libs`, `tar_sha256`, plus `installed_at` and `installed_from` added on install. Parsing tolerates unknown fields.
  - If `schema` is newer than this binary understands, print "run `ngiab-rs self update`".
- [ ] Releases: model bundles are published under **separate tags `models-YYYY.MM.DD[.N]`**, which are never marked latest **[A]**.
  - To find the newest: `net::github_releases(REPO)`, keep tags starting with `models-` that are not drafts, and take the maximum by dotted-numeric comparison.
  - `--version models-2026.11.23` (or just `2026.11.23`) pins a specific release.
- [ ] `models status` prints:
  - the installed release, `source_image` and install date
  - the backup release (from `container.bak/manifest.json`)
  - the newest release available (network failures are reported softly)
  - disk usage
  - glibc compatibility: host `gnu_get_libc_version()` compared with `min_glibc`
- [ ] `models update [--version R] [--file TAR] [--check] [--force] [-y]`:
  1. **Platform gate.** Unless the target is `cfg(all(target_os="linux", target_arch="x86_64"))`, print "model files are only used by the local backend on Linux x86_64; docker/podman need nothing" and exit 0.
     - Also warn, but continue, if host glibc is below `min_glibc`. The user decided that systems with glibc below 2.34 are expected to use docker/podman.
  2. Take an exclusive lock on `~/.ngiab/.lock` with `File::try_lock` (stable since Rust 1.89).
  3. Recover from any earlier crash (see below).
  4. Get the sibling `ngiab-models-x86_64-unknown-linux-gnu.json` for the chosen release.
     - If `--force` is not set and its `tar_sha256` matches the installed one, print "up to date" and stop.
     - `--check` only reports.
  5. Download the tarball to `~/.ngiab/container.new.tar.gz.part`, or use the file given by `--file`. With `--file`, if a sibling `.json` is present, verify against its `tar_sha256`. Otherwise warn that the file is unverified.
     - Verify against the release's `SHA256SUMS`.
  6. Run `rm -rf container.new`, extract into it, then write `manifest.json` with `tar_sha256`, `installed_at` and `installed_from` added.
  7. Swap using renames on the same filesystem:
     - `rm -rf container.bak`
     - `container` → `container.bak` (if it exists)
     - `container.new` → `container`

     If the second rename fails, move `container.bak` back.
  8. Print the old and new releases and the backup location. Only one level of undo is kept, because each update deletes the previous `.bak`. This is what was asked for.
- [ ] `models revert [-y]`:
  - Requires `container.bak` with a valid manifest. Otherwise print "nothing to revert" and exit 1.
  - Do a three-way swap: `container` → `container.tmp`, `container.bak` → `container`, `container.tmp` → `container.bak`. Running `revert` twice therefore re-applies the update; document this in the README.
  - Asks for confirmation unless `-y` is given. Uses the same lock as `update`.
- [ ] Crash recovery (every `models` command, under the lock):
  - If `container` is missing and `container.tmp` exists, rename it back.
  - Delete any stale `container.new` and `*.part`.
  - Report what was done.
  - Never touch anything else in `~/.ngiab`.
- [ ] `Detected` gains `models: Option<Manifest>`, read from `container/manifest.json`. It feeds `Availability` for local **[C]**.
- [ ] **Acceptance:**
  - Install, update, update again: only one `.bak` remains, and an unchanged sha skips the download.
  - After update then revert, the previous manifest is back. A second revert swaps them again.
  - Killing the process during extraction leaves `container/` intact.
  - `readlink container/dmod/shared_libs/libcfebmi.so` still resolves.
  - On macOS and Windows the command prints the no-op message.

### B.5 `install.sh` (POSIX sh, repo root, published as a release asset)
- [ ] Usage: `curl -LsSf https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.sh | sh`. Pass flags with `| sh -s -- --backends docker -y`.
- [ ] Flags and environment variables (a flag wins over its env var):

  | flag | env | meaning |
  | --- | --- | --- |
  | `--version X.Y.Z` | `NGIAB_RS_VERSION` | pin the binary release (default latest) |
  | `--install-dir DIR` | `NGIAB_RS_INSTALL_DIR` | default `$HOME/.local/bin` |
  | `--backends local,docker,podman\|none` | `NGIAB_RS_BACKENDS` | skip the menu |
  | `--skip-binary` | `NGIAB_RS_SKIP_BINARY=1` | only set up backends (used by `ngiab-rs backend install`) |
  | `--no-modify-path` | `NGIAB_RS_NO_MODIFY_PATH=1` | don't edit shell rc files |
  | `-y`, `--yes` | `NGIAB_RS_YES=1` | accept all prompts, including sudo/package installs |
- [ ] Platform detection:
  - Map `uname -s`/`uname -m` to `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin` or `x86_64-apple-darwin`.
  - On an Intel macOS shell running under Rosetta (`sysctl -n sysctl.proc_translated` returns 1), prefer the arm64 binary.
  - On Linux aarch64 and anything else, print that there is no prebuilt binary and suggest the container (`docker run -it -v "$PWD:/data" joshcu/ngiab-rs`) or `cargo install --git …`. Then exit 1.
- [ ] Download:
  - Use `curl`, or fall back to `wget`.
  - Verify against `SHA256SUMS` with `sha256sum`, or `shasum -a 256` on macOS.
  - Extract to a temp dir, then `install -m 755` into the install dir. On macOS, run `xattr -d com.apple.quarantine` (best effort).
  - If the install dir is not on `PATH`, add an `export PATH=…` line marked `# ngiab-rs` to each of `~/.bashrc`, `~/.zshrc` and `~/.profile` that exists. For fish, write `~/.config/fish/conf.d/ngiab-rs.fish`. This must be idempotent.
- [ ] Prompts read from `</dev/tty`. If `/dev/tty` can't be read and neither `--backends` nor `-y` was given, install only the binary, print the non-interactive command line, and never hang.
- [ ] Backend menu: numbered and multi-select (`1 3`, `all`, or Enter for the recommended choice). Each entry describes what it will do.
  1. **local**: runs bmi-driver and rs-route as native binaries, with no containers. It is the fastest option. **Linux x86_64 only**; elsewhere it is shown as unavailable with the reason.
     - Will:
       - Check for rustup, or offer to install it (`curl https://sh.rustup.rs | sh -s -- -y`).
       - Check for a C/C++ compiler, `cmake` and `make`, and offer an install command for each package manager:
         - apt: `build-essential cmake`
         - dnf: `gcc gcc-c++ make cmake`
         - pacman: `base-devel cmake`
         - zypper: the equivalent packages

         gfortran and Python headers are **not** needed. Fortran runtime libraries are bundled in the model files.
       - Source `TOOL_VERSIONS` (fetched from the release tag) and run `cargo install bmi-driver --version $BMI_DRIVER_VERSION --locked --features static`, then the same for `rs_route`. This takes about 5–15 minutes; say so.
       - Check `ldd --version` against the bundle's `min_glibc` (2.34). If it is lower, warn that local won't work and recommend docker/podman.
       - Run the freshly installed `ngiab-rs models update -y` to download the model files (about 30–80 MB) into `~/.ngiab/container`. The download logic is not duplicated in sh.
  2. **docker**: runs each tool in `joshcu/ngiab-rs:<version>`.
     - If docker is present, run `docker info`. On a permission error, offer `sudo usermod -aG docker $USER` and explain that a re-login is needed. If the daemon is down, offer `sudo systemctl enable --now docker`.
     - If docker is missing:
       - Linux: offer `curl -fsSL https://get.docker.com | sudo sh`. Fall back to the package manager or a docs link on distros it doesn't support, such as Arch and openSUSE.
       - macOS: `brew install --cask docker`, or the Docker Desktop URL. Docker Desktop has to be launched once before it works.
     - Then run `docker pull docker.io/joshcu/ngiab-rs:<installed version>`. The tag must match the binary's version, because the default image is pinned to it.
  3. **podman**: rootless and daemonless.
     - Install it with apt, dnf, pacman or zypper, or `brew install podman`.
     - On macOS, run `podman machine init && podman machine start` if no machine exists.
     - Then pull the same version-matched image.
  - Default choice:
    - `local` on Linux x86_64 when cargo is already present.
    - Otherwise any container runtime that already works.
    - Otherwise docker.
  - Echo every sudo or package-manager command and ask for confirmation (skipped with `-y`).
  - A failure in one backend doesn't stop the others. Print a summary table at the end.
- [ ] Record the result with `ngiab-rs backend record <kinds>`. This updates `install.json`, so the shell script doesn't have to write JSON.
- [ ] `shellcheck` clean. Tested under dash and bash on ubuntu:24.04, fedora and macOS. Re-running it upgrades the binary and re-checks the backends.
- [ ] **Acceptance:**
  - On a clean ubuntu:24.04, `--backends none` works, and `ngiab-rs --version` runs.
  - `--backends local -y` leads to a working `--auto` run on a sample folder **[C]**.
  - Run with no tty and no flags, it never hangs.

### B.6 `install.ps1` (Windows, published as a release asset)
- [ ] Usage: `powershell -ExecutionPolicy ByPass -c "irm https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.ps1 | iex"`.
  - Configuration comes **only from environment variables**: `NGIAB_RS_BACKENDS`, `NGIAB_RS_VERSION`, `NGIAB_RS_INSTALL_DIR`, `NGIAB_RS_NO_MODIFY_PATH`, `NGIAB_RS_YES` and `NGIAB_RS_SKIP_BINARY`.
  - There is no `param()` block, because it conflicts with `irm | iex`.
  - `-File` is used only for the pinned re-run from B.7, which also passes its settings through these variables.
- [ ] Architecture: AMD64 is supported. ARM64 gets a warning and the x64 build, which runs under emulation. Anything else fails.
- [ ] Install steps:
  1. Set TLS 1.2 so Windows PowerShell 5.1 can download.
  2. Download the zip and `SHA256SUMS` with `Invoke-WebRequest -UseBasicParsing`, and verify with `Get-FileHash -Algorithm SHA256`.
  3. `Expand-Archive` it.
  4. Install to `%USERPROFILE%\.local\bin\ngiab-rs.exe`. If the exe is in use, rename the old one to `.old` first.
  5. Add the directory to the user `Path` via `[Environment]::SetEnvironmentVariable`, and to `$env:Path` for the current session.
- [ ] Backend menu offers docker and podman only, with the line "local: not available on Windows (model libraries are Linux-only)".
  - **docker:** `winget install -e --id Docker.DockerDesktop`.
    - Needs WSL2. Check `wsl --status`; if it fails, suggest `wsl --install` and a reboot.
    - Docker Desktop must be launched once.
    - Then `docker pull` the version-matched tag.
  - **podman:** `winget install -e --id RedHat.Podman`, then `podman machine init` and `podman machine start`, then pull.
  - If winget is missing, print the download URLs instead.
- [ ] Only call `Read-Host` when the session is interactive and `NGIAB_RS_YES` is not set.
- [ ] Record the result with `ngiab-rs backend record`.
- [ ] **Acceptance:**
  - On a fresh `windows-latest` with `NGIAB_RS_BACKENDS=none`, `ngiab-rs --version` works in a new shell.
  - Manual test with Docker Desktop: `--auto` completes on sample data **[C]**.

### B.7 Re-running the installer when a backend is missing
This is the only place it is implemented. The local section relies on it.

- [ ] `ngiab-rs backend list` prints, for each backend, its `Availability` **[C]** with the reason, the platform support, and the model-bundle status.
- [ ] `ngiab-rs backend install <kinds…>` (`src/installer.rs`):
  1. Download the installer **pinned to the running version**: `https://github.com/{REPO}/releases/download/v{CARGO_PKG_VERSION}/install.sh`, or `install.ps1` on Windows.
  2. Verify it against that release's `SHA256SUMS`.
  3. Run it with inherited stdio:
     - Unix: `sh <tmp> --skip-binary --backends <kinds>`.
     - Windows: set `NGIAB_RS_SKIP_BINARY=1` and `NGIAB_RS_BACKENDS=<kinds>`, then run `powershell -ExecutionPolicy Bypass -File <tmp>`.
  4. Run `Detected::detect()` again and report the result.
  - Refuse `local` on anything other than Linux x86_64, and refuse inside a container.
- [ ] CLI hook in `run_main`. This replaces the hard error at `src/main.rs:60-64`.
  - It applies when `--backend X` is `NotInstalled` or `Unsupported` but installable, or when no backend is available at all, and stdin and stdout are both terminals and `--auto` is not set.
  - Prompt: "Backend 'X' is not set up (<reason>). Run the installer now? [y/N]".
  - On yes: `installer::run`, then detect again and continue.
  - Otherwise: show the current error plus "run `ngiab-rs backend install X`".
  - For `NotRunning`, don't offer to reinstall. Print the matching start hint instead: `systemctl start docker`, open Docker Desktop, or `podman machine start`.
- [ ] TUI hook:
  - Add `MenuItem::InstallBackend` ("Install backend…") to `App::menu_items()` (`src/app.rs:195`) when a backend that is supported on this platform is `NotInstalled`.
  - Replace the "No backend available" popup in `request_run` (`src/app.rs:291-297`) with a confirm popup.
  - Add the field `want_installer: Option<Vec<BackendKind>>`. It goes through the shared helper "suspend TUI → run closure → resume" (`main.rs:140-147` is the existing `want_shell` pattern). Afterwards: `app.detected = Detected::detect()` and `refresh_preflight()`.
  - Add `declined_install: HashSet<BackendKind>` so the prompt isn't repeated in the same session.
  - The backend picker is a small checkbox popup listing the backends that are not installed.
- [ ] **Acceptance:**
  - On a machine with only podman, `ngiab-rs ./data --backend docker` prompts, installs, and continues.
  - `--auto` never prompts.
  - No prompt appears inside a container.
  - A stopped daemon gets the start hint, not an installer prompt.

### B.8 README
- [ ] At the very top, under the title:
  ```sh
  # macOS and Linux
  curl -LsSf https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.sh | sh
  # Windows
  powershell -ExecutionPolicy ByPass -c "irm https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.ps1 | iex"
  ```
- [ ] A backend-by-platform table:
  - local: Linux x86_64 only, glibc ≥ 2.34, needs Rust plus cc/cmake/make.
  - docker/podman: Linux, macOS and Windows.

  Note that Python BMI models and SUMMA are not supported.
- [ ] Non-interactive examples, the install location and PATH changes, and **uninstall**:
  - Delete the binary.
  - Delete `~/.ngiab/container`, `~/.ngiab/container.bak` and `~/.ngiab/install.json`. **Do not delete the rest of `~/.ngiab`**; it belongs to NGIAB_data_preprocess.
  - Remove the `# ngiab-rs` PATH lines.
- [ ] Document:
  - `self update`
  - `models update|revert|status`, including that running revert twice re-applies the update
  - `backend list|install`
  - the `run` escape hatch for data folders named like a subcommand
  - the "Releasing" steps (A.2)
- [ ] Update the usage block (`README.md:8-19`).

### B.9 Ordering
1. B.1 and B.2 (with `net.rs`).
2. B.3 and B.4, in parallel. Both need rc releases **[A]**.
3. B.5 and B.6.
4. B.7 (needs `Availability` **[C]**) and B.8.

### B.10 Risks
- **Unattended installs.** Several steps can't fully complete without the user:
  - Docker Desktop on macOS and Windows needs a GUI first launch and licence acceptance.
  - `get.docker.com` doesn't support every distro.
  - Being added to the docker group only takes effect after a re-login.

  The installer must say so rather than report success.
- **GitHub API rate limits.** 60 requests an hour is shared across a NAT. Binaries have the `releases/latest/download` fallback. `models update` needs the API to find the newest `models-*` release, so honour `GITHUB_TOKEN` and suggest it in the error message.
- **Repo slug.** Confirm `JoshCu/ngiab-rs` when the remote is created.

---

## C. Local backend: transparent use of `~/.ngiab/container/`

**Goal.** On Linux x86_64, a user with `bmi-driver`, `rs-route` and the model bundle can run any NGIAB data folder with `--backend local`. They never edit `realization.json` or the per-catchment configs. The only change to the user's `config/` is bmi-driver's optional mapping write-back (C.3.4), which keeps a `.bak`.

### Facts this plan relies on (checked against the code and the images)
- **bmi-driver v0.2.1 CLI** (`JoshCu/bmi-driver/src/main.rs:70-119`):
  - Accepts `--config <realization.json>` and `--hf <gpkg>`.
  - Canonicalizes both paths, then runs `chdir(data_dir)`. Relative paths in a realization still resolve against the data folder even when the realization file lives somewhere else.
  - Has no option to remap paths. One wouldn't be enough anyway: model libraries read their own init files, e.g. Noah-OWP's `parameter_dir = "/dmod/datasets/static/"`.
- **bmi-driver can rewrite its realization.** At `main.rs:247-255` it may ask "Add these mappings to realization.json? [y/N]". If the user agrees, it writes to the file passed as `--config`.
- **`awiciroh/ngiab` contents:**
  - `/dmod/datasets` is 80K.
  - `/dmod/shared_libs` is 11M: cfe, sloth, noah `surfacebmi`, lasam, pet, sac, snow17, sft, smp, topmodel, summa and `librust_lstm_1025.so`, plus `.so.X.Y.Z` symlinks.
  - `/ngen/ngen/extern/lstm` is 69M, but the Rust LSTM only needs `trained_neuralhydrology_models` (9.7M).
- **Library dependencies:**
  - `libsacbmi`, `libsnow17bmi`, `libsurfacebmi` and `libsummabmi` need `libiso_c_bmi.so`. Their RPATHs point to build dirs that don't exist, which is why CI patches them to `$ORIGIN` (A.6).
  - The Fortran libs need `libgfortran.so.5` and `libquadmath.so.0`. These are bundled (A.6).
  - The C++ libs need the host's `libstdc++.so.6`.
  - The highest glibc requirement is **2.34**, from `librust_lstm_1025.so`. The user has accepted that older distros must use docker/podman.
  - `libsummabmi` also needs sundials, netcdf/netcdf-fortran and openblas. These are missing even from the image, so **SUMMA is unsupported on every backend**. The preflight `ldd` check (C.5.2) reports this.
- **The LSTM is Rust, not Python.** It is configured as `"name":"bmi_c"`, `"model_type_name":"bmi_rust"`, `"registration_function":"register_bmi_lstm"`. bmi-driver is built without `--features python`, so `bmi_python` modules are unsupported locally and, once Python is dropped from the image (A.1), in docker too.
- **LSTM `config.yml` files contain an absolute `run_dir: /ngen/ngen/extern/lstm/...`.** CI rewrites these to relative paths (A.6). rust-lstm resolves a relative `run_dir` against the parent of the run dirs (`rust-lstm/src/lib.rs:271-289`).
- **rs-route** only uses `-i`, `-o` and the root, plus relative paths in `troute.yaml`. It needs no mapping.
- **Paths seen in real realizations:**
  - Both `/dmod/shared_libs/libcfebmi.so.1.0.0` and unversioned names, so the bundle must keep its symlinks.
  - Arbitrary host paths. Leave these alone.
  - `init_config` templates such as `./config/cat_config/lstm/{{id}}.yml`.

### C.1 Bundle contract (consumed from A.6 and B.4)
- [ ] The container path `/X` maps to `home::container_dir()/X`, so mapping is a prefix join:
  ```
  ~/.ngiab/container/
    manifest.json
    dmod/datasets/static/...
    dmod/shared_libs/*.so*                     (symlinks, RPATH=$ORIGIN, + libgfortran/libquadmath)
    ngen/ngen/extern/lstm/trained_neuralhydrology_models/...   (relative run_dir)
  ```
- [ ] `src/models.rs` provides `Manifest` and `Manifest::load(container_dir())`. Mapped prefixes come from `manifest.prefixes`. If no manifest exists, fall back to `["/dmod", "/ngen/ngen/extern"]`.

### C.2 Path mapping (`src/paths.rs`)
- [ ] Treat container paths as POSIX **strings**, not `Path`. On Windows `Path::new("/dmod/x").is_absolute()` is false.
  ```rust
  pub enum Resolved { Host(PathBuf), InImage, Unmapped(PathBuf) }
  pub struct PathMap { pub kind: BackendKind, pub data_dir: PathBuf, pub model_root: Option<PathBuf>, pub prefixes: Vec<String> }
  impl PathMap {
      pub fn resolve(&self, p: &str) -> Resolved;
      pub fn rewrite_str(&self, s: &str) -> Option<String>; // Some if changed
      pub fn unrewrite_str(&self, s: &str) -> Option<String>; // model_root/... -> /dmod/... (for write-back)
  }
  ```
- [ ] Mapping rules. A prefix only matches on a component boundary (it must be followed by `/`, the end of the string, or a quote), so `/database` does not match `/data`.

  | input | container backends | local |
  |---|---|---|
  | relative (`./x`, `x`) | `data_dir/x` | `data_dir/x` |
  | `/data/...`, `/ngen/ngen/data/...` | `data_dir/...` | **always** `data_dir/...` (user decision: on local, `/data` *is* the folder the user picked, e.g. `~/ngiab_preprocess_output/7_year_provo`) |
  | `/dmod/...`, `/ngen/ngen/extern/...` | `InImage` | `model_root/dmod/...` etc. (`Unmapped` if no bundle) |
  | other absolute | `InImage` | literal |

  The local rewrite therefore also covers `/data` and `/ngen/ngen/data`, which map to `data_dir`.
- [ ] Replace `preflight::host_path` (`preflight.rs:80-94`) with `PathMap::resolve`. Keep a thin wrapper so the call sites at `preflight.rs:153,215,232` barely change.
  - Move the tests at `preflight.rs:373-390` to `paths.rs`.
  - The local `/data` case in the test at `preflight.rs:389` changes, because `/data` now always maps.
  - Add test cases for the extern and dmod prefixes, boundary matching, spaces in paths, and a Windows-style `data_dir`.
- [ ] In `plan.rs:38`, the absolute `output_root` passed to rs-route `-i` must go through `PathMap`. On local, `/ngen/ngen/data/outputs/ngen` and `/data/outputs/ngen` both become `data_dir/outputs/ngen`.

### C.3 Staging (`src/stage.rs`), local backend only
Decision: rewrite the configs into a staging copy and pass `--config` to bmi-driver. Alternatives were rejected:
- Symlinking `/dmod` needs root.
- `unshare` and bind mounts are Linux-only and fragile.
- A path remap inside bmi-driver can't reach files that the libraries read themselves.

- [ ] **C.3.1 Staging dir (user decision): `<data>/.ngiab-rs/stage/`.** It is wiped and recreated on every local run.
  - It lives next to `config/`, so the echoed command stays self-contained and can be copy-pasted.
  - `preflight::safe_to_clean` (`preflight.rs:97`) and `count_files` must never count or delete `.ngiab-rs`. Only output dirs are cleaned.
  - D.1's "is this a data folder" detection must ignore it.
  - The staged files are generated, so no `.gitignore` is needed inside it.
- [ ] **C.3.2** `pub fn stage(map: &PathMap, data_dir: &Path, divide_ids: &[String]) -> Result<Staged>`, where `Staged { realization: PathBuf, rewrote: usize, original_hash: u64 }`:
  1. Parse `config/realization.json` as a `serde_json::Value`. Walk it generically: `global`, `catchments.*` overrides, and nested `modules[*].params`.
  2. Rewrite every string value that starts with a mapped prefix using `rewrite_str`. Edits are made on the JSON value, not on text, so escaping and spaces are safe.
  3. For each `init_config`, expand `{{id}}` for every divide. Change `read_divides` (`preflight.rs:305`) so it can return all ids.
     - If none of the expanded files contains a mapped prefix, leave the template alone. This is the usual case for CFE and sloth.
     - Otherwise, write files to `stage/init/<module-index>/<same relative path>`:
       - Files that contain a prefix get the rewritten text.
       - All other files are symlinked to the original.
       - Point `init_config` at the staged absolute template, keeping `{{id}}`.
     - Binary files (a NUL byte in the first 8 KB) are symlinked, never rewritten.
  4. Text rewrites only happen at a boundary: the preceding character must be start-of-line, whitespace, `"`, `'`, `=`, `:`, `[` or `,`.
     - Warn if `model_root` contains `" #"`, `": "` or quotes, because those break unquoted YAML or namelist scalars.
  5. Write `stage/realization.json` and return the hash of what was written.
- [ ] **C.3.3 Edge cases:**
  - A template without `{{id}}`: stage that single file.
  - Missing per-catchment files: skip them, because preflight already fails on the first divide.
  - Large domains (100k+ catchments × several modules):
    - Run staging as a new `StepKind::Stage` (`plan.rs:5`) with progress.
    - In auto mode, print "staging N files…".
  - Symlinks are used rather than hardlinks, so there are no filesystem constraints.
- [ ] **C.3.4 Mapping write-back (user decision: apply to the real file, with a `.bak`).** After the bmi-driver step, if the hash of `stage/realization.json` differs from `original_hash`:
  1. Reverse-map the value:
     - Staged `init_config` goes back to the original template.
     - `model_root/...` goes back to the container prefix, via `unrewrite_str`.
     - The `data_dir` paths that were mapped from `/data` and `/ngen/ngen/data` go back to their original strings. Keep a reverse table built during staging.
  2. Copy `config/realization.json` to `config/realization.json.bak`.
  3. Write the reverse-mapped JSON to `config/realization.json`.
  4. Tell the user: a flash message in the TUI, or stderr in auto mode.

  Hook it into `App::advance` (`app.rs:378`) and the step loop in `auto.rs:53-70`. The result matches what a docker run would have written.
- [ ] **C.3.5** `stage()` does nothing for container backends or when nothing needs rewriting. In that case run without `--config`, so the command line is the same as today.

### C.4 Command construction: env and `--config`
- [x] Add `pub env: Vec<(String, String)>` to `CommandSpec` (`backend.rs:140`). _(plus `CommandSpec::new` and `to_command()`; display shell-quotes values)_
  - Apply it in `pty.rs:46`, `auto.rs:58` and `run_shell` (`main.rs:155-178`).
  - `CommandSpec::display` (`backend.rs:148`) prints `K=V` first, so the command can be copy-pasted.
- [ ] Local `Tool::BmiDriver` in `Launcher::tool_command` (`backend.rs:185-193`) sets:
  - `LD_LIBRARY_PATH=<model_root>/dmod/shared_libs[:$LD_LIBRARY_PATH]`. This is still needed alongside the `$ORIGIN` RPATH, because user libraries that bmi-driver dlopens may need bundled libs. Worker subprocesses inherit it.
  - `OMP_NUM_THREADS=1` if unset, matching the Dockerfile.
- [ ] `plan::tool_args` (`plan.rs:29`) gains `staged: Option<&Path>`. When set, the bmi-driver args become `[root, "--config", staged, "-j", N]`. Update the test at `plan.rs:76-93`.
- [ ] `Launcher` gains `model_root: Option<PathBuf>`, set in:
  - `App::launcher` (`app.rs:178`)
  - the `--auto` launcher (`main.rs:86`)
  - `run_shell` (`main.rs:155-178`)

  Update the fixtures at `backend.rs:404-415`.

### C.5 Preflight (`src/preflight.rs`)
- [ ] **C.5.1 Models check.** Local only, placed after the backend checks (`preflight.rs:260-270`).
  - No bundle → **Fail** "model files not installed: run `ngiab-rs models update`".
  - Unreadable manifest → **Warn**.
  - Otherwise → **Ok** "models-YYYY.MM.DD (<root>)".
  - Add `pub models: Option<Manifest>` to `Preflight`.
- [ ] **C.5.2 Library check** (`preflight.rs:241-250`), resolved through `PathMap`:
  - A mapped path that doesn't exist → **Fail** "not in model bundle <release> (try `ngiab-rs models update`)".
  - Any other missing absolute path → **Fail**. It is a Warn today.
  - For each existing library, run `ldd` with the same `LD_LIBRARY_PATH`. Any `not found` → **Fail**, listing the missing sonames. This is what catches SUMMA.
  - If `ldd` itself is missing → **Info** "dependency check skipped".
- [ ] **C.5.3 glibc** (`cfg(all(target_os="linux", target_env="gnu"))`): if `gnu_get_libc_version()` is lower than `manifest.min_glibc`, **Fail** "glibc X < 2.34 required by the model libraries; use docker or podman".
- [ ] **C.5.4 Adapter check.** Extend `config::Module` (`config.rs:14`) with `adapter: Option<String>` (the `name` field). Today `module_from` (`config.rs:90-97`) merges `model_type_name` and `name`, so they need separating.
  - Any `bmi_python` module → **Fail** "Python BMI models are not supported (bmi-driver is built without Python)". This applies to every backend now that the image has no Python.
- [ ] **C.5.5 Init-config scan.** For the first divide (`preflight.rs:223-240`):
  - A `/ngen/...` or `/dmod/...` path not covered by the bundle → **Warn**. Example: `/ngen/ngen/extern/cfe/...`.
  - When staging will rewrite files → **Info** "N config files will be rewritten for local paths".
- [ ] **C.5.6** The `output_root` Fail message at `preflight.rs:164-168` now applies to container backends only. On local, `/data/...` and `/ngen/ngen/data/...` resolve to the data folder.

### C.6 Windows port: every Unix-only call site
This is needed for the native Windows binary, which uses docker/podman only. `cargo build --target x86_64-pc-windows-msvc` must pass.

- [x] **Cargo.toml**: move `libc` to `[target.'cfg(unix)'.dependencies]`.
- [x] **`main.rs:102`**: `SIGHUP` doesn't exist on Windows. Use `#[cfg(unix)]` for the three-signal array and `#[cfg(windows)]` for `[SIGTERM, SIGINT]`.
- [x] **`backend.rs:356-375` (`find_executable`)**:
  - The `PermissionsExt` mode check becomes `#[cfg(unix)]`.
  - On Windows, try `name` plus each `PATHEXT` extension and check `is_file()`.
  - `/usr/local/bin` becomes `cfg(unix)`.
  - `~/.cargo/bin` stays on all platforms.
- [x] **`backend.rs:270-272` (`user_args`)**: `libc::getuid`/`getgid` becomes `cfg(unix)`. On Windows return `vec![]`.
- [x] **`backend.rs:323` (`libc::geteuid`)**: becomes `cfg(unix)`. On Windows use only `"rootless":true`.
- [x] **`backend.rs:290-292` (`selinux_enforcing`)**: gate with `cfg(target_os="linux")`.
- [x] **`backend.rs:71-73`**: move the in-container check into a free function, `in_container()`, which B.3 also uses. Gate it with `cfg(unix)`. _(`backend::in_container()`; returns false on non-unix)_
- [x] **`backend.rs:241-244` (local `shell_command`)**: on Windows use `%COMSPEC%`, falling back to `powershell.exe`.
- [ ] **`backend.rs:251-258` (`mount_args`)**: _(partial: `backend::strip_verbatim` turns `\\?\C:\...` into `C:\...` and `\\?\UNC\server\share` into `\\server\share`, unit-tested on every platform. Not done: checking Docker Desktop and podman machine needs Windows hardware)_
  - The Windows source is `C:\Users\...`. Check that this works with Docker Desktop and with podman machine.
  - Never pass a `\\?\` path. Use `std::path::absolute`, which `main.rs:71` and `app.rs:229` already do.
- [ ] **`preflight.rs:82-86`**: replaced by the string logic in `paths.rs` (C.2). _(partial: `host_path` now treats container paths as POSIX strings (`starts_with('/')`, component-boundary prefixes), so `/dev/null` and `/ngen/ngen/data/...` no longer map to `C:\...` on Windows; tested. Not done: the `PathMap` replacement belongs to C.2)_
- [x] **`plan.rs:30,38`**: the `/` joins are only correct for container roots. Add `debug_assert!(kind.is_container() || cfg!(unix))`. _(the assert is in `plan::command_for`, where the backend kind is known)_
- [x] **`app.rs:600-630` (`complete_path`)**: splits on `/` only. This is superseded by the picker rework in D.2. If it is kept, also split on `MAIN_SEPARATOR` and accept `~\` at `app.rs:220`. _(kept; it now splits on `/` and `MAIN_SEPARATOR` and accepts `~\`)_
- [x] **`pty.rs:352,369` tests**: mark `#[cfg(unix)]`. Add a Windows smoke test (`cmd /C echo hello`); portable-pty uses ConPTY there. _(the Windows test compiles but has not been run on Windows)_
- [x] **Platform constant**: _(`backend::LOCAL_SUPPORTED`. Exception: local probes also run inside our own image (`in_container()` and `/dmod/shared_libs` exists), because there the tools and `/dmod` are native, arm64 included. A generic container such as toolbox on an arm64 host does not qualify. Gating is in `backend::probe_local`, unit-tested)_
  ```rust
  pub const LOCAL_SUPPORTED: bool = cfg!(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"));
  ```
  - When this is false, `Detected::detect` skips the local probes.

### C.7 Availability
The installer prompt hook itself is specified once, in B.7.
- [ ] Replace `Detected::available -> bool` (`backend.rs:94-99`) with:
  ```rust
  pub enum Availability { Available, NotInstalled(String), NotRunning(String), Unsupported(String) }
  pub fn availability(&self, k: BackendKind) -> Availability
  ```
  - Keep `available()` as `matches!(…, Available)`.
  - Update all four call sites to show the reason: `main.rs:61`, `app.rs:253`, `backend.rs:103` (`default_kind`) and `ui.rs:191`.
- [ ] How each backend is classified:
  - **Local:**
    - `Unsupported("not supported on this platform; use docker or podman")` if `!LOCAL_SUPPORTED`.
    - `Unsupported("glibc too old")` when glibc is below `min_glibc`.
    - `NotInstalled` if bmi-driver, rs-route or the bundle manifest is missing. Name what is missing.
  - **docker/podman:**
    - `NotInstalled` if the binary is missing.
    - `NotRunning(note)` when `RuntimeInfo.usable == false`. This shows a "start it" hint and never offers a reinstall.
- [ ] `default_kind()` keeps the order local > docker > podman, but only picks `Available` backends. A user with bmi-driver but no bundle therefore falls through to docker instead of failing.
- [ ] `cycle_backend` (`app.rs:251`): show unavailable backends greyed out with their reason. Don't hide them.
- [ ] If only the local bundle is missing (the binaries are present), offer `ngiab-rs models update` in-process instead of the full installer.

### C.8 Ordering
1. C.6 and the `CommandSpec.env` part of C.4. These are independent; they belong in the foundation PR and unblock Windows CI.
2. The bundle from A.6 (rc models release), together with C.1.
3. C.2.
4. C.3.
5. The rest of C.4.
6. C.5.
7. C.7, then B.7.

### C.9 Acceptance criteria
- **Clean machine runs.** On a clean Ubuntu 22.04/24.04 x86_64 machine that has only `bmi-driver`, `rs-route` and `~/.ngiab/container` (no docker, no gfortran), `ngiab-rs <folder> --auto -b local` completes with no config edits for:
  - a CFE + Noah-OWP + sloth folder
  - an LSTM folder
- **Output parity.** bmi-driver CSVs and rs-route NetCDF match the docker backend's output within float tolerance.
- **No config changes.** Every file under `<data>/config/` has the same sha256 after a run, unless the mapping prompt was accepted. If it was, `realization.json.bak` exists and the new file contains container-style paths, never `~/.ngiab/...` or `.ngiab-rs/stage`.
- **Paths with spaces.** Works when the data folder and `$HOME` contain spaces.
- **Copy-pasteable command.** The echoed command can be pasted into a shell and runs:
  ```sh
  LD_LIBRARY_PATH=… bmi-driver <data> --config <data>/.ngiab-rs/stage/realization.json -j N
  ```
- **Clean failures.** Each of these produces one clear preflight **Fail** line, not a crash:
  - missing bundle
  - missing library
  - `ldd` not-found (e.g. SUMMA)
  - glibc too old
  - `bmi_python`
- **Cleaning outputs** never touches `.ngiab-rs/`.
- **Other platforms.** Windows and macOS builds compile, and `--backend local` reports "not supported on this platform".
- **Tests.** `cargo test` passes on all release targets.

### C.10 Risks
- **Staging cost.** Domains with around 1M catchment files make staging slow. bmi-driver could later accept a path-prefix map for `library_file`, but init files would still need rewriting.
- **Unquoted YAML/namelist values.** If `$HOME` contains `": "` or `" #"`, the text rewrite can break the file. This plan detects it and warns. A possible fallback is a symlink root without spaces, such as `/tmp/ngiab-rs-<uid>`.
- **Upstream layout.** We rely on rust-lstm accepting a relative `run_dir` (verify against the shipped `.so`) and on `awiciroh/ngiab` keeping its `/dmod` layout. The manifest records `source_digest`.
- **Write-back reverse mapping.** If bmi-driver reformats the whole JSON on write-back, key order and whitespace in the user's file will change. That is acceptable, and docker runs behave the same.

---

## D. TUI improvements and evaluation (notes.md)

(Within this part, "section N" means D.N.)

Scope: every item in `notes.md`. Install, CI, model files and the local backend are covered in other sections. Shared pieces: the HTTP client `src/net.rs` (B.3, lands with the CLI restructure), `home::preprocessor_file()` (B.1), the 'suspend TUI → run → resume' helper in `main.rs`, and `RunArgs` (B.2) for new flags such as `--no-mouse`.

Facts checked against the code and the live environment, which the plan relies on:

- **Path prompt.** It is pre-filled only from `State.last_path` (`app.rs:146-158`). `State` is stored at `dirs::config_dir()/ngiab-rs/state.json` (`config.rs:113-140`).
- **`~/.ngiab/preprocessor`.** This is a plain-text file. NGIAB_data_preprocess (`modules/data_processing/file_paths.py`) reads it with `Path(f.readline().strip()).expanduser()` and writes it with `f.write(str(working_dir))`, with no trailing newline guaranteed. The path in it is the **parent** folder that holds one subfolder per run (for example `~/ngiab_preprocess_output/00_demo/`). It is not a data folder itself. The local copy contains `/home/josh/ngiab_preprocess_output\n`.
- **Event loop.** `main.rs:127-138` handles `Event::Key` only. The drain loop also throws away every non-key event.
- **Footer.** `ui.rs:93-134` draws every key hint in the same style. The Run screen footer just swaps to a different list when the run finishes.
- **Chart.** `ui.rs:409-515` draws one `Dataset`. The x bounds are always the full series and `legend_position(None)` is set. `Series::decimated` (`eval.rs:50`) always works on the whole series.
- **Eval list filter.** It is a case-insensitive substring match (`eval.rs:364-370`).
- **Eval state is thrown away.** `App::finish_run` (`app.rs:423-436`) and `begin_run` (`app.rs:324`) both set `self.eval = None`. Any pins or zoom would be lost after a rerun.
- **Gage table.** `config/*.gpkg` has the table `"flowpath-attributes"` with `id TEXT` (for example `wb-2613110`) and `gage TEXT` (for example `09210000`, leading zero kept). Most rows have no gage. Preflight already finds the gpkg (`preflight.rs:206`, `pf.gpkg`) and opens it read-only with rusqlite (`preflight.rs:305`).
- **Existing `eval/` folders.** At least one data folder (`00_demo/eval/`) already has an `eval/` folder made by the existing Python NGIAB evaluation tool. It contains `streamflow.db`, `json/gage-*_results.json`, `usgs_cache/usgs_{gage}_{YYYYmmddHH}_{YYYYmmddHH}.csv` (columns `,Time,values`, hourly, m³/s), `nwm_cache/*.nc` and `debug/`. ngiab-rs must not overwrite these files.
- **Python tool's metrics.** Its `json/*_results.json` reports `kge{kge,r,a,b}`, `nse`, `pbias`. Its pbias sign is `100·(Σo−Σs)/Σo`: b=1.55 gives pbias −55.4.
- **USGS API.** `api.waterdata.usgs.gov/ogcapi/v1/collections/continuous/items` is live (api-version 1.19.0). It returned 2008 15-minute data for USGS-09210500 in ft³/s with `approval_status` and `qualifier` columns. One water year was 24,509 rows in a single response with `limit=50000`. `.../daily/items` with `statistic_id=00003` also works. The v0 URL still answers, but its response links point to v1.
- **Legacy service shutdown.** USGS says legacy `waterservices.usgs.gov` gets a 2 s delay per request from 2026-11-16, 10 s from 2027-01-11, and is redirected away on 2027-02-22. Do not build on it.

---

### D.0 Shared groundwork (do first)

- [ ] **Hit map for mouse support.** Add `src/hit.rs` with `pub enum Target`:
  - `MenuItem(usize)`
  - `Backend(BackendKind)`
  - `FooterKey(KeyCode)`
  - `EvalTab(Tab)`
  - `EvalVar(usize)`
  - `EvalRow(usize)`, an index into `visible`
  - `PickerRow(usize)`
  - `BrowserRow(usize)`
  - `RunStep(usize)`
  - `PopupKey(KeyCode)`
  - `ChartPlot`
  - `ChartList`
  - `RunTerm`

  Also add `pub struct HitMap(Vec<(Rect, Target)>)` with `clear()`, `push()` and `at(col,row) -> Option<Target>`, where the topmost entry wins: search in reverse so popups beat the screen underneath.
- [ ] **Fill the hit map while drawing.** Add `pub hit: HitMap` to `App`. Clear it at the top of `ui::draw` (`ui.rs:22`). `draw` already takes `&mut App`. Change `draw_home`, `draw_eval`, `draw_footer` and `draw_popup` to take `&mut App` or return their hits.
- [ ] **Mouse capture in `main.rs::run_tui`:**
  - After `ratatui::init()`, run `execute!(stdout(), EnableMouseCapture)`, using `ratatui::crossterm::{execute, event::{EnableMouseCapture, DisableMouseCapture}}`.
  - Disable capture before every `ratatui::restore()`: at the normal exit, in the shared suspend/resume helper that replaces the `want_shell` block at `main.rs:140-147` (re-enable after `ratatui::init()`), and in a panic hook. `ratatui::init`'s hook does not turn mouse capture off, so chain `std::panic::take_hook()`.
- [ ] **Event loop.** Change it to `match event::read()? { Event::Key(k) => app.on_key(k), Event::Mouse(m) => app.on_mouse(m), _ => {} }` in both the first read and the drain loop (`main.rs:127-138`). In the drain loop, merge runs of `MouseEventKind::Drag` and keep only the last one.
- [ ] **CLI and toggle.** Add a `--no-mouse` flag (env `NGIAB_RS_NO_MOUSE`) and an `m` key on Home that turns capture on and off. Mouse capture breaks normal terminal text selection; users must hold Shift (or Option on macOS) to select. Mention this in the Help popup (`ui.rs:558-576`).
- [ ] **Fuzzy matching dependency.** Add `nucleo-matcher = "0.3"`: synchronous, no thread pool, the matcher used by Helix. Recommended over `fuzzy-matcher`, which gets few updates. Add `src/fuzzy.rs` with `pub fn rank<'a>(query: &str, items: impl Iterator<Item=(usize, &'a str)>) -> Vec<(usize, u32 /*score*/, Vec<u32> /*match idx*/)>`, using `Pattern::parse(query, CaseMatching::Smart, Normalization::Smart)`. Both the folder picker and the eval list filter use it.
- **Acceptance criteria:** mouse events arrive in `App::on_mouse`. After quitting, opening a shell, or a panic, the terminal does not print `^[[<0;..M` junk.

---

### D.1 Default data path and remembered paths

**Precedence for the path the TUI starts with:**

1. The CLI `path` argument. If it is a valid folder, open Home directly, as now (`app.rs:171-174`).
2. Inside a container, `/data` (already `main.rs:56-59`).
3. `State.last_path`, but only if it still exists and contains `config/`. The prompt is pre-filled with it, so pressing Enter reopens the last folder.
4. Otherwise, the base folder followed by `/`. The picker (section 2) then lists its child folders straight away.

**Base folder** (the root the picker searches), resolved in this order:

1. The first line of `~/.ngiab/preprocessor` with whitespace trimmed and `~` expanded, used only if it is a folder.
2. `~/ngiab_preprocess_output/`, if it exists.
3. The current working directory.

Rationale: the preprocessor file names the folder that holds the runs, not a single run. "Last used" is more specific, so it wins for the pre-fill. The preprocessor folder decides where the browse list starts.

- [ ] Add `pub fn preprocessor_dir() -> Option<PathBuf>` and `pub fn base_dir() -> PathBuf` to `config.rs`. Use `home::preprocessor_file()` (B.1; always `~/.ngiab/preprocessor`, ignores `NGIAB_HOME`) and read only the first line. A missing, empty or bad file is ignored quietly. This works on Windows too, as `%USERPROFILE%\.ngiab\preprocessor`.
- [ ] Extend `State` (`config.rs:113`) with `#[serde(default)] pub recent: Vec<PathBuf>`: most recent first, at most 20 entries, no duplicates, pruned of folders that no longer exist when loaded. The old `{"last_path": ...}` format must still load. `submit_path` (`app.rs:233`) currently writes `State { last_path: Some(p) }.save()`, which would wipe any new fields. Change it to load, update and save.
- [ ] Add `App.base_dir: PathBuf`. Set it in `App::new` and show it as a dim hint under the prompt in `draw_path_input` (`ui.rs:138`), for example "searching in ~/ngiab_preprocess_output (from ~/.ngiab/preprocessor)".
- [ ] Move `~` expansion into one helper, `config::expand_tilde`. It is currently duplicated in `app.rs:220` and `app.rs:606`.
- **Acceptance criteria:**
  - No state file plus a preprocessor file means the prompt shows `<preprocessor dir>/` and the picker lists its runs.
  - A stale `last_path` falls back to the base folder.
  - The CLI argument always wins.
  - Unit tests cover a preprocessor file with a trailing newline, `~`, a missing file, and a path that does not exist.

### D.2 fzf-style dropdown on the folder prompt

- [ ] **New module `src/picker.rs`** with `pub struct Picker { candidates: Vec<Candidate>, matches: Vec<(usize, Vec<u32>)>, sel: usize, scan_root: PathBuf }`. Each `Candidate { path: PathBuf, display: String, kind: Recent | Child | Nested, is_data_dir: bool, mtime: SystemTime, has_outputs: bool }`.
- [ ] **Candidate sources, refreshed when the folder part of the input changes:**
  - (a) `State.recent`.
  - (b) Subfolders of the folder part of the typed text. With nothing typed, use the base folder.
  - (c) Folders one level further down from (b), so typing `provo` finds `~/ngiab_preprocess_output/7_year_provo`.

  Mark `is_data_dir` when `config/realization.json` exists. Cap the scan at about 2,000 entries, skip hidden folders unless the query starts with `.`, and never follow symlinks more than one level down.
- [ ] **Matching:**
  - Query = the text after the last `/`, matched with `fuzzy::rank` against the folder name. Recent entries are matched against the full path.
  - With an empty query, sort recent entries first, then by `mtime`, newest first.
  - When scores tie, data folders come first.
- [ ] **Keys in `on_path_key` (`app.rs:578`):**
  - `Up`/`Down` and `Ctrl-p`/`Ctrl-n` move the selection.
  - `Tab` accepts the highlighted entry into the input and adds `/`, replacing the current shell-style `complete_path`, which stays as the fallback when there are no matches.
  - `Enter` on a highlighted data folder submits it. On a folder that is not a data folder, it moves into that folder. With nothing highlighted, the typed path is submitted as now.
  - `Ctrl-b` or `F2` opens the browser (section 3).
- [ ] **UI (`draw_path_input`, `ui.rs:138-156`):**
  - Give the box `Constraint::Fill` height and put a list under the input line of about 12 rows or whatever fits.
  - Matched characters are bold or accent-coloured, using the match indices.
  - Data folders get a green ●. Other folders are dim and end in `/`.
  - Show an "outputs" tag when `outputs/troute/*.nc` exists, plus a relative mtime such as "3d ago".
  - Add a final row "📁 Browse…", which opens the browser.
- [ ] **Mouse:** clicking a row selects it, double-clicking (two clicks on the same row within 400 ms, tracked in `App.last_click`) works like Enter, and the scroll wheel moves the selection.
- **Acceptance criteria:**
  - Typing `7yr` ranks `7_year_provo*` first.
  - Each keypress responds in under 5 ms with 2,000 candidates. Fuzzy ranking runs on every keypress, while the folder scan only runs when the folder part changes.

### D.3 Browse option (native dialog + in-TUI navigator)

- [ ] **In-TUI browser** (always available; the fallback whenever no native dialog can be shown). Add `Screen::Browse` and `src/browser.rs` with `BrowserState { cwd: PathBuf, entries: Vec<Entry>, sel: usize, show_hidden: bool }`.
  - Folders only, plus a `..` entry. Data folders get a ● marker, and the preview pane on the right shows `config/`, `forcings/` and `outputs/` with counts (reuse the bullet styling from `ui.rs:144-146`).
  - Keys: `Enter`/`→`/`l` go into the folder, `←`/`h`/`Backspace` go up, `.` shows hidden folders, `/` filters fuzzily within the current folder, `s` or `Space` selects the current folder (or the highlighted one) and submits it, `~` goes home, `b` goes to the base folder, `Esc` returns to the path prompt.
  - On Windows, show a drive list when going up from a drive root (`C:\`).
  - Mouse: click selects, double-click enters, the wheel scrolls.
- [ ] **Native dialog (the user wants this).** Add `rfd` and turn it on in release builds. Behind a default-on Cargo feature `native-dialog = ["dep:rfd"]`, so it can be disabled.
  - On Linux, use rfd's **xdg-desktop-portal** backend, not GTK3, so the release binary gains no GTK dynamic library dependency. Add `ldd` to the CI link check to confirm.
  - Show "Browse…" as a native dialog only when a display is available:
    - Linux: `$DISPLAY` or `$WAYLAND_DISPLAY` is set.
    - macOS and Windows: always.
    - Never when `detected.in_container` or `$SSH_CONNECTION` is set.
  - Call `rfd::FileDialog::new().set_directory(base).pick_folder()` through the shared "suspend TUI → run → resume" helper (the `want_shell` pattern, `main.rs:140-147`).
  - Fall back to the in-TUI browser if no display is available, if no portal is available, or if the dialog returns an error. Cancelling (`None`) goes back to the path prompt.
- **Acceptance criteria:** the browser works over SSH and inside the container. Selecting a folder sends it through `submit_path`.

### D.4 Highlight the bottom menu options after a run completes

Confirmed by the user: "bottom menu" means the footer key hints. While a run is going, the Run screen footer only shows `ctrl-c`, `scroll` and `tab`, and its keys look the same as on every other screen. After the run finishes, the next actions (`e` evaluate, `q` home, `r` rerun, `s` shell) should stand out.

- [ ] Change the footer key list in `draw_footer` (`ui.rs:94-123`) from `&[(&str,&str)]` to `Vec<FooterKey { key: &str, label: &str, code: KeyCode, state: Disabled | Normal | Highlight }>`.
- [ ] **Run screen:** always list the same keys, so the layout does not jump: `e evaluate`, `q home`, `r rerun`, `ctrl-c cancel`, `pgup/pgdn`, `tab`.
  - While running, `e`, `q` and `r` are `Disabled` (dark gray text, no background) and `ctrl-c` is normal.
  - When finished, `ctrl-c` is disabled. If the run succeeded, `e` and `q` are `Highlight` (black on green, bold). If it failed, they are black on yellow and `r` is normal.
  - Add `r` = rerun the same plan to `on_run_key` (`app.rs:687-695`), going through `request_run` so the cleanup popup still appears. It must only fire when `run.finished`: while a run is active, keys are forwarded to the PTY (`app.rs:697-709`).
- [ ] **Home screen.** Dim the `e`/Evaluate entry and the "Evaluate outputs" menu item when there are no outputs: `preflight.existing_output_files() == 0` and no `outputs/troute/*.nc`. After a successful run, give "Evaluate outputs" a green ● and bold text until it is opened. Track this with `App.fresh_outputs: bool`, set in `finish_run` and cleared in `open_eval`.
- [ ] **Clickable footer.** Clicking a footer key builds the matching `KeyEvent` and sends it through `on_key`. Disabled keys ignore clicks. Record a `Target::FooterKey` hit for each span using the width already added up.
- **Acceptance criteria:** while running, the footer shows `e` dim. When the run finishes successfully it turns green within one tick, and clicking it opens Eval.

### D.5 Mouse support across screens (on top of section 0)

- [ ] **`App::on_mouse(MouseEvent)`.** If a popup is open, only popup targets work, and clicking outside a `Message`/`Help` popup closes it. Otherwise it dispatches by `self.hit.at(col,row)` and the current screen.
- [ ] **Home:**
  - Clicking a menu row sets `menu_idx`. Clicking the already selected row activates it.
  - Clicking a backend line selects that backend (add `App::set_backend(kind)` taken out of `cycle_backend`, `app.rs:251`).
  - The wheel moves through the menu.
- [ ] **Run:**
  - Clicking a step row sets `run.view`.
  - The wheel over the terminal pane calls `scroll_view(run, ±3)` (`app.rs:762`).
  - Mouse events are never forwarded to the PTY.
- [ ] **Eval:**
  - Clicking the "Hydrograph"/"Catchment" titles switches tab. Record each title span's x range: the block title starts at `list_area.x + 1`.
  - Clicking a variable chip sets `var_idx`/`col_idx`. Chip widths are known in `draw_chart`, `ui.rs:439-444`.
  - Clicking a list row selects it. The row index is `start + (row - inner.y)`. Save `start` from `ui.rs:382` into `EvalState.list_start` while drawing.
  - The wheel over the list moves the selection by ±3. The wheel over the chart zooms (section 6).
- [ ] **Popups.** The `[d]`, `[k]`, `[esc]`, `[y]` and `[n]` labels in `draw_popup` (`ui.rs:525-595`) become `Target::PopupKey`. This needs the popup's inner rect plus line offsets. The simplest way is to have `draw_popup` record the rect of each key span as it lays out the lines.
- **Acceptance criteria:** every action reachable with the keyboard on Home, Run (once finished) and Eval can also be done by clicking. Keyboard behaviour does not change.

### D.6 Chart rework: zoom, time window selection, keyboard pan

Ratatui `Chart` limitation: the inner plot rect, after y-label width and the x-axis rows are subtracted, is computed privately inside `Chart::render` and is not exposed. Mapping mouse x to time therefore needs a plot rect whose position is known.

- [ ] **Refactor `draw_chart` (`ui.rs:409-515`) so the plot rect is known:**
  - Split `chart_area` (inside the block) into `[y_labels (width = max label width + 1) | plot]` above `[x_labels row]`.
  - Draw the y and x labels as `Paragraph`s.
  - Draw `Chart` into `plot` with `Axis::default().bounds(..)` and **no labels**. No labels means no axis lines and no reserved space, so the canvas is exactly `plot`.
  - Save `plot` into `EvalState.plot_rect` and the hit map (`Target::ChartPlot`). Optionally draw a `│` axis line with a `Block` border on the left and bottom.
- [ ] **View state in `EvalState`:**
  - `view: Option<(i64,i64)>`. `None` means the full extent, which is the union of the t-ranges of all shown series.
  - `drag: Option<(u16 /*start col*/, u16 /*cur col*/)>`.
  - `view` is kept when the item or variable changes, so the same window can be compared across reaches. It is reset by `0`/`Esc` and when the tab changes.
- [ ] **Mapping helpers in `eval.rs`:**
  - `col_to_t(col, plot, (t0,t1)) = t0 + ((col - plot.x) as f64 + 0.5) / plot.width as f64 * (t1 - t0)`, with the inverse `t_to_col`.
  - Clamp `view` to the full extent. Minimum span = `max(2 * output_interval, 3600 * 6)` seconds.
- [ ] **Scroll to zoom.**
  - `ScrollUp` over the plot zooms in by 0.8× and `ScrollDown` zooms out by 1.25×. The time under the cursor stays fixed: `new_t0 = tc - (tc - t0) * f`, `new_t1 = tc + (t1 - tc) * f`.
  - Zooming out past the full extent sets `view = None`.
  - Shift+wheel, or the horizontal wheel `ScrollLeft`/`ScrollRight`, pans by 10% of the span.
- [ ] **Click and drag to select.**
  - `Down(Left)` inside the plot starts the drag, `Drag(Left)` updates it, and `Up(Left)` with `|dx| >= 2` cols sets `view` to the selected range. A smaller movement counts as a click and shows the time and value at that column as a dim crosshair line in the info panel.
  - Draw the selection while dragging after the chart renders, by restyling the plot's buffer cells between the two columns with `f.buffer_mut()[(x,y)].set_bg(Color::Rgb(40,40,70))`.
  - `Right` click or `Esc` resets the zoom.
- [ ] **Keyboard equivalents in `on_eval_key` (`app.rs:712-760`):**
  - `z`/`Z` zoom in/out around the centre.
  - `[`/`]` pan left/right.
  - `0` resets.
  - `←`/`→` stay as variable cycling.
- [ ] **Decimating a range.** Add `Series::decimated_range(&self, t0: i64, t1: i64, buckets) -> Vec<(f64,f64)>`. `t` is sorted, so it can use `partition_point` and keep one point on each side so lines reach the edges. Rewrite `decimated` to call it. Y bounds are recalculated from **visible** points only (min/max over all shown datasets in the window).
- [ ] **X labels adapt to span:** `%Y-%m-%d` when span > 3 days, `%m-%d %H:%M` when span ≥ 6 h, otherwise `%H:%M`. Use 3-5 labels depending on `plot.width / 14`. The chart title shows `"<from> → <to> (zoomed, 0 to reset)"` when `view.is_some()`.
- [ ] Stats in the info panel (`ui.rs:449-461`) show the full series. When zoomed, add the window's min, mean and max on a second line (same rule as section 9: only when the values differ).
- **Acceptance criteria:**
  - Wheel-zooming on a peak keeps the peak under the cursor.
  - Dragging across an event zooms to exactly the dragged columns, within ±1 column of time.
  - Unit tests cover `col_to_t`/`t_to_col` round trips, `decimated_range` boundaries, and clamping.

### D.7 Pinning hydrographs and catchment series (Space)

- [ ] **State in `EvalState`:**
  - `pins: Vec<Pin>`, where `Pin { key: PinKey, label: String, series: Series, color: Color }` and `PinKey { tab: Tab, nc_idx: usize, item: usize, var: String }`. `var` is stored by name, not index, so a pin stays valid after a CSV column clamp.
  - Each pin stores its own loaded `Series`, so it stays fixed while browsing.
- [ ] **Space** (non-filtering mode only, in `on_eval_key` `app.rs:712-760`) toggles a pin for the current selection and variable. **`c`** clears all pins. **`P`** cycles a "pins only" mode that hides the unpinned current selection.
- [ ] **Colours:** a fixed palette of 8 colours that are easy to tell apart and not cyan. Cyan stays for the current unpinned selection. Suggested: `Yellow, Magenta, LightGreen, LightRed, LightBlue, Rgb(255,165,0), LightMagenta, White`. A pin gets the first colour no other pin is using. Limit to 8 pins and flash "max 8 pinned".
- [ ] **Chart:**
  - One `Dataset` per pin plus the current selection, if it is not pinned. All are cut to the view window.
  - Turn on the legend (`legend_position(Some(LegendPosition::TopRight))` and `hidden_legend_constraints` so it hides when the chart is small) when more than one dataset is drawn.
  - Braille cells keep the colour of the last dataset drawn, so draw the current selection last, to keep it on top.
- [ ] **Units:** if pinned series have different `units` or variable names (for example flow and depth, or a hydrograph and a catchment CSV), show a yellow warning in the info panel: "mixed units: y-axis shared". Pins from both tabs are allowed and stay visible on either tab.
- [ ] **List:** pinned rows show a ● in their pin colour before the label. Add this in the `ListItem` builder at `ui.rs:383-389`, looking up `pins` by `(tab, nc_idx, item)`.
- [ ] **Keep pins across reruns.** `finish_run`/`begin_run` set `eval = None` (`app.rs:324`, `app.rs:435`). Save pin keys and `view` into `App.eval_carry` and restore them in `open_eval`, reloading each pinned series from the new outputs. Drop pins whose item no longer exists.
- **Acceptance criteria:** pin three reaches, switch to the Catchment tab, and the three lines plus the current catchment are still drawn in different colours with a legend. Zooming applies to all of them.

### D.8 USGS gage observations

**8a. Shared HTTP client (defined in B.3)**

- [x] Reuse `src/net.rs` from B.3 (`ureq = { version = "3", default-features = false, features = ["rustls","json","gzip"] }`; rustls, no OpenSSL). USGS-specific needs, to be folded into that one module rather than a second client:
  - `pub fn agent() -> ureq::Agent`, built once (`OnceLock`), with `user_agent("ngiab-rs/{CARGO_PKG_VERSION}")`, `timeout_connect(10s)`, `timeout_global(120s)`, and `http_status_as_error(false)` so 429 and 5xx responses can be checked.
  - Downloads use B.3's `download(url, dest, progress)` (`.part` then rename). Do not add a second `get_to_file`.

**8b. Gage to wb- mapping**

- [ ] In the new `src/obs.rs`: `pub fn gage_map(gpkg: &Path) -> Result<HashMap<i64 /*wb id*/, String /*gage*/>>`.
  - SQL: `SELECT id, gage FROM "flowpath-attributes" WHERE gage IS NOT NULL AND trim(gage) != ''`.
  - If that table is missing, retry with `flowpath_attributes` (older hydrofabric).
  - Strip `wb-` and parse the number to match `TrouteFile.feature_ids` (`i64`).
  - Keep the gage as text, with leading zeros. Accept only `^[0-9]{8,15}$` and log the rest, which are non-USGS (for example Canadian `08NM…`) or malformed.
  - Open read-only, as in `preflight.rs:305`. Load once in `EvalState::new` from `preflight.gpkg`, passed in by `App::open_eval` (`app.rs:503`).
- [ ] **List integration:**
  - `item_label` (`eval.rs:345`) becomes `wb-2613116 ⚑09210500` for gaged reaches, so fuzzy filtering by gage number works.
  - `a` toggles "gaged only" (filters `visible`). The list title shows `"N gaged"`.
  - The fuzzy matching from section 0 replaces the substring `refilter` (`eval.rs:364`).

**8c. Fetching**

- **API.** `https://api.waterdata.usgs.gov/ogcapi/v1/collections/continuous/items?f=csv&monitoring_location_id=USGS-{gage}&parameter_code=00060&time={start}Z/{end}Z&limit=50000&properties=time,value,unit_of_measure,approval_status,qualifier&skipGeometry=true`. Fall back to `collections/daily/items` with `statistic_id=00003` when the continuous query returns 0 rows for the whole window. Instantaneous data is thin before about 2007 at many sites.
- [ ] **Paging and chunking.**
  - Fetch in **one request per calendar year** (about 35k rows at 15 minutes, under the 50k limit).
  - Still follow `rel="next"` links. CSV responses carry them in a `Link` header. If that header is missing, use `f=json` and its `links[]`.
  - Retry 429 and 5xx up to 3 times with backoff, respecting `Retry-After`.
  - Send `X-Api-Key` when `$API_USGS_PAT` is set (the variable the dataretrieval packages use) or `$NGIAB_RS_USGS_API_KEY` is set. Requests without a key work but have lower per-IP hourly limits. Show a hint about the key on a 429.
- [ ] **Window.** Use `[min(troute.times) - 1h, max(troute.times)]` in UTC, extended to whole years for chunking. Model times from `TrouteFile::open` are already treated as UTC (`eval.rs:148`).
- [ ] **Run in the background.** Use `std::thread::spawn` plus an `mpsc` channel kept in `EvalState.obs_rx`. `App::tick` (`app.rs:461`) drains it. The UI must never block. While loading, show a spinner in the info panel: "fetching USGS-09210500 2009 (2/6)…".
- [ ] **When to fetch.** Fetch automatically the first time a gaged reach is selected, which is one gage at a time and cheap. `u` refetches that gage and ignores the cache. `U` fetches all gages in the domain one after another (with a confirmation popup when there are more than 20).

**8d. Processing**

- [ ] **Units.** Check `unit_of_measure`. `ft^3/s` × 0.028316846592 gives m³/s. Treat values that are empty or that the qualifier marks as non-numeric (`Ice`, `Eqp`, `Ssn`, `Dis`, …) as NaN. Keep `approval_status` so a "provisional" badge can be shown when any value in the window is `Provisional`.
- [ ] **Hourly alignment.** Resample the instantaneous series to the model's time steps by taking the **mean of observations in (t − Δ, t]**, where Δ = the model output interval (`Realization.output_interval`, default 3600). This matches a period-mean model output stamped at the end of the period. The existing Python tool's debug CSV shows NGEN values stamped from `01:00`. When there are no observations in the interval, the value is NaN.
- [ ] **Daily fallback.** Compare against the model's daily mean, using UTC days. Note that USGS daily values use **local** standard-time days, so there is up to a few hours of offset. Show a "daily (local day)" badge.

**8e. Cache**

New folder `<data>/eval/ngiab-rs/usgs/`. It is a subfolder so it does not collide with the Python tool's `eval/usgs_cache`, `streamflow.db` and so on.

- [ ] Raw chunk files `USGS-{gage}/continuous_{YYYY}.csv` with columns `time_utc_unix,value_m3s,approval,qualifier`, plus `USGS-{gage}/meta.json`: `{ "api": "...v1/continuous", "fetched": {"2009": "2026-11-23T21:14:19Z", ...}, "unit_src": "ft^3/s" }`.
- [ ] **Cache rules:**
  - A year is fresh if it ended before the fetch time **and** every row was `Approved`.
  - Otherwise, refetch after 24 h.
  - An empty year is cached as a 0-row file, so known gaps are not fetched again.
- [ ] **Read-only fallback.** If nothing is cached and the network fails, read `eval/usgs_cache/usgs_{gage}_*.csv` from the Python tool (`,Time,values`, hourly m³/s, naive UTC) when one covers the window.
- [ ] **Offline.** On a network error, use whatever is cached and flash "offline: showing cached USGS data" or "offline: no cached data for USGS-…". Never fail the eval screen.
- [ ] Create `eval/` on demand only. Add `eval/` and `.ngiab-rs/` (C.3.1 staging) to the list of things preflight leaves alone. It is not an output folder, so the cleanup popup must not delete it.

**8f. Display**

- [ ] Draw the observed series as its own `Dataset` (`Color::Gray` with `Marker::Dot`, or a dashed look using `GraphType::Scatter`), named "USGS 09210500". `o` toggles it.
- [ ] Observations only apply to the `flow` variable. For velocity, depth or catchment series, show "obs: flow only".
- [ ] Pinned gaged reaches also get their own observation line (a dimmed version of the pin colour) when "obs for pins" is on (`O`). Off by default, to keep the chart readable.

### D.9 Metrics (KGE, NSE and others)

- [ ] **New `src/metrics.rs`:**
  - `pub struct Metrics { n: usize, coverage: f64, nse: f64, kge: f64, r: f64, alpha: f64, beta: f64, pbias: f64, rmse: f64, log_nse: Option<f64> }`
  - `pub fn compute(sim: &[f64], obs: &[f64]) -> Option<Metrics>`, using pairs where both values are finite.
  - Return `None` if there are fewer than 24 pairs (or 3 for daily) or the observations have zero variance.
- [ ] **Formulas (state them in the Help popup):**
  - NSE = 1 − Σ(s−o)² / Σ(o−ō)².
  - KGE (Gupta 2009) = 1 − √((r−1)² + (α−1)² + (β−1)²), where α = σs/σo and β = μs/μo. Reporting KGE′ (2012, using the variability ratio of CVs) could be a later option.
  - PBIAS = 100·Σ(o−s)/Σo. This is the same sign convention as the existing Python tool's `results.json`: positive means the model underestimates.
  - RMSE in m³/s.
  - Pearson r.
  - Optional log-NSE with ε = 0.01·ō, for low flows.
- [ ] **Pairing.** `align(sim: &Series, obs_hourly: &Series) -> (Vec<f64>, Vec<f64>)`, joined on exact timestamps (both are on the model grid after section 8d).
- [ ] **Display.** One line in the info panel (enlarge `info_area` from `Length(4)` to `Length(6)`, `ui.rs:410`): `KGE 0.66 (r 0.89 α 1.00 β 1.32)  NSE 0.73  PBIAS −32.2%  RMSE 1.4  n 52,560 (98%)`. Colour KGE and NSE green when ≥ 0.5, yellow when ≥ 0, red when < 0.
- [ ] **Zoomed metrics.** When `view.is_some()`, also compute metrics on the window's pairs. Show a second line, "window", **only if** the window covers fewer pairs than the full range **and** at least one shown value differs after formatting to 2 decimals. Recompute only when `view`, the selection or the observations change (cache by key). Do not recompute on every frame.
- [ ] **Export.** `x` writes `<data>/eval/ngiab-rs/metrics.csv` (`wb_id,gage,var,kge,r,alpha,beta,nse,pbias,rmse,n,coverage,window_start,window_end`). Combined with `U`, this also gives a per-gage summary table.
- [ ] **Unit tests:** a perfect match gives NSE = KGE = 1 and PBIAS = 0. Sim = 2×obs gives β = 2 and α = 2. Constant observations give `None`. Vectors with many NaNs are paired correctly. Check the numbers against `00_demo/eval/json/gage-09210500_results.json`, allowing for that tool's resampling.

---

### Ordering and dependencies

1. **Section 0** (hit map, mouse capture, `fuzzy.rs`). Everything else depends on it.
2. **Section 1**, then **section 2**, then **section 3**. The picker needs `base_dir` and `recent`, and the browser is opened from the picker.
3. **Section 4.** Independent and small; can be done alongside step 2.
4. **Section 6** (chart refactor with a known plot rect), then **section 7** (pins), then **section 5**'s Eval mouse parts. Zoom and pins both change the dataset-building code in `draw_chart`, so do the refactor once, first.
5. **Section 8a**: needs `net.rs`, which lands with the CLI restructure (B.2/B.3).
6. **Section 8b**, then **8c to 8e**, then **8f**, then **section 9**.

Suggested PR split: (0+1+2+3), (4+5), (6+7), (8a), (8b to 8f + 9).

### Risks / open questions

- **Picker scan depth.** Child folders plus one level deeper under the base folder. Is that enough, or do some runs sit deeper (for example `output/<project>/<run>`)? USER: that's enough
- **Native dialog on Linux needs xdg-desktop-portal** (a running portal service, normal on GNOME/KDE desktops). If none is reachable, the dialog errors and the in-TUI browser is used. Verify on a minimal window-manager setup. USER: try it and don't worry about verification for now as long as the fallback works
- **Observation resampling.** The plan uses the mean of instantaneous values over (t−1h, t]. The alternative is the instantaneous value nearest the hour (closer to what hydrotools gives). Results differ slightly at flashy sites. USER: let the user toggle between the two
- **KGE variant.** 2009 KGE (matches the existing Python tool) or KGE′ 2012. The plan uses 2009 and shows r, α and β.
- **NWM retrospective overlay.** The Python tool caches `eval/nwm_cache/*.nc`. Showing it as an extra comparison line is cheap if the file exists. In scope? USER: yes, fetching the nwm output would also be good too if it's not too much extra work to pull output from the zarr archive
- **USGS API stability.** v1 is the current version (v0 links redirect to it). Paging limits and rate limits for unauthenticated requests are not clearly documented. Requests without a key may get 429s when fetching many gages (`U`). Handle 429 properly and document the `API_USGS_PAT` key. USER: I think the it's something like 100000 max rows of data from the api per request. fd for usgs in /mnt/raid/code/JoshCu/datastream_viewer to find more example api usage. 
- **Daily fallback.** USGS daily values use local days, which can shift peaks a little. Metrics will be labelled "daily".
- **Mouse capture** stops normal copy/paste selection in the terminal (Shift-drag works in most terminals). `--no-mouse` and the `m` toggle cover this. Windows Terminal and conhost both report mouse events through crossterm, but confirm on the Windows build. USER: don't spend too much time on the windows build
- **Braille overlay colours.** When lines cross, a cell takes the colour of the last dataset drawn. With 8 pins plus observations, overlapping parts can be hard to read. This is a ratatui limitation; the cap of 8 pins and the draw order keep it manageable.
- **Rerun keeps pins and zoom.** Pins and `view` are carried across a rerun by key (section 7). Pins on reaches that no longer exist are dropped quietly.

---

## Appendix: original requirements (verbatim)

### install+ci.md

```text
can you also add some setup glue that will copy the required files out of the ngiab-rs image into ~/.ngiab/container/ and then match the /ngen/ngen/extern etc and /dmod/ paths. Then when running temporarily mount those or update the config files so that people don't have to
I want an install.sh install.ps1 style install for this tool. e.g.
# On macOS and Linux.
curl -LsSf https://astral.sh/uv/install.sh | sh
# On Windows.
powershell -ExecutionPolicy ByPass -c "irm https://astral.sh/uv/install.ps1 | iex"
add the install commands to the top of the readme.
for this to work I need some CI that will crosscompile this for multiple platforms and add it to the releases page.
ngiab-rs self update should update ngiab-rs
ngiab-rs models update should update the ~/.ngiab/container/ files and place the old ones into ~/.ngiab/container.bak
ngiab-rs models revert should undo the last update and revert to the ~/.ngiab/container.bak files
if one or more of the backends isn't supported then it should ask and then rerun the install.sh.
as well as downloading ngiab-rs, the install.sh should give people options for backends to install and details of what it will do. local will require the rust toolchain to be installed and will compile bmi-driver and rs_route. it will then copy the model files out of the ngiab-rs image. docker will check for and optionally install docker, the same for podman.
```

### notes.md

```text
default path if not set, read from ~/.ngiab/preprocessor, if not present ~/ngiab_preprocess_output/ 
fzf style dropdown on folder selection
add a "browse" button/option open a file navigator to find a folder
after run is complete, light up the menu options on the bottom.
add mouse support
add zoom and time range selection to graphs
add scroll to zoom to graph
add click and drag to select time window on graph
let me press space to select a hydrograph/catchment and toggle it as pinned on so I can overlay multiple graphs, give them different coloured lines
fetch gage outputs from the usgs api. gage to wb- matching can be found in the flowpath-attributes table of the .gpkg in the config folder. colums gage and id. cache the usgs output in a new eval folder in the same level as config outputs forcings.
add some of the common hydrology metrics kge nse etc for both the whole hydrograph and the currently zoomed selection. only show the extra zoomed metrics if they're different from the whole hydrograph metrics
```
