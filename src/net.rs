//! The one HTTP module: GitHub releases, downloads, checksums and archive
//! extraction. Used by `self update`, `models update` and the USGS fetcher.
//!
//! Every request goes through [`agent()`], which never turns an HTTP status into an
//! error (`http_status_as_error(false)`), so callers can inspect 403/429/5xx.

// Consumers (self update, models update, USGS) land in later steps.
#![allow(dead_code)]

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const USER_AGENT: &str = concat!("ngiab-rs/", env!("CARGO_PKG_VERSION"));

/// Sent as `Authorization: Bearer …` to api.github.com only. Without it the API
/// allows 60 requests an hour per IP.
pub const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";

const GITHUB_API: &str = "https://api.github.com";

/// Releases per page when listing (the API maximum).
const PER_PAGE: usize = 100;
/// Hard stop for pagination, in case the API keeps returning full pages.
const MAX_PAGES: usize = 20;

/// Downloads can be large (model bundles); give them more than the default
/// 120 s global timeout but still a bound.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The shared agent: `User-Agent: ngiab-rs/<ver>`, 10 s connect and 120 s global
/// timeouts, and HTTP error statuses returned as normal responses.
pub fn agent() -> ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            let config = ureq::Agent::config_builder()
                .user_agent(USER_AGENT)
                .timeout_connect(Some(Duration::from_secs(10)))
                .timeout_global(Some(Duration::from_secs(120)))
                .http_status_as_error(false)
                .build();
            ureq::Agent::new_with_config(config)
        })
        .clone()
}

/// Should `url` get the `GITHUB_TOKEN`? Only the API host, never asset/CDN hosts.
fn is_github_api(url: &str) -> bool {
    url.strip_prefix(GITHUB_API)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('?'))
}

fn github_token() -> Option<String> {
    std::env::var(GITHUB_TOKEN_ENV)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// A GET through [`agent()`], with the GitHub API headers (and token, if set) when
/// `url` is on api.github.com. ureq never forwards `Authorization` on redirects.
pub fn get(url: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let mut req = agent().get(url);
    if is_github_api(url) {
        req = req
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = github_token() {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
    }
    req
}

/// Calls `req` and fails on a non-2xx status, with a rate-limit hint for GitHub.
fn call_ok(
    req: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    url: &str,
) -> Result<ureq::http::Response<ureq::Body>> {
    let res = req.call().with_context(|| format!("GET {url}"))?;
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let remaining = res
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok());
    if is_github_api(url)
        && (status.as_u16() == 429 || (status.as_u16() == 403 && remaining == Some("0")))
    {
        let hint = if github_token().is_some() {
            "wait for the limit to reset"
        } else {
            "set GITHUB_TOKEN (any token, no scopes needed) or wait an hour"
        };
        bail!("GitHub API rate limit reached for {url}: {hint}");
    }
    bail!("GET {url}: HTTP {status}");
}

/// GETs `url` and parses the body as JSON (at most 10 MB).
pub fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    call_ok(get(url), url)?
        .body_mut()
        .read_json()
        .with_context(|| format!("reading JSON from {url}"))
}

/// GETs `url` as text (at most 10 MB), e.g. a `SHA256SUMS` file.
pub fn get_text(url: &str) -> Result<String> {
    call_ok(get(url), url)?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

// ---------------------------------------------------------------------------
// GitHub releases

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// Which release [`github_release`] fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseRef<'a> {
    /// The release GitHub marks as latest (never a prerelease or a `models-*` tag).
    Latest,
    Tag(&'a str),
}

/// One release of `repo` (`owner/name`).
pub fn github_release(repo: &str, which: ReleaseRef) -> Result<Release> {
    let url = match which {
        ReleaseRef::Latest => format!("{GITHUB_API}/repos/{repo}/releases/latest"),
        ReleaseRef::Tag(t) => format!("{GITHUB_API}/repos/{repo}/releases/tags/{t}"),
    };
    get_json(&url).with_context(|| match which {
        ReleaseRef::Latest => format!("finding the latest release of {repo}"),
        ReleaseRef::Tag(t) => format!("finding release {t} of {repo}"),
    })
}

/// Every release of `repo`, newest first as the API orders them, drafts included
/// when the token can see them. Follows pagination.
pub fn github_releases(repo: &str) -> Result<Vec<Release>> {
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = format!("{GITHUB_API}/repos/{repo}/releases?per_page={PER_PAGE}&page={page}");
        let batch: Vec<Release> =
            get_json(&url).with_context(|| format!("listing releases of {repo}"))?;
        let full = batch.len() == PER_PAGE;
        all.extend(batch);
        if !full {
            break;
        }
    }
    Ok(all)
}

/// The semver of a binary release tag: `v0.2.0`, `0.2.0` or `v0.2.0-rc1`.
/// `None` for anything else (e.g. `models-2026.11.23`).
pub fn tag_version(tag: &str) -> Option<semver::Version> {
    let t = tag.trim();
    semver::Version::parse(t.strip_prefix('v').unwrap_or(t)).ok()
}

/// `https://github.com/{repo}/releases/latest/download/{asset}`: works without the
/// API (and its rate limit) for assets of the latest release.
pub fn latest_download_url(repo: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/latest/download/{asset}")
}

/// `https://github.com/{repo}/releases/download/{tag}/{asset}`.
pub fn release_download_url(repo: &str, tag: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{asset}")
}

/// The latest release's tag without the API: `github.com/{repo}/releases/latest`
/// redirects to `…/releases/tag/<tag>`, so read the `Location` header.
pub fn latest_tag_via_redirect(repo: &str) -> Result<String> {
    let url = format!("https://github.com/{repo}/releases/latest");
    let res = agent()
        .get(&url)
        .config()
        .max_redirects(0)
        .build()
        .call()
        .with_context(|| format!("GET {url}"))?;
    ensure!(
        res.status().is_redirection(),
        "GET {url}: expected a redirect, got HTTP {}",
        res.status()
    );
    let location = res
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .with_context(|| format!("GET {url}: redirect without a Location header"))?;
    tag_from_location(location)
        .with_context(|| format!("{repo} has no latest release (redirected to {location})"))
}

/// The tag in a `…/releases/tag/<tag>` URL (percent-decoded).
fn tag_from_location(location: &str) -> Option<String> {
    let (_, rest) = location.split_once("/releases/tag/")?;
    let rest = rest.split(['?', '#']).next()?.trim_end_matches('/');
    let tag = percent_decode(rest)?;
    (!tag.is_empty() && !tag.contains('/')).then_some(tag)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---------------------------------------------------------------------------
// Downloads

/// `dest` with `.part` appended to the file name: where [`download`] writes until
/// the transfer is complete. Callers clean up stale ones on the next run.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// Streams `url` into `dest.part`, then renames it to `dest`. With `progress`, a
/// single `\r`-updated line goes to stderr (only when stderr is a terminal).
/// Returns the number of bytes written. On failure `dest` is untouched and the
/// `.part` file is removed.
pub fn download(url: &str, dest: &Path, progress: bool) -> Result<u64> {
    let part = part_path(dest);
    let res = (|| -> Result<u64> {
        let req = get(url)
            .config()
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .build();
        let res = call_ok(req, url)?;
        let total = res.body().content_length();
        let mut reader = res.into_body().into_reader();
        if let Some(dir) = part.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = File::create(&part).with_context(|| format!("creating {}", part.display()))?;
        let mut out = BufWriter::new(file);
        let label = dest
            .file_name()
            .map_or_else(|| url.to_string(), |n| n.to_string_lossy().into_owned());
        let mut bar = Progress::new(&label, total, progress && io::stderr().is_terminal());
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).with_context(|| format!("downloading {url}")),
            };
            out.write_all(&buf[..n])
                .with_context(|| format!("writing {}", part.display()))?;
            done += n as u64;
            bar.update(done);
        }
        bar.finish(done);
        if let Some(total) = total {
            ensure!(
                done == total,
                "downloading {url}: got {done} of {total} bytes"
            );
        }
        let file = out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&part, dest)
            .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
        Ok(done)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&part);
    }
    res
}

struct Progress {
    label: String,
    total: Option<u64>,
    enabled: bool,
    last: Option<Instant>,
}

impl Progress {
    fn new(label: &str, total: Option<u64>, enabled: bool) -> Self {
        Self {
            label: label.to_string(),
            total,
            enabled,
            last: None,
        }
    }

    fn update(&mut self, done: u64) {
        if !self.enabled
            || self
                .last
                .is_some_and(|t| t.elapsed() < Duration::from_millis(100))
        {
            return;
        }
        self.last = Some(Instant::now());
        self.draw(done);
    }

    fn finish(&mut self, done: u64) {
        if self.enabled {
            self.draw(done);
            eprintln!();
        }
    }

    fn draw(&self, done: u64) {
        let line = progress_line(&self.label, done, self.total);
        // Pad so a shorter line fully overwrites the previous one.
        eprint!("\r{line:<70}");
        let _ = io::stderr().flush();
    }
}

fn progress_line(label: &str, done: u64, total: Option<u64>) -> String {
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    match total {
        Some(t) if t > 0 => format!(
            "{label}: {:.1} / {:.1} MiB ({}%)",
            mib(done),
            mib(t),
            done.saturating_mul(100) / t
        ),
        _ => format!("{label}: {:.1} MiB", mib(done)),
    }
}

// ---------------------------------------------------------------------------
// Checksums

/// Lowercase hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The `(hex, name)` pairs of a `sha256sum` output: `<hex>  <name>` (text mode)
/// or `<hex> *<name>` (binary mode). Blank lines and `#` comments are skipped;
/// any other malformed line is an error, so a corrupted file is never trusted.
pub fn parse_sha256sums(text: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let parsed = line.split_once(' ').and_then(|(hex, rest)| {
            let name = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('*'))?;
            let ok = hex.len() == 64 && hex.bytes().all(|c| c.is_ascii_hexdigit());
            (ok && !name.is_empty()).then(|| (hex.to_ascii_lowercase(), name.to_string()))
        });
        match parsed {
            Some(p) => out.push(p),
            None => bail!("SHA256SUMS line {}: malformed: {line:?}", i + 1),
        }
    }
    Ok(out)
}

/// The expected hash of `asset_name` in a `SHA256SUMS` text.
pub fn expected_sha256(sums_text: &str, asset_name: &str) -> Result<String> {
    let sums = parse_sha256sums(sums_text)?;
    let mut hits = sums.iter().filter(|(_, n)| n == asset_name).map(|(h, _)| h);
    let Some(first) = hits.next() else {
        bail!("{asset_name} is not listed in SHA256SUMS");
    };
    ensure!(
        hits.all(|h| h == first),
        "SHA256SUMS lists {asset_name} more than once with different hashes"
    );
    Ok(first.clone())
}

/// Checks `file` against the `asset_name` line of `sums_text`.
pub fn verify_sha256sums(sums_text: &str, asset_name: &str, file: &Path) -> Result<()> {
    let want = expected_sha256(sums_text, asset_name)?;
    let got = sha256_file(file)?;
    ensure!(
        got == want,
        "checksum mismatch for {asset_name}: expected {want}, got {got} ({})",
        file.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Archives

/// A relative path made only of normal components (`.` allowed): no `..`, no
/// root, no drive prefix.
fn check_relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty(),
        "archive entry with an empty path"
    );
    for c in path.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => bail!("archive entry {} contains `..`", path.display()),
            Component::RootDir | Component::Prefix(_) => {
                bail!("archive entry {} is an absolute path", path.display())
            }
        }
    }
    Ok(())
}

/// Does `target`, resolved relative to the directory `base` (itself relative to the
/// extraction root), stay inside the root? Purely lexical.
fn link_stays_inside(base: &Path, target: &Path) -> bool {
    let mut depth: usize = base
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

fn open_tar_gz(archive: &Path) -> Result<tar::Archive<flate2::read::GzDecoder<File>>> {
    let f = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    Ok(tar::Archive::new(flate2::read::GzDecoder::new(f)))
}

/// Checks every entry of a `.tar.gz` before anything is written: paths must be
/// relative without `..`, links must point inside the archive, and only regular
/// files, directories, symlinks and hard links are allowed.
fn check_tar_gz(archive: &Path) -> Result<()> {
    use tar::EntryType;
    let mut ar = open_tar_gz(archive)?;
    let entries = ar
        .entries()
        .with_context(|| format!("reading {}", archive.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", archive.display()))?;
        let path = entry.path()?.into_owned();
        let kind = entry.header().entry_type();
        match kind {
            EntryType::XGlobalHeader | EntryType::XHeader => continue,
            EntryType::Regular | EntryType::Continuous | EntryType::Directory => {
                check_relative(&path)?
            }
            EntryType::Symlink | EntryType::Link => {
                check_relative(&path)?;
                let target = entry
                    .link_name()?
                    .with_context(|| format!("link {} has no target", path.display()))?;
                // Symlinks resolve against their own directory, hard links against
                // the archive root.
                let base = if kind == EntryType::Symlink {
                    path.parent().unwrap_or(Path::new(""))
                } else {
                    Path::new("")
                };
                ensure!(
                    link_stays_inside(base, &target),
                    "archive link {} -> {} points outside the archive",
                    path.display(),
                    target.display()
                );
            }
            other => bail!(
                "archive entry {} has an unsupported type {other:?}",
                path.display()
            ),
        }
    }
    Ok(())
}

/// Extracts a `.tar.gz` into `into` (created if missing) with
/// `tar::Archive::unpack`: permissions and symlinks are preserved. The archive is
/// checked first ([`check_tar_gz`]), so an archive with an absolute path, a `..`
/// component or a link leaving the tree is rejected before anything is written.
pub fn extract_tar_gz(archive: &Path, into: &Path) -> Result<()> {
    check_tar_gz(archive)?;
    fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;
    let mut ar = open_tar_gz(archive)?;
    ar.set_preserve_permissions(true);
    ar.set_overwrite(true);
    ar.unpack(into)
        .with_context(|| format!("extracting {} into {}", archive.display(), into.display()))
}

/// Extracts a `.zip` (the Windows release archive) into `into`, with the same
/// path checks as [`extract_tar_gz`]. Symlinks are rejected.
#[cfg(windows)]
pub fn extract_zip(archive: &Path, into: &Path) -> Result<()> {
    let f = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(f).with_context(|| format!("reading {}", archive.display()))?;
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        let name = entry.name()?.into_owned();
        check_relative(Path::new(&name.replace('\\', "/")))?;
        ensure!(
            entry.enclosed_name().is_some(),
            "archive entry {name} escapes the extraction folder"
        );
        ensure!(!entry.is_symlink(), "archive entry {name} is a symlink");
    }
    fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;
    zip.extract(into)
        .with_context(|| format!("extracting {} into {}", archive.display(), into.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ngiab-rs-net-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// One tar entry: (raw path, type, link target, contents). The path is
    /// written straight into the header so unsafe names can be crafted.
    type Spec<'a> = (&'a str, tar::EntryType, Option<&'a str>, &'a [u8]);

    fn write_tar_gz(path: &Path, entries: &[Spec]) {
        let gz = GzEncoder::new(File::create(path).unwrap(), Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (name, kind, link, data) in entries {
            let mut h = tar::Header::new_gnu();
            {
                let raw = &mut h.as_old_mut().name;
                raw.fill(0);
                raw[..name.len()].copy_from_slice(name.as_bytes());
            }
            h.set_entry_type(*kind);
            h.set_mode(if *kind == tar::EntryType::Directory {
                0o755
            } else {
                0o644
            });
            h.set_size(data.len() as u64);
            if let Some(l) = link {
                let raw = &mut h.as_old_mut().linkname;
                raw.fill(0);
                raw[..l.len()].copy_from_slice(l.as_bytes());
            }
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }

    const SUMS: &str = "\
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  empty.txt
2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824 *hello.txt

# comment
";

    #[test]
    fn sha256_of_a_file() {
        let d = tmpdir("sha");
        let f = d.join("hello.txt");
        fs::write(&f, "hello").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn parses_sha256sums() {
        let sums = parse_sha256sums(SUMS).unwrap();
        assert_eq!(sums.len(), 2);
        assert_eq!(sums[0].1, "empty.txt");
        // Binary-mode `*` and uppercase hex are normalised.
        assert_eq!(sums[1].1, "hello.txt");
        assert!(sums[1].0.starts_with("2cf24dba"));
        assert!(parse_sha256sums("").unwrap().is_empty());
    }

    #[test]
    fn rejects_corrupted_sha256sums() {
        for bad in [
            "nothex  file",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85  short.txt",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 x",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85z  bad.txt",
        ] {
            assert!(parse_sha256sums(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn verifies_against_sha256sums() {
        let d = tmpdir("verify");
        let f = d.join("download");
        fs::write(&f, "hello").unwrap();
        verify_sha256sums(SUMS, "hello.txt", &f).unwrap();

        let err = verify_sha256sums(SUMS, "empty.txt", &f).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");

        let err = verify_sha256sums(SUMS, "missing.txt", &f).unwrap_err();
        assert!(err.to_string().contains("not listed"), "{err}");

        // A line that's corrupted anywhere in the file fails verification.
        let corrupted = format!("{SUMS}garbage\n");
        assert!(verify_sha256sums(&corrupted, "hello.txt", &f).is_err());

        // Conflicting duplicates are refused; identical ones are fine.
        let dup = format!("{SUMS}{}  hello.txt\n", "0".repeat(64));
        assert!(expected_sha256(&dup, "hello.txt").is_err());
        let same = format!(
            "{SUMS}2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  hello.txt\n"
        );
        verify_sha256sums(&same, "hello.txt", &f).unwrap();
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn tag_from_latest_redirect() {
        let t = |s| tag_from_location(s);
        assert_eq!(
            t("https://github.com/JoshCu/ngiab-rs/releases/tag/v0.2.0").as_deref(),
            Some("v0.2.0")
        );
        assert_eq!(
            t("https://github.com/o/r/releases/tag/v1.0.0-rc.1/").as_deref(),
            Some("v1.0.0-rc.1")
        );
        assert_eq!(
            t("https://github.com/o/r/releases/tag/v1.0.0%2Bbuild?x=1#y").as_deref(),
            Some("v1.0.0+build")
        );
        // No release: GitHub redirects to the releases list.
        assert_eq!(t("https://github.com/o/r/releases"), None);
        assert_eq!(t("https://github.com/o/r/releases/tag/"), None);
        assert_eq!(t("https://github.com/o/r/releases/tag/%zz"), None);
    }

    #[test]
    fn release_tag_versions() {
        let v = |s| tag_version(s).map(|v| v.to_string());
        assert_eq!(v("v0.2.0").as_deref(), Some("0.2.0"));
        assert_eq!(v("0.2.0").as_deref(), Some("0.2.0"));
        assert_eq!(v("v0.2.0-rc1").as_deref(), Some("0.2.0-rc1"));
        assert_eq!(v("models-2026.11.23"), None);
        assert_eq!(v("v0.2"), None);
        assert!(tag_version("v0.2.0-rc2") > tag_version("v0.2.0-rc1"));
        assert!(tag_version("v0.2.0") > tag_version("v0.2.0-rc2"));
    }

    #[test]
    fn token_only_for_the_api_host() {
        assert!(is_github_api("https://api.github.com/repos/o/r/releases"));
        assert!(is_github_api("https://api.github.com"));
        assert!(!is_github_api("https://api.github.com.evil.example/x"));
        assert!(!is_github_api(
            "https://github.com/o/r/releases/download/v1/x.tar.gz"
        ));
        assert!(!is_github_api(
            "https://objects.githubusercontent.com/github-production-release-asset/x"
        ));
        assert!(!is_github_api("http://api.github.com/repos"));
    }

    #[test]
    fn download_urls_and_part_path() {
        assert_eq!(
            latest_download_url("o/r", "a.tar.gz"),
            "https://github.com/o/r/releases/latest/download/a.tar.gz"
        );
        assert_eq!(
            release_download_url("o/r", "v1.2.3", "SHA256SUMS"),
            "https://github.com/o/r/releases/download/v1.2.3/SHA256SUMS"
        );
        assert_eq!(
            part_path(Path::new("/a/b/x.tar.gz")),
            Path::new("/a/b/x.tar.gz.part")
        );
    }

    #[test]
    fn progress_line_formats() {
        assert_eq!(
            progress_line("x", 512 * 1024, Some(1024 * 1024)),
            "x: 0.5 / 1.0 MiB (50%)"
        );
        assert_eq!(progress_line("x", 1024 * 1024, None), "x: 1.0 MiB");
    }

    #[test]
    fn release_json_parses() {
        let json = r#"[{"tag_name":"models-2026.11.23","draft":false,"prerelease":false,
            "name":"x","assets":[{"name":"SHA256SUMS","size":12,
            "browser_download_url":"https://github.com/o/r/releases/download/models-2026.11.23/SHA256SUMS",
            "id":1}]},{"tag_name":"v0.1.0"}]"#;
        let rels: Vec<Release> = serde_json::from_str(json).unwrap();
        assert_eq!(rels.len(), 2);
        assert_eq!(rels[0].asset("SHA256SUMS").unwrap().size, 12);
        assert!(rels[0].asset("nope").is_none());
        assert!(rels[1].assets.is_empty() && !rels[1].draft);
    }

    #[test]
    fn link_containment() {
        let p = Path::new;
        assert!(link_stays_inside(
            p("dmod/shared_libs"),
            p("libcfebmi.so.1.0")
        ));
        assert!(link_stays_inside(p("a/b"), p("../c/./d")));
        assert!(link_stays_inside(p("a/b"), p("../../x")));
        assert!(!link_stays_inside(p("a/b"), p("../../../x")));
        assert!(!link_stays_inside(p(""), p("../x")));
        assert!(!link_stays_inside(p("a"), p("/etc/passwd")));
    }

    #[test]
    fn extracts_a_safe_archive_with_symlinks() {
        use tar::EntryType::*;
        let d = tmpdir("safe");
        let ar = d.join("ok.tar.gz");
        write_tar_gz(
            &ar,
            &[
                ("dmod/", Directory, None, b""),
                ("dmod/shared_libs/", Directory, None, b""),
                ("dmod/shared_libs/libx.so.1.0", Regular, None, b"elf"),
                (
                    "dmod/shared_libs/libx.so",
                    Symlink,
                    Some("libx.so.1.0"),
                    b"",
                ),
                ("dmod/hard", Link, Some("dmod/shared_libs/libx.so.1.0"), b""),
                ("./manifest.json", Regular, None, b"{}"),
            ],
        );
        let out = d.join("out");
        extract_tar_gz(&ar, &out).unwrap();
        let lib = out.join("dmod/shared_libs/libx.so");
        assert_eq!(fs::read(&lib).unwrap(), b"elf");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(&lib).unwrap(),
            Path::new("libx.so.1.0"),
            "symlink preserved"
        );
        assert_eq!(fs::read(out.join("dmod/hard")).unwrap(), b"elf");
        assert_eq!(fs::read(out.join("manifest.json")).unwrap(), b"{}");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn rejects_unsafe_archives_before_writing() {
        use tar::EntryType::*;
        let d = tmpdir("unsafe");
        // (case, entries, expected error text)
        let cases: &[(&str, Vec<Spec>, &str)] = &[
            ("dotdot", vec![("../evil", Regular, None, b"x")], "`..`"),
            (
                "nested-dotdot",
                vec![("a/../../evil", Regular, None, b"x")],
                "`..`",
            ),
            (
                "absolute",
                vec![("/tmp/evil", Regular, None, b"x")],
                "absolute",
            ),
            (
                "symlink-out",
                vec![("a/link", Symlink, Some("../../etc"), b"")],
                "outside",
            ),
            (
                "symlink-abs",
                vec![("link", Symlink, Some("/etc/passwd"), b"")],
                "outside",
            ),
            (
                "hardlink-out",
                vec![("h", Link, Some("../x"), b"")],
                "outside",
            ),
            ("fifo", vec![("p", Fifo, None, b"")], "unsupported"),
        ];
        for (name, entries, want) in cases {
            // A harmless first entry proves nothing is written before the check.
            let mut all: Vec<Spec> = vec![("first.txt", Regular, None, b"1")];
            all.extend(entries.iter().cloned());
            let ar = d.join(format!("{name}.tar.gz"));
            write_tar_gz(&ar, &all);
            let out = d.join(format!("out-{name}"));
            let err = extract_tar_gz(&ar, &out).expect_err(name).to_string();
            assert!(err.contains(want), "{name}: {err}");
            assert!(!out.exists(), "{name}: wrote output before failing: {err}");
        }
        assert!(!d.parent().unwrap().join("evil").exists());
        fs::remove_dir_all(d).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn zip_extraction() {
        use std::io::Write as _;
        let write_zip = |path: &Path, names: &[&str]| {
            let mut w = zip::ZipWriter::new(File::create(path).unwrap());
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for n in names {
                w.start_file(*n, opts).unwrap();
                w.write_all(b"x").unwrap();
            }
            w.finish().unwrap();
        };
        let d = tmpdir("zip");
        let ok = d.join("ok.zip");
        write_zip(&ok, &["ngiab-rs.exe", "README.md"]);
        extract_zip(&ok, &d.join("out")).unwrap();
        assert_eq!(fs::read(d.join("out/ngiab-rs.exe")).unwrap(), b"x");
        for (i, bad) in ["../evil", "C:/evil", "/evil", "a\\..\\..\\evil"]
            .iter()
            .enumerate()
        {
            let ar = d.join(format!("bad{i}.zip"));
            write_zip(&ar, &["first.txt", bad]);
            let out = d.join(format!("out-bad{i}"));
            assert!(extract_zip(&ar, &out).is_err(), "{bad}");
            assert!(!out.exists(), "{bad}");
        }
        fs::remove_dir_all(d).unwrap();
    }

    /// Real network smoke test (not run by default):
    /// `cargo test net::tests::network_smoke -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs the network"]
    fn network_smoke() {
        let repo = "cli/cli";
        let rels = github_releases(repo).unwrap();
        assert!(rels.len() > PER_PAGE, "pagination: {} releases", rels.len());
        let latest = github_release(repo, ReleaseRef::Latest).unwrap();
        assert_eq!(latest_tag_via_redirect(repo).unwrap(), latest.tag_name);
        let tagged = github_release(repo, ReleaseRef::Tag(&latest.tag_name)).unwrap();
        assert_eq!(tagged, latest);
        eprintln!(
            "{repo}: {} releases, latest {}",
            rels.len(),
            latest.tag_name
        );

        let ver = latest.tag_name.trim_start_matches('v');
        let asset = latest
            .asset(&format!("gh_{ver}_linux_amd64.tar.gz"))
            .unwrap();
        let sums = latest.asset(&format!("gh_{ver}_checksums.txt")).unwrap();
        let sums = get_text(&sums.browser_download_url).unwrap();
        let d = tmpdir("smoke");
        let dest = d.join(&asset.name);
        let n = download(&asset.browser_download_url, &dest, true).unwrap();
        assert_eq!(n, asset.size);
        verify_sha256sums(&sums, &asset.name, &dest).unwrap();
        extract_tar_gz(&dest, &d.join("out")).unwrap();
        assert!(d.join(format!("out/gh_{ver}_linux_amd64/bin/gh")).is_file());
        fs::remove_dir_all(d).unwrap();

        // A repo without releases.
        assert!(github_releases("CIROH-UA/rs_route").unwrap().is_empty());
        let err = github_release("CIROH-UA/rs_route", ReleaseRef::Latest).unwrap_err();
        eprintln!("expected error: {err:#}");
        assert!(latest_tag_via_redirect("CIROH-UA/rs_route").is_err());
    }

    #[test]
    fn download_failure_leaves_no_part_file() {
        let d = tmpdir("dl");
        let dest = d.join("x");
        // Nothing listens on port 1; the connection is refused at once.
        assert!(download("http://127.0.0.1:1/x", &dest, false).is_err());
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
        fs::remove_dir_all(d).unwrap();
    }
}
