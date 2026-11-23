//! Mapping container paths (as written in NGIAB realizations and model configs)
//! onto this machine (tasks.md C.2).
//!
//! Container paths are POSIX *strings*, never `Path`s: on Windows
//! `Path::new("/dmod/x").is_absolute()` is false. A prefix only matches on a path
//! component boundary, so `/database` is not `/data`.
//!
//! | input                                 | docker/podman  | local                          |
//! |---------------------------------------|----------------|--------------------------------|
//! | relative (`./x`, `x`)                 | `data_dir/x`   | `data_dir/x`                   |
//! | `/data/...`, `/ngen/ngen/data/...`    | `data_dir/...` | `data_dir/...`                 |
//! | bundle prefixes (`/dmod/...`, ...)    | `InImage`      | `model_root/dmod/...`          |
//! | other absolute                        | `InImage`      | literal                        |

use crate::backend::{BackendKind, CONTAINER_DATA};
use crate::config::normalise_rel;
use std::path::{Path, PathBuf};

/// Container paths that always mean the data folder the user picked.
pub const DATA_PREFIXES: [&str; 2] = [CONTAINER_DATA, "/ngen/ngen/data"];

/// Where a path from a realization or model config lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// On this machine.
    Host(PathBuf),
    /// Inside the container image; can't be checked from the host.
    InImage,
    /// A bundle path on a local run without a model bundle (the literal path).
    Unmapped(PathBuf),
}

impl Resolved {
    pub fn host(self) -> Option<PathBuf> {
        match self {
            Resolved::Host(p) => Some(p),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PathMap {
    pub kind: BackendKind,
    pub data_dir: PathBuf,
    /// Where container paths under `prefixes` live (local backend). `/` inside our own
    /// image, where `/dmod` is real and the mapping is the identity.
    pub model_root: Option<PathBuf>,
    /// Container prefixes provided by the model bundle (`/dmod`, `/ngen/ngen/extern`).
    pub prefixes: Vec<String>,
}

/// `p` without `prefix`, if `prefix` matches a whole leading component sequence:
/// the rest is empty or starts with `/`.
fn strip_component_prefix<'a>(p: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = p.strip_prefix(prefix)?;
    (rest.is_empty() || rest.starts_with('/')).then_some(rest)
}

/// Characters that may precede a path inside a config file for it to be rewritten.
fn is_text_boundary_before(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '=' | ':' | '[' | ',')
}

/// Characters that may follow a prefix inside a config file (besides `/`).
fn is_text_boundary_after(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ']' | ';')
}

/// `p` as a string without a trailing separator (unless it is the root).
fn path_str(p: &Path) -> String {
    let s = p.display().to_string();
    let t = s.trim_end_matches('/');
    if t.is_empty() { s } else { t.to_string() }
}

impl PathMap {
    fn is_local(&self) -> bool {
        self.kind == BackendKind::Local
    }

    /// The bundle prefix `p` starts with, if any (longest first).
    fn model_prefix<'a>(&'a self, p: &str) -> Option<&'a str> {
        self.prefixes
            .iter()
            .filter(|pre| strip_component_prefix(p, pre).is_some())
            .max_by_key(|pre| pre.len())
            .map(String::as_str)
    }

    /// Does `p` start with one of the bundle prefixes?
    pub fn is_model_path(&self, p: &str) -> bool {
        self.model_prefix(p).is_some()
    }

    /// Where `p` lives on this machine (see the module docs).
    pub fn resolve(&self, p: &str) -> Resolved {
        if !p.starts_with('/') {
            let rel = normalise_rel(p);
            return Resolved::Host(if rel.is_empty() || rel == "." {
                self.data_dir.clone()
            } else {
                self.data_dir.join(rel)
            });
        }
        for prefix in DATA_PREFIXES {
            if let Some(rest) = strip_component_prefix(p, prefix) {
                let rest = rest.trim_start_matches('/');
                return Resolved::Host(if rest.is_empty() {
                    self.data_dir.clone()
                } else {
                    self.data_dir.join(normalise_rel(rest))
                });
            }
        }
        if !self.is_local() {
            return Resolved::InImage;
        }
        if self.is_model_path(p) {
            return match &self.model_root {
                Some(root) => Resolved::Host(root.join(p.trim_start_matches('/'))),
                None => Resolved::Unmapped(PathBuf::from(p)),
            };
        }
        Resolved::Host(PathBuf::from(p))
    }

    /// Local runs: the host string for a container path `s` that starts with a data
    /// or bundle prefix. `None` when nothing changes (container backends, other paths,
    /// no bundle, or the identity mapping inside our own image).
    pub fn rewrite_str(&self, s: &str) -> Option<String> {
        if !self.is_local() {
            return None;
        }
        let out = self.map_prefix(s)?;
        (out != s).then_some(out)
    }

    /// The mapped form of `s` if it starts (at a component boundary) with a mapped
    /// prefix, whether or not that changes it.
    fn map_prefix(&self, s: &str) -> Option<String> {
        for prefix in DATA_PREFIXES {
            if let Some(rest) = strip_component_prefix(s, prefix) {
                return Some(format!("{}{rest}", path_str(&self.data_dir)));
            }
        }
        let prefix = self.model_prefix(s)?;
        let root = path_str(self.model_root.as_ref()?);
        let root = root.trim_end_matches('/');
        Some(format!("{root}{prefix}{}", &s[prefix.len()..]))
    }

    /// Local runs: rewrite every container path inside a config file's text. A path
    /// only matches after the start of a line, whitespace, a quote, `=`, `:`, `[` or
    /// `,`, and its prefix must end at `/`, whitespace, a quote, `,`, `]`, `;` or the
    /// end of the text. `None` when nothing changes.
    pub fn rewrite_text(&self, text: &str) -> Option<String> {
        if !self.is_local() {
            return None;
        }
        let mut out = String::with_capacity(text.len() + 64);
        let mut last = 0;
        let mut changed = false;
        for (i, _) in text.match_indices('/') {
            if i < last {
                continue;
            }
            let before_ok = text[..i]
                .chars()
                .next_back()
                .is_none_or(is_text_boundary_before);
            if !before_ok {
                continue;
            }
            let Some((prefix, mapped)) = self.text_prefix_at(&text[i..]) else {
                continue;
            };
            out.push_str(&text[last..i]);
            out.push_str(&mapped);
            last = i + prefix.len();
            changed |= mapped != prefix;
        }
        if !changed {
            return None;
        }
        out.push_str(&text[last..]);
        Some(out)
    }

    /// At the start of `s`: (the matched prefix, its replacement).
    fn text_prefix_at<'a>(&self, s: &'a str) -> Option<(&'a str, String)> {
        let ends_ok = |prefix: &str| {
            s.strip_prefix(prefix).is_some_and(|rest| {
                rest.chars()
                    .next()
                    .is_none_or(|c| c == '/' || is_text_boundary_after(c))
            })
        };
        for prefix in DATA_PREFIXES {
            if ends_ok(prefix) {
                return Some((&s[..prefix.len()], path_str(&self.data_dir)));
            }
        }
        let root = path_str(self.model_root.as_ref()?);
        let root = root.trim_end_matches('/');
        let prefix = self
            .prefixes
            .iter()
            .filter(|p| ends_ok(p))
            .max_by_key(|p| p.len())?;
        Some((&s[..prefix.len()], format!("{root}{prefix}")))
    }

    /// Container-style paths (`/dmod/...`, `/ngen/...`, data and bundle prefixes)
    /// mentioned in a config file's text, using the same boundaries as
    /// [`rewrite_text`](Self::rewrite_text).
    pub fn container_paths_in(&self, text: &str) -> Vec<String> {
        let mut v = Vec::new();
        for (i, _) in text.match_indices('/') {
            let before_ok = text[..i]
                .chars()
                .next_back()
                .is_none_or(is_text_boundary_before);
            if !before_ok {
                continue;
            }
            let rest = &text[i..];
            let interesting = rest.starts_with("/ngen/")
                || rest.starts_with("/dmod/")
                || DATA_PREFIXES
                    .iter()
                    .any(|p| strip_component_prefix(rest, p).is_some())
                || self.is_model_path(rest);
            if !interesting {
                continue;
            }
            let end = rest
                .find(|c: char| is_text_boundary_after(c) || c == '#' || c == '!')
                .unwrap_or(rest.len());
            let token = rest[..end].to_string();
            if !v.contains(&token) {
                v.push(token);
            }
        }
        v
    }

    /// Reverse of the bundle half of [`rewrite_str`](Self::rewrite_str):
    /// `model_root/dmod/x` back to `/dmod/x` (for writing a realization back).
    pub fn unrewrite_str(&self, s: &str) -> Option<String> {
        if !self.is_local() {
            return None;
        }
        let root = path_str(self.model_root.as_ref()?);
        let root = root.trim_end_matches('/');
        if root.is_empty() {
            return None; // identity mapping (our own image)
        }
        let rest = strip_component_prefix(s, root)?;
        self.is_model_path(rest).then(|| rest.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(kind: BackendKind, data: &str, root: Option<&str>) -> PathMap {
        PathMap {
            kind,
            data_dir: data.into(),
            model_root: root.map(PathBuf::from),
            prefixes: vec!["/dmod".into(), "/ngen/ngen/extern".into()],
        }
    }

    // Moved from preflight.rs (host_path), with the local `/data` case changed:
    // on local `/data` is always the data folder.
    #[test]
    fn resolve_container_and_local() {
        let d = Path::new("/home/me/run");
        let docker = map(BackendKind::Docker, "/home/me/run", None);
        let local = map(
            BackendKind::Local,
            "/home/me/run",
            Some("/home/me/.ngiab/container"),
        );
        assert_eq!(
            docker.resolve("./forcings/f.nc"),
            Resolved::Host(d.join("forcings/f.nc"))
        );
        assert_eq!(
            docker.resolve("/ngen/ngen/data/config/x.yml"),
            Resolved::Host(d.join("config/x.yml"))
        );
        assert_eq!(docker.resolve("/dmod/x.so"), Resolved::InImage);
        assert_eq!(
            local.resolve("/dmod/x.so"),
            Resolved::Host("/home/me/.ngiab/container/dmod/x.so".into())
        );
        // component-wise prefix: /database is not /data
        assert_eq!(docker.resolve("/database/x"), Resolved::InImage);
        assert_eq!(
            local.resolve("/database/x"),
            Resolved::Host("/database/x".into())
        );
        assert_eq!(local.resolve("/dmodx/y"), Resolved::Host("/dmodx/y".into()));
        // in-image absolute paths are never mapped into the data folder (on any OS)
        assert_eq!(docker.resolve("/dev/null"), Resolved::InImage);
        assert_eq!(
            local.resolve("/dev/null"),
            Resolved::Host("/dev/null".into())
        );
        assert_eq!(docker.resolve("/data"), Resolved::Host(d.into()));
        assert_eq!(docker.resolve("/data/"), Resolved::Host(d.into()));
        assert_eq!(
            docker.resolve("/ngen/ngen/data/outputs/ngen"),
            Resolved::Host(d.join("outputs").join("ngen"))
        );
        // local: /data always is the picked folder
        assert_eq!(local.resolve("/data/x"), Resolved::Host(d.join("x")));
        assert_eq!(
            local.resolve("/ngen/ngen/data/outputs/ngen/"),
            Resolved::Host(d.join("outputs/ngen"))
        );
        // extern prefix
        assert_eq!(
            local.resolve("/ngen/ngen/extern/lstm/m/config.yml"),
            Resolved::Host("/home/me/.ngiab/container/ngen/ngen/extern/lstm/m/config.yml".into())
        );
        assert_eq!(
            local.resolve("/ngen/ngen/externals"),
            Resolved::Host("/ngen/ngen/externals".into())
        );
        // no bundle
        let bare = map(BackendKind::Local, "/home/me/run", None);
        assert_eq!(
            bare.resolve("/dmod/x.so"),
            Resolved::Unmapped("/dmod/x.so".into())
        );
        assert_eq!(bare.resolve("."), Resolved::Host(d.into()));
    }

    #[test]
    fn spaces_and_windows_data_dirs() {
        let local = map(
            BackendKind::Local,
            "/home/me/my run",
            Some("/home/me/a b/container"),
        );
        assert_eq!(
            local.resolve("./forcings/my file.nc"),
            Resolved::Host("/home/me/my run/forcings/my file.nc".into())
        );
        assert_eq!(
            local
                .rewrite_str("/dmod/shared_libs/libcfebmi.so")
                .as_deref(),
            Some("/home/me/a b/container/dmod/shared_libs/libcfebmi.so")
        );
        let win = map(BackendKind::Docker, r"C:\Users\me\run", None);
        assert_eq!(
            win.resolve("/data/outputs/ngen"),
            Resolved::Host(Path::new(r"C:\Users\me\run").join("outputs/ngen"))
        );
        assert_eq!(win.resolve("/dev/null"), Resolved::InImage);
        assert_eq!(
            win.resolve("config/x.ini"),
            Resolved::Host(Path::new(r"C:\Users\me\run").join("config/x.ini"))
        );
    }

    #[test]
    fn rewrite_and_unrewrite_strings() {
        let local = map(BackendKind::Local, "/home/me/run/", Some("/b/container/"));
        assert_eq!(
            local.rewrite_str("/dmod/shared_libs/x.so").as_deref(),
            Some("/b/container/dmod/shared_libs/x.so")
        );
        assert_eq!(
            local.rewrite_str("/dmod").as_deref(),
            Some("/b/container/dmod")
        );
        assert_eq!(
            local.rewrite_str("/ngen/ngen/data/outputs/ngen").as_deref(),
            Some("/home/me/run/outputs/ngen")
        );
        assert_eq!(local.rewrite_str("/data").as_deref(), Some("/home/me/run"));
        assert_eq!(local.rewrite_str("/database"), None);
        assert_eq!(local.rewrite_str("./config/x"), None);
        assert_eq!(local.rewrite_str("/dev/null"), None);
        assert_eq!(
            local
                .unrewrite_str("/b/container/dmod/shared_libs/x.so")
                .as_deref(),
            Some("/dmod/shared_libs/x.so")
        );
        assert_eq!(local.unrewrite_str("/b/container/other"), None);
        assert_eq!(local.unrewrite_str("/b/containers/dmod/x"), None);
        // container backends never rewrite
        let docker = map(BackendKind::Docker, "/home/me/run", Some("/b"));
        assert_eq!(docker.rewrite_str("/dmod/x"), None);
        assert_eq!(docker.rewrite_text("p = \"/dmod/x\""), None);
        // inside our own image the bundle mapping is the identity
        let image = map(BackendKind::Local, "/data", Some("/"));
        assert_eq!(image.rewrite_str("/dmod/shared_libs/x.so"), None);
        assert_eq!(image.rewrite_str("/data/outputs"), None);
        assert_eq!(
            image.rewrite_text("parameter_dir = \"/dmod/datasets/\""),
            None
        );
        assert_eq!(image.unrewrite_str("/dmod/x"), None);
        assert_eq!(image.resolve("/dmod/x"), Resolved::Host("/dmod/x".into()));
        // no bundle: bundle prefixes stay
        let bare = map(BackendKind::Local, "/r", None);
        assert_eq!(bare.rewrite_str("/dmod/x"), None);
        assert_eq!(bare.rewrite_str("/data/x").as_deref(), Some("/r/x"));
    }

    #[test]
    fn rewrite_text_boundaries() {
        let local = map(BackendKind::Local, "/home/me/run", Some("/b c/container"));
        let noah = "  parameter_dir      = \"/dmod/datasets/static/\"\n  x = data/config/a\n";
        assert_eq!(
            local.rewrite_text(noah).unwrap(),
            "  parameter_dir      = \"/b c/container/dmod/datasets/static/\"\n  x = data/config/a\n"
        );
        let yaml = "train_cfg_file:\n  - /ngen/ngen/extern/lstm/m1/config.yml\n- /ngen/ngen/extern/lstm/m2/config.yml\n";
        assert_eq!(
            local.rewrite_text(yaml).unwrap(),
            "train_cfg_file:\n  - /b c/container/ngen/ngen/extern/lstm/m1/config.yml\n- /b c/container/ngen/ngen/extern/lstm/m2/config.yml\n"
        );
        // start of text, after '=' / ':' / '[' / ',' / quote
        assert_eq!(
            local.rewrite_text("/dmod/a").unwrap(),
            "/b c/container/dmod/a"
        );
        assert_eq!(
            local
                .rewrite_text("k=/dmod/a,/data/f [/dmod]:'/dmod/z'")
                .unwrap(),
            "k=/b c/container/dmod/a,/home/me/run/f [/b c/container/dmod]:'/b c/container/dmod/z'"
        );
        // not at a boundary
        assert_eq!(local.rewrite_text("x/dmod/a ./dmod/b"), None);
        assert_eq!(
            local.rewrite_text("p=/dmodx /database /ngen/ngen/externals"),
            None
        );
        assert_eq!(local.rewrite_text("nothing here"), None);
        // longest prefix wins and paths keep their tails
        let both = PathMap {
            prefixes: vec!["/ngen".into(), "/ngen/ngen/extern".into()],
            ..local.clone()
        };
        assert_eq!(
            both.rewrite_str("/ngen/ngen/extern/x").as_deref(),
            Some("/b c/container/ngen/ngen/extern/x")
        );
    }

    #[test]
    fn container_paths_in_text() {
        let local = map(BackendKind::Local, "/r", None);
        let text = "a = \"/dmod/datasets/static/\" ! c\nb: /ngen/ngen/extern/cfe/x.ini\nc=./rel /dev/null\nd: /data/forcings/x.nc";
        assert_eq!(
            local.container_paths_in(text),
            vec![
                "/dmod/datasets/static/",
                "/ngen/ngen/extern/cfe/x.ini",
                "/data/forcings/x.nc"
            ]
        );
    }
}
