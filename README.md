# ngiab-rs

NextGen In A Box, Rust edition: a [ratatui](https://ratatui.rs) front end that runs
[bmi-driver](https://github.com/JoshCu/bmi-driver) followed by
[rs-route](https://github.com/CIROH-UA/rs_route) on a NextGen data folder, shows their live
progress, and plots the results.

```
ngiab-rs [DATA_DIR] [--auto] [-j N] [--backend local|docker|podman] [--image IMAGE] [--no-routing]
```

| flag | meaning |
| --- | --- |
| `DATA_DIR` | folder with `config/` (realization.json + `.gpkg`), `forcings/`, `outputs/`. If omitted: `/data` inside the container, otherwise you're prompted (the last-used path is remembered). |
| `--auto` | no menus: run the preflight checks, then bmi-driver, then rs-route, then exit (non-zero on failure). |
| `-j N`, `--num-cpus N` | CPUs to use, passed as `bmi-driver -j N` and `rs-route -n N` (default: all). |
| `--backend` | `local` binaries, `docker`, or `podman`. Default preference: local > docker > podman. Also `NGIAB_RS_BACKEND`. |
| `--image` | image for container backends (default `docker.io/joshcu/ngiab-rs:<version of this binary>`, or `NGIAB_RS_IMAGE`). |
| `--no-routing` | auto mode: only run bmi-driver. |

Other subcommands: `ngiab-rs self update`, `ngiab-rs models update|revert|status` and
`ngiab-rs backend list|install` (see `ngiab-rs --help`). `ngiab-rs run [DATA_DIR] [flags]` is the
explicit form of the default action. Because of this, a data folder named `self`, `models`,
`backend` or `run` is read as a subcommand: write it as a path (`ngiab-rs ./models`) or use
`ngiab-rs run models`.

## Interactive mode

* **Home**: menu, available backends (press `b` to switch), jobs (`+`/`-`), and the preflight checks:
  folder layout, realization (period, models, output format), hydrofabric and divide count,
  forcing file, per-model init configs (checked for the first divide), model libraries (local runs),
  binaries or container runtime and image, plus any existing outputs.
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
docker build -t joshcu/ngiab-rs .
docker run --rm -it -v "$PWD/my_run:/data" joshcu/ngiab-rs                 # TUI inside the container
docker run --rm     -v "$PWD/my_run:/data" joshcu/ngiab-rs /data --auto -j 8  # headless
```

## Building

```sh
cargo install --path .              # needs libnetcdf on the system
cargo install --path . --features static   # builds netcdf/hdf5 from source (used in the Dockerfile)
```
