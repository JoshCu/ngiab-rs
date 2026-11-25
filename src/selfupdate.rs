//! `ngiab-rs self update` (tasks.md B.3).
//!
//! The new release is downloaded into a scratch folder next to the running exe
//! (same file system, so the final rename is atomic), verified against the
//! release's `SHA256SUMS`, extracted, smoke-tested with `--version`, and only
//! then swapped in with `self_replace`. Any failure before the swap leaves the
//! installed exe untouched.

use crate::cli::{self, SelfUpdateArgs};
use crate::{backend, home, models, net};
use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

const EXE_NAME: &str = if cfg!(windows) {
    "ngiab-rs.exe"
} else {
    "ngiab-rs"
};

/// Scratch folder next to the exe. A leftover one (interrupted update) is removed
/// by the next update.
const SCRATCH_DIR: &str = ".ngiab-rs-update";

const SMOKE_TIMEOUT: Duration = Duration::from_secs(5);

/// `ngiab-rs-<triple>.tar.gz` (`.zip` on Windows): the release archive for this build.
pub fn asset_name() -> String {
    asset_name_for(env!("NGIAB_TARGET"))
}

fn asset_name_for(target: &str) -> String {
    let ext = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("ngiab-rs-{target}.{ext}")
}

fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is semver")
}

/// `0.2.0` or `v0.2.0` -> (`v0.2.0`, version).
fn parse_requested(v: &str) -> Result<(String, Version)> {
    let ver = net::tag_version(v)
        .with_context(|| format!("invalid version {v:?}: expected e.g. 0.2.0 or v0.2.0"))?;
    Ok((format!("v{ver}"), ver))
}

/// What to do once the target release is known.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    UpToDate,
    Update,
    /// Only with an explicit `--version`: needs confirmation.
    Downgrade,
}

fn decide(current: &Version, target: &Version, pinned: bool) -> Decision {
    match target.cmp(current) {
        std::cmp::Ordering::Greater => Decision::Update,
        std::cmp::Ordering::Less if pinned => Decision::Downgrade,
        _ => Decision::UpToDate,
    }
}

/// Why this install must not update itself, if it must not.
fn refusal(
    in_container: bool,
    exe: &Path,
    cargo_bin: Option<&Path>,
    install_method: &str,
) -> Option<String> {
    if in_container {
        return Some(format!(
            "ngiab-rs is running inside a container: pull the image instead \
             (docker pull {})",
            image_for(None)
        ));
    }
    if install_method == "cargo" || cargo_bin.is_some_and(|b| exe.starts_with(b)) {
        return Some(
            "ngiab-rs was installed with cargo: use `cargo install --force --locked ngiab-rs` \
             to update it"
                .into(),
        );
    }
    None
}

/// `$CARGO_HOME/bin`, else `~/.cargo/bin` (canonicalised when it exists).
fn cargo_bin() -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")))?;
    let bin = home.join("bin");
    Some(bin.canonicalize().unwrap_or(bin))
}

/// The pinned docker/podman image for `ver` (default: this build).
fn image_for(ver: Option<&Version>) -> String {
    match ver {
        Some(v) => format!("docker.io/joshcu/ngiab-rs:{v}"),
        None => backend::DEFAULT_IMAGE.to_string(),
    }
}

/// The newest binary release: GitHub's "latest" release, or for a prerelease build
/// the highest version tag including prereleases (so rc1 finds rc2). Falls back to the `releases/latest` redirect when the API fails
/// (e.g. rate-limited).
fn newest_release(repo: &str, include_prereleases: bool) -> Result<(String, Version)> {
    let api = if include_prereleases {
        net::github_releases(repo).and_then(|list| {
            pick_newest(&list, true).with_context(|| format!("{repo} has no ngiab-rs release"))
        })
    } else {
        net::github_release(repo, net::ReleaseRef::Latest).and_then(|r| {
            let ver = net::tag_version(&r.tag_name).with_context(|| {
                format!(
                    "the latest release of {repo} ({}) is not a version tag",
                    r.tag_name
                )
            })?;
            Ok((r.tag_name, ver))
        })
    };
    match api {
        Ok(found) => Ok(found),
        Err(api) => {
            let tag = net::latest_tag_via_redirect(repo)
                .with_context(|| format!("{api:#}; and without the API"))?;
            let ver = net::tag_version(&tag)
                .with_context(|| format!("latest release tag {tag:?} is not a version"))?;
            Ok((tag, ver))
        }
    }
}

fn pick_newest(list: &[net::Release], include_prereleases: bool) -> Option<(String, Version)> {
    list.iter()
        .filter(|r| !r.draft && (include_prereleases || !r.prerelease))
        .filter_map(|r| Some((r.tag_name.clone(), net::tag_version(&r.tag_name)?)))
        .filter(|(_, v)| include_prereleases || v.pre.is_empty())
        .max_by(|a, b| a.1.cmp(&b.1))
}

/// Is `dir` writable? Probed by creating (and removing) a file in it.
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".ngiab-rs-write-test-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The file `self_replace` will replace: the exe, through one level of symlink
/// (what `self_replace` itself does on Unix).
fn exe_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running executable")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// `self update [--version V] [--check] [-y]`.
pub fn update(args: &SelfUpdateArgs) -> Result<ExitCode> {
    let exe = exe_path()?;
    let info = home::InstallInfo::load();
    // --check only reads, so it works everywhere.
    if !args.check
        && let Some(why) = refusal(
            backend::in_container(),
            &exe,
            cargo_bin().as_deref(),
            &info.install_method,
        )
    {
        bail!("{why}");
    }

    let repo = home::repo();
    let current = current_version();
    let pinned = args.version.is_some();
    let (tag, target) = match &args.version {
        Some(v) => parse_requested(v)?,
        None => newest_release(&repo, !current.pre.is_empty())?,
    };
    let decision = decide(&current, &target, pinned);
    if args.check {
        return Ok(match decision {
            Decision::UpToDate => {
                println!("ngiab-rs {current} is up to date");
                ExitCode::SUCCESS
            }
            _ => {
                println!("ngiab-rs {target} is available (installed: {current})");
                ExitCode::from(cli::EXIT_UPDATE_AVAILABLE)
            }
        });
    }
    match decision {
        Decision::UpToDate => {
            println!("ngiab-rs {current} is up to date");
            return Ok(ExitCode::SUCCESS);
        }
        Decision::Downgrade => {
            if !cli::confirm(
                &format!("Downgrade ngiab-rs from {current} to {target}?"),
                args.yes,
            )? {
                println!("cancelled");
                return Ok(ExitCode::FAILURE);
            }
        }
        Decision::Update => {}
    }

    let dir = exe
        .parent()
        .context("the executable has no parent folder")?
        .to_path_buf();
    ensure!(
        dir_writable(&dir),
        "{} is not writable, so ngiab-rs can't replace itself; re-run the installer \
         (or update as the user that owns that folder)",
        dir.display()
    );

    let scratch = dir.join(SCRATCH_DIR);
    let res = (|| -> Result<()> {
        let asset = asset_name();
        let sums_url = net::release_download_url(&repo, &tag, "SHA256SUMS");
        let url = net::release_download_url(&repo, &tag, &asset);
        println!("downloading ngiab-rs {target} ({asset})");
        let new = fetch_verified(&sums_url, &url, &asset, &scratch, &target)?;
        self_replace::self_replace(&new).map_err(|e| replace_error(e, &exe))?;
        Ok(())
    })();
    let _ = fs::remove_dir_all(&scratch);
    res?;
    println!("updated ngiab-rs {current} -> {target} ({})", exe.display());

    let mut info = info;
    info.ngiab_rs_version = target.to_string();
    info.target = env!("NGIAB_TARGET").into();
    info.bin_path = Some(exe.clone());
    if info.install_method.is_empty() {
        info.install_method = "installer".into();
    }
    info.installed_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if let Err(e) = info.save() {
        eprintln!("warning: could not update install.json: {e}");
    }

    print_hints(&repo, &target);
    Ok(ExitCode::SUCCESS)
}

fn replace_error(e: std::io::Error, exe: &Path) -> anyhow::Error {
    let hint = if cfg!(windows) {
        "; it may be in use (another ngiab-rs window, or antivirus). Close it and retry, \
         or re-run install.ps1"
    } else {
        ""
    };
    anyhow::anyhow!("replacing {}: {e}{hint}", exe.display())
}

/// Download `url` (the release archive called `asset`) into `scratch`, check it
/// against `SHA256SUMS` at `sums_url`, extract it and smoke-test the binary.
/// Returns the verified new executable. Nothing outside `scratch` is touched.
fn fetch_verified(
    sums_url: &str,
    url: &str,
    asset: &str,
    scratch: &Path,
    want: &Version,
) -> Result<PathBuf> {
    // Leftovers of an interrupted update.
    if scratch.symlink_metadata().is_ok() {
        fs::remove_dir_all(scratch)
            .with_context(|| format!("removing the old {}", scratch.display()))?;
    }
    fs::create_dir_all(scratch).with_context(|| format!("creating {}", scratch.display()))?;
    let sums = net::get_text(sums_url).context("fetching SHA256SUMS")?;
    // Fails early (before the download) if the release has no build for us.
    net::expected_sha256(&sums, asset)
        .with_context(|| format!("the release has no {asset} (no build for this platform?)"))?;
    let archive = scratch.join(asset);
    net::download(url, &archive, true)?;
    net::verify_sha256sums(&sums, asset, &archive)?;
    let out = scratch.join("x");
    if asset.ends_with(".zip") {
        extract_zip(&archive, &out)?;
    } else {
        net::extract_tar_gz(&archive, &out)?;
    }
    let new = out.join(EXE_NAME);
    let meta = new
        .symlink_metadata()
        .with_context(|| format!("{asset} has no {EXE_NAME} at its root"))?;
    ensure!(
        meta.is_file(),
        "{EXE_NAME} in {asset} is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&new, fs::Permissions::from_mode(0o755))?;
    }
    smoke_test(&new, want)?;
    Ok(new)
}

#[cfg(windows)]
fn extract_zip(archive: &Path, into: &Path) -> Result<()> {
    net::extract_zip(archive, into)
}

#[cfg(not(windows))]
fn extract_zip(archive: &Path, _into: &Path) -> Result<()> {
    bail!(
        "{}: zip archives are only used on Windows",
        archive.display()
    )
}

/// Run `<new> --version` and require `ngiab-rs <want>`: catches a binary that
/// can't start here (e.g. too new a glibc) before it replaces the working one.
fn smoke_test(new: &Path, want: &Version) -> Result<()> {
    let out = backend::run_with_timeout(Command::new(new).arg("--version"), SMOKE_TIMEOUT)
        .with_context(|| {
            format!(
                "the new binary did not run (`{} --version` failed or took over {} s)",
                new.display(),
                SMOKE_TIMEOUT.as_secs()
            )
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("ngiab-rs {want}");
    ensure!(
        out.status.success() && stdout.trim() == expected,
        "the new binary failed its check: `--version` gave {:?} (exit {}), expected {expected:?}{}",
        stdout.trim(),
        out.status,
        match String::from_utf8_lossy(&out.stderr).trim() {
            "" => String::new(),
            e => format!(": {e}"),
        }
    );
    Ok(())
}

fn print_hints(repo: &str, target: &Version) {
    let image = image_for(Some(target));
    println!(
        "The docker/podman image for this version is {image}; it is pulled on next use, \
         or run `docker pull {image}` now."
    );
    if !backend::LOCAL_SUPPORTED {
        return;
    }
    let Ok(installed) = home::container_dir()
        .and_then(|d| models::Manifest::load(&d).map_err(std::io::Error::other))
    else {
        return;
    };
    if let Ok(newest) = models::newest_release(repo)
        && models::release_lt(&installed.release, &newest.tag_name)
    {
        println!(
            "New model files are available ({}): run `ngiab-rs models update`",
            newest.tag_name
        );
    }
}

/// Windows: remove what an earlier `self update` may have left next to the exe
/// (`self_replace` relocates the running exe to `.ngiab-rs.*.__relocated__.exe`
/// and deletes it on exit; a crash can leave it, or a `.__temp__.exe` copy, behind;
/// installers may leave `ngiab-rs.exe.old`). Best effort, only files older than a
/// minute (so an update in progress elsewhere isn't disturbed); files still in use
/// simply fail to delete.
#[cfg(windows)]
pub fn cleanup_stale_binaries() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else { return };
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        if !is_stale_binary_name(&name) {
            continue;
        }
        let old_enough = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(60));
        if old_enough {
            let _ = fs::remove_file(e.path());
        }
    }
}

#[cfg(not(windows))]
pub fn cleanup_stale_binaries() {}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn is_stale_binary_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "ngiab-rs.exe.old"
        || (name.starts_with(".ngiab-rs.")
            && (name.ends_with(".__relocated__.exe") || name.ends_with(".__temp__.exe")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn asset_names() {
        assert_eq!(
            asset_name_for("x86_64-unknown-linux-gnu"),
            "ngiab-rs-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name_for("aarch64-apple-darwin"),
            "ngiab-rs-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name_for("x86_64-pc-windows-msvc"),
            "ngiab-rs-x86_64-pc-windows-msvc.zip"
        );
        assert!(asset_name().starts_with("ngiab-rs-"));
    }

    #[test]
    fn requested_versions() {
        assert_eq!(
            parse_requested("0.2.0").unwrap(),
            ("v0.2.0".into(), v("0.2.0"))
        );
        assert_eq!(parse_requested("v0.2.0").unwrap().0, "v0.2.0");
        assert_eq!(
            parse_requested("v0.2.0-rc1").unwrap(),
            ("v0.2.0-rc1".into(), v("0.2.0-rc1"))
        );
        assert!(parse_requested("models-2026.11.23").is_err());
        assert!(parse_requested("latest").is_err());
    }

    #[test]
    fn decisions_and_check_exit_codes() {
        let cur = v("0.2.0");
        assert_eq!(decide(&cur, &v("0.2.1"), false), Decision::Update);
        assert_eq!(decide(&cur, &v("0.2.0"), false), Decision::UpToDate);
        assert_eq!(decide(&cur, &v("0.1.9"), false), Decision::UpToDate);
        assert_eq!(decide(&cur, &v("0.1.9"), true), Decision::Downgrade);
        assert_eq!(decide(&cur, &v("0.2.0"), true), Decision::UpToDate);
        // Prerelease ordering: rc1 < rc2 < final.
        assert_eq!(
            decide(&v("0.2.0-rc1"), &v("0.2.0-rc2"), false),
            Decision::Update
        );
        assert_eq!(
            decide(&v("0.2.0-rc2"), &v("0.2.0"), false),
            Decision::Update
        );
        assert_eq!(cli::EXIT_UPDATE_AVAILABLE, 10);
    }

    fn rel(tag: &str, prerelease: bool, draft: bool) -> net::Release {
        net::Release {
            tag_name: tag.into(),
            draft,
            prerelease,
            assets: vec![],
        }
    }

    #[test]
    fn newest_binary_release_ignores_models_and_drafts() {
        let list = vec![
            rel("models-2026.12.01", false, false),
            rel("v0.3.0", false, true),
            rel("v0.2.1-rc1", true, false),
            rel("v0.2.0", false, false),
            rel("v0.1.0", false, false),
        ];
        assert_eq!(pick_newest(&list, false).unwrap().0, "v0.2.0");
        assert_eq!(pick_newest(&list, true).unwrap().0, "v0.2.1-rc1");
        assert_eq!(
            pick_newest(&[rel("models-2026.12.01", false, false)], true),
            None
        );
    }

    #[test]
    fn refusals() {
        let exe = Path::new("/home/u/.local/bin/ngiab-rs");
        let cargo = Path::new("/home/u/.cargo/bin");
        assert!(
            refusal(true, exe, Some(cargo), "installer")
                .unwrap()
                .contains("pull the image")
        );
        assert!(
            refusal(
                false,
                Path::new("/home/u/.cargo/bin/ngiab-rs"),
                Some(cargo),
                ""
            )
            .unwrap()
            .contains("cargo install --force")
        );
        assert!(
            refusal(false, exe, Some(cargo), "cargo")
                .unwrap()
                .contains("cargo install")
        );
        assert_eq!(refusal(false, exe, Some(cargo), "installer"), None);
        assert_eq!(refusal(false, exe, None, ""), None);
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("ngiab-rs-selfupdate-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writable_dirs() {
        let d = tmpdir("writable");
        assert!(dir_writable(&d));
        assert!(!dir_writable(&d.join("missing")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ro = d.join("ro");
            fs::create_dir(&ro).unwrap();
            fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
            // root ignores permissions; only assert when the probe can tell.
            let root = unsafe { libc::geteuid() } == 0;
            if !root {
                assert!(!dir_writable(&ro));
            }
            fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn stale_binary_names() {
        assert!(is_stale_binary_name("ngiab-rs.exe.old"));
        assert!(is_stale_binary_name(".ngiab-rs.abcdefgh.__relocated__.exe"));
        assert!(is_stale_binary_name(".ngiab-rs.abcdefgh.__temp__.exe"));
        assert!(!is_stale_binary_name("ngiab-rs.exe"));
        assert!(!is_stale_binary_name(".ngiab-rs.abc.__selfdelete__.exe"));
        assert!(!is_stale_binary_name("other.exe.old"));
    }

    /// Serves `files` (path -> body) over HTTP on localhost until dropped.
    #[cfg(unix)]
    struct Server {
        url: String,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[cfg(unix)]
    impl Server {
        fn start(files: Vec<(&'static str, Vec<u8>)>) -> Self {
            use std::io::{Read, Write};
            use std::sync::atomic::{AtomicBool, Ordering};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            std::thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    let Ok((mut s, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    s.set_nonblocking(false).unwrap();
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && s.read(&mut byte).unwrap_or(0) == 1 {
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let resp = match files.iter().find(|(p, _)| *p == path) {
                        Some((_, body)) => {
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            r.extend_from_slice(body);
                            r
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec(),
                    };
                    let _ = s.write_all(&resp);
                }
            });
            Self { url, stop }
        }
    }

    #[cfg(unix)]
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// A release archive whose `ngiab-rs` is a script printing `version_line`.
    #[cfg(unix)]
    fn fake_archive(version_line: &str) -> Vec<u8> {
        use flate2::write::GzEncoder;
        let script = format!("#!/bin/sh\necho '{version_line}'\n");
        let mut b = tar::Builder::new(GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (name, data, mode) in [
            ("ngiab-rs", script.as_bytes(), 0o644),
            ("README.md", b"readme".as_slice(), 0o644),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(mode);
            h.set_cksum();
            b.append_data(&mut h, name, data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn fetch_verify_extract_and_smoke_test() {
        let asset = "ngiab-rs-x86_64-unknown-linux-gnu.tar.gz";
        let good = fake_archive("ngiab-rs 9.9.9");
        let wrong_version = fake_archive("ngiab-rs 9.9.8");
        let sums = |body: &[u8]| format!("{}  {asset}\n", net::sha256_bytes(body)).into_bytes();
        let server = Server::start(vec![
            ("/good/SHA256SUMS", sums(&good)),
            ("/good/a", good.clone()),
            // SHA256SUMS that doesn't match the archive (corrupted / tampered).
            ("/bad/SHA256SUMS", sums(b"something else")),
            ("/bad/a", good.clone()),
            ("/old/SHA256SUMS", sums(&wrong_version)),
            ("/old/a", wrong_version),
            (
                "/none/SHA256SUMS",
                b"0000000000000000000000000000000000000000000000000000000000000000  other.zip\n"
                    .to_vec(),
            ),
        ]);
        let d = tmpdir("fetch");
        let scratch = d.join(SCRATCH_DIR);
        let want = v("9.9.9");
        let u = |p: &str| format!("{}{p}", server.url);

        // A stale scratch folder is replaced.
        fs::create_dir_all(scratch.join("junk")).unwrap();
        let new = fetch_verified(
            &u("/good/SHA256SUMS"),
            &u("/good/a"),
            asset,
            &scratch,
            &want,
        )
        .unwrap();
        assert_eq!(new, scratch.join("x").join("ngiab-rs"));
        assert!(!scratch.join("junk").exists());
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&new).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }

        let e = fetch_verified(&u("/bad/SHA256SUMS"), &u("/bad/a"), asset, &scratch, &want)
            .unwrap_err();
        assert!(format!("{e:#}").contains("checksum mismatch"), "{e:#}");
        assert!(!scratch.join("x").exists());

        let e = fetch_verified(&u("/old/SHA256SUMS"), &u("/old/a"), asset, &scratch, &want)
            .unwrap_err();
        assert!(format!("{e:#}").contains("9.9.8"), "{e:#}");

        let e = fetch_verified(
            &u("/none/SHA256SUMS"),
            &u("/none/a"),
            asset,
            &scratch,
            &want,
        )
        .unwrap_err();
        assert!(
            format!("{e:#}").contains("no build for this platform"),
            "{e:#}"
        );

        let e = fetch_verified(
            &u("/missing/SHA256SUMS"),
            &u("/missing/a"),
            asset,
            &scratch,
            &want,
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("404"), "{e:#}");
        let _ = fs::remove_dir_all(&d);
    }
}
