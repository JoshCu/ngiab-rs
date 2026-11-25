# ngiab-rs

```sh
# macOS and Linux
curl -LsSf https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.sh | sh
# Windows
powershell -ExecutionPolicy ByPass -c "irm https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.ps1 | iex"
```

NextGen In A Box, Rust edition: a [ratatui](https://ratatui.rs) front end that runs
[bmi-driver](https://github.com/JoshCu/bmi-driver) followed by
[rs-route](https://github.com/CIROH-UA/rs_route) on a NextGen data folder, shows their live
progress, and plots the results.

## Backends

| backend | platforms | needs |
| --- | --- | --- |
| `local` | Linux x86_64 only, glibc ≥ 2.34 | `bmi-driver` and `rs-route` built with Rust (needs a C/C++ compiler, gfortran, cmake ≥ 3.26 and make), plus the model files in `~/.ngiab/container` (`ngiab-rs models update`). No containers. Fastest. |
| `docker` | Linux, macOS, Windows | Docker. Runs each tool in `docker.io/joshcu/ngiab-rs:<version of this binary>`. |
| `podman` | Linux, macOS, Windows | Podman (rootless, daemonless). Same image. |

Older Linux distros (Ubuntu 20.04, Debian 11, RHEL 8: glibc < 2.34) should use docker or podman.
**Python BMI models and SUMMA are not supported** on any backend.

## Installing

The installer downloads the binary into `~/.local/bin` (`%USERPROFILE%\.local\bin` on Windows), adds it
to `PATH` (lines marked `# ngiab-rs` in `~/.bashrc`, `~/.zshrc`, `~/.profile`, and
`~/.config/fish/conf.d/ngiab-rs.fish`), then offers a menu of backends and explains what each one will
install. Every `sudo` or package-manager command is shown and confirmed first.

Non-interactive use (flags win over environment variables):

```sh
curl -LsSf https://github.com/JoshCu/ngiab-rs/releases/latest/download/install.sh | sh -s -- --backends docker -y
```

| flag | env | meaning |
| --- | --- | --- |
| `--version X.Y.Z` | `NGIAB_RS_VERSION` | pin the binary release (default latest) |
| `--install-dir DIR` | `NGIAB_RS_INSTALL_DIR` | default `~/.local/bin` |
| `--backends local,docker,podman\|none` | `NGIAB_RS_BACKENDS` | skip the menu |
| `--skip-binary` | `NGIAB_RS_SKIP_BINARY=1` | only set up backends |
| `--no-modify-path` | `NGIAB_RS_NO_MODIFY_PATH=1` | don't edit shell rc files |
| `-y`, `--yes` | `NGIAB_RS_YES=1` | accept all prompts, including sudo/package installs |

`install.ps1` takes the same settings as environment variables only, e.g.
`$env:NGIAB_RS_BACKENDS="docker"; irm …/install.ps1 | iex`. On Windows only docker and podman are offered.

Without a terminal and without `--backends`/`-y`, the installer only installs the binary and prints the
command to finish. Some steps can't finish unattended: Docker Desktop needs to be launched once, and
being added to the `docker` group only takes effect after logging in again.

Other ways to get it: `cargo install --git https://github.com/JoshCu/ngiab-rs --features static`, or the
container image (`docker run -it -v "$PWD:/data" joshcu/ngiab-rs`), which is also the option for Linux aarch64.

### Uninstalling

1. Delete the binary (`~/.local/bin/ngiab-rs`, or `ngiab-rs.exe` on Windows).
2. Delete `~/.ngiab/container`, `~/.ngiab/container.bak` and `~/.ngiab/install.json`.
   **Do not delete the rest of `~/.ngiab`**: it belongs to NGIAB_data_preprocess.
3. Remove the `# ngiab-rs` PATH lines from your shell rc files (and `~/.config/fish/conf.d/ngiab-rs.fish`).

## Usage

```
ngiab-rs [DATA_DIR] [--auto] [-j N] [--backend local|docker|podman] [--image IMAGE] [--no-routing]
ngiab-rs self update [--version V] [--check] [-y]
ngiab-rs models update [--version models-YYYY.MM.DD] [--file TAR] [--check] [--force] [-y]
ngiab-rs models revert [-y]
ngiab-rs models status
ngiab-rs backend list
ngiab-rs backend install <local|docker|podman>...
ngiab-rs run [DATA_DIR] [flags]
```

| flag | meaning |
| --- | --- |
| `DATA_DIR` | folder with `config/` (realization.json + `.gpkg`), `forcings/`, `outputs/`. If omitted: `/data` inside the container, otherwise you're prompted (the last-used path is remembered). |
| `--auto` | no menus: run the preflight checks, then bmi-driver, then rs-route, then exit (non-zero on failure). Never prompts. |
| `-j N`, `--num-cpus N` | CPUs to use, passed as `bmi-driver -j N` and `rs-route -n N` (default: all). |
| `-b`, `--backend` | `local`, `docker` or `podman`. Default: the first usable of local > docker > podman. Also `NGIAB_RS_BACKEND`. |
| `--image` | image for container backends (default `docker.io/joshcu/ngiab-rs:<version of this binary>`, or `NGIAB_RS_IMAGE`). |
| `--no-routing` | auto mode: only run bmi-driver. |

A data folder named `self`, `models`, `backend` or `run` is read as a subcommand: write it as a path
(`ngiab-rs ./models`) or use `ngiab-rs run models`.

### `self update`

Replaces the running binary with the newest release (or `--version`), after checking it against the
release's `SHA256SUMS` and running the new binary's `--version`. `--check` exits 0 when up to date and 10
when an update is available. A downgrade asks for confirmation unless `-y`. It refuses inside a
container (pull the image instead) and for `cargo install` builds (use `cargo install --force`). The
docker/podman image matching the new version is pulled on next use.

### `models update | revert | status` (local backend, Linux x86_64)

The local backend needs the model libraries and datasets that live at `/dmod` and `/ngen/ngen/extern`
inside the image. `models update` downloads them from the newest `models-YYYY.MM.DD` release into
`~/.ngiab/container` (about 11 MB) and moves the previous copy to `~/.ngiab/container.bak`. Only one level
of undo is kept. `models revert` swaps `container` and `container.bak`, so **running revert twice
re-applies the update**. `--file TAR` installs a local tarball. `$NGIAB_HOME` moves `~/.ngiab`.
On macOS and Windows these commands do nothing: docker/podman need no model files.

When running locally, container paths in `realization.json` and the per-catchment configs
(`/dmod/...`, `/ngen/ngen/extern/...`, `/data/...`, `/ngen/ngen/data/...`) are mapped to the bundle and
your data folder. Your `config/` is never edited: rewritten copies go to `<data>/.ngiab-rs/stage/` and
bmi-driver gets `--config <data>/.ngiab-rs/stage/realization.json`. If you accept bmi-driver's
"add these mappings?" prompt, the change is written to your `config/realization.json` with container-style
paths, after saving `realization.json.bak`, the same as a docker run would.

### `backend list | install`

`backend list` shows each backend, whether it can be used here and why not, and the model-file status.
`backend install docker` downloads the installer for this exact version, verifies it, and runs it for
just that backend. When you pick a backend that isn't set up, ngiab-rs offers to do this for you
(never in `--auto` mode or inside a container). A stopped docker daemon or podman machine gets a hint to
start it instead.

## Interactive mode

* **Home**: menu, available backends (press `b` to switch; unusable ones are greyed out with the reason),
  jobs (`+`/`-`), and the preflight checks:
  folder layout, realization (period, models, output format), hydrofabric and divide count,
  forcing file, per-model init configs (checked for the first divide), model files and libraries (local
  runs), binaries or container runtime and image, plus any existing outputs.
* **Run**: each step runs in a pseudo-terminal. The tool's own output is shown live, and a progress gauge
  is driven by the tool's progress bar. Anything you type goes to the tool, so bmi-driver's
  "add these mappings? [y/N]" prompt works. `ctrl-c` cancels the run (containers are killed too), and
  `pgup`/`pgdn` scroll back. If outputs already exist you choose whether to delete them or keep them.
* **Evaluate**: a *Hydrograph* tab (rs-route `outputs/troute/*.nc`: flow / velocity / depth per
  `wb-*`) and a *Catchment* tab (any column of bmi-driver's `cat-*.csv`). `←`/`→` changes the variable
  and `/` filters the list. Min/mean/max and the peak time are shown above the chart.
* **Shell**: opens a shell in the data folder, inside the image when a container backend is selected.

rs-route is always given `-i <output_root from realization.json>` and `-o outputs/troute`, so it reads
wherever bmi-driver wrote. bmi-driver's default is `outputs/bmi-driver`, while rs-route defaults to `outputs/ngen`.

## Containers

The data folder is mounted at `/data`. Each tool runs in its own `docker run --rm` with `--entrypoint`.
Under rootful docker the container runs as your UID:GID, so output files are owned by you.

```sh
docker run --rm -it -v "$PWD/my_run:/data" joshcu/ngiab-rs                 # TUI inside the container
docker run --rm     -v "$PWD/my_run:/data" joshcu/ngiab-rs /data --auto -j 8  # headless
```

## Building

```sh
cargo install --path .                     # needs libnetcdf on the system
cargo install --path . --features static   # builds netcdf/hdf5 from source (cmake ≥ 3.26, C compiler)
docker build -t joshcu/ngiab-rs .          # base image pinned by ARG NGIAB_BASE, tools by TOOL_VERSIONS
```

## Releasing

**Binaries and Docker image.** `Cargo.toml` `version` is the source of truth.

1. Bump `version` in `Cargo.toml`, then run `cargo update -p ngiab-rs --offline`.
2. Commit as `release vX.Y.Z`.
3. `git tag vX.Y.Z && git push origin main vX.Y.Z`.

`release.yml` checks that the tag matches `Cargo.toml`, builds and tests Linux x86_64 (manylinux_2_28),
macOS arm64/x86_64 and Windows x86_64, pushes `joshcu/ngiab-rs:X.Y.Z` for amd64 and arm64, and publishes
the GitHub release with the archives, `install.sh`, `install.ps1` and `SHA256SUMS`. Stable releases also
move the `X.Y` and `latest` Docker tags and GitHub's "latest" release. A tag containing `-`
(e.g. `v0.2.0-rc1`) is a prerelease and moves neither.

**Model bundle.** Bump `ARG NGIAB_BASE=` in the Dockerfile if needed and commit, then
`git tag models-$(date +%Y.%m.%d) && git push origin models-$(date +%Y.%m.%d)` (add `.1`, `.2` for a
second release on the same day). `models.yml` builds `ngiab-models-x86_64-unknown-linux-gnu.tar.gz` from
that base image and publishes it as a release that is never marked latest. When the base changes, also
cut a binary patch release, because only `release.yml` rebuilds the Docker image. To build a bundle
locally: `scripts/build-model-bundle.sh awiciroh/ngiab:<tag> models-test /tmp/bundle`.

**Dry runs.** Run either workflow from the Actions tab with `dry_run` checked: everything is built and
uploaded as workflow artifacts, nothing is released or pushed.

**One-time setup.** Repo secrets `DOCKERHUB_USERNAME` (`joshcu`) and `DOCKERHUB_TOKEN` (Read & Write on
`joshcu/ngiab-rs`; the Docker Hub repo must exist). `GITHUB_TOKEN` covers the release uploads.

## License

MIT, see [LICENSE](LICENSE).
