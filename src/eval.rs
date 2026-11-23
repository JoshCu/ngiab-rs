//! Evaluation: load rs-route NetCDF hydrographs and bmi-driver catchment CSVs.
//! Deliberately simple; intended to be extended (observations, metrics, ...).

use anyhow::{Context, Result, bail};
use chrono::NaiveDateTime;
use std::path::{Path, PathBuf};

/// A single time series ready to plot. Times are unix seconds (UTC, naive).
#[derive(Debug, Clone, Default)]
pub struct Series {
    pub title: String,
    pub units: String,
    pub t: Vec<i64>,
    pub y: Vec<f64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub peak_t: i64,
    pub n: usize,
}

impl Series {
    pub fn stats(&self) -> Option<Stats> {
        let mut s = Stats { min: f64::INFINITY, max: f64::NEG_INFINITY, ..Default::default() };
        let mut sum = 0.0;
        for (&t, &y) in self.t.iter().zip(&self.y) {
            if !y.is_finite() {
                continue;
            }
            if y > s.max {
                s.max = y;
                s.peak_t = t;
            }
            s.min = s.min.min(y);
            sum += y;
            s.n += 1;
        }
        (s.n > 0).then(|| {
            s.mean = sum / s.n as f64;
            s
        })
    }

    /// Min/max decimation into at most `2 * buckets` points; keeps peaks visible
    /// when a multi-year hourly series is drawn into a few hundred columns.
    pub fn decimated(&self, buckets: usize) -> Vec<(f64, f64)> {
        let pts: Vec<(f64, f64)> = self
            .t
            .iter()
            .zip(&self.y)
            .filter(|(_, y)| y.is_finite())
            .map(|(&t, &y)| (t as f64, y))
            .collect();
        let buckets = buckets.max(1);
        if pts.len() <= buckets * 2 {
            return pts;
        }
        let per = pts.len().div_ceil(buckets);
        let mut out = Vec::with_capacity(buckets * 2);
        for chunk in pts.chunks(per) {
            let (mut lo, mut hi) = (chunk[0], chunk[0]);
            for &p in chunk {
                if p.1 < lo.1 {
                    lo = p;
                }
                if p.1 > hi.1 {
                    hi = p;
                }
            }
            if lo.0 <= hi.0 {
                out.push(lo);
                if hi.0 != lo.0 {
                    out.push(hi);
                }
            } else {
                out.push(hi);
                out.push(lo);
            }
        }
        out
    }
}

pub fn fmt_time(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| t.to_string())
}

fn parse_dt(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d_%H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(d);
        }
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

// ---------------------------------------------------------------------------
// rs-route NetCDF
// ---------------------------------------------------------------------------

pub const ROUTE_VARS: [&str; 3] = ["flow", "velocity", "depth"];

/// Metadata of an rs-route output file. The file itself is only opened while
/// reading, so it is never held open while rs-route rewrites it.
pub struct TrouteFile {
    path: PathBuf,
    pub feature_ids: Vec<i64>,
    pub times: Vec<i64>,
    pub vars: Vec<String>,
}

fn str_attr(var: &netcdf::Variable, name: &str) -> Option<String> {
    match var.attribute_value(name)?.ok()? {
        netcdf::AttributeValue::Str(s) => Some(s),
        netcdf::AttributeValue::Strs(v) => v.into_iter().next(),
        _ => None,
    }
}

impl TrouteFile {
    pub fn open(path: &Path) -> Result<Self> {
        let file = netcdf::open(path).with_context(|| format!("opening {}", path.display()))?;
        let fid = file.variable("feature_id").context("no feature_id variable")?;
        let feature_ids: Vec<i64> = fid.get_values::<i64, _>(..)?;
        let tvar = file.variable("time").context("no time variable")?;
        let raw: Vec<f64> = tvar.get_values::<f64, _>(..)?;
        let units = str_attr(&tvar, "units").unwrap_or_default();
        // "seconds since 2008-10-01 00:00:00"
        let (scale, base) = match units.split_once(" since ") {
            Some((u, b)) => {
                let scale = match u.trim() {
                    "seconds" | "second" | "s" => 1.0,
                    "minutes" | "minute" => 60.0,
                    "hours" | "hour" | "h" => 3600.0,
                    "days" | "day" => 86400.0,
                    other => bail!("unsupported time units '{other}'"),
                };
                let base = parse_dt(b).with_context(|| format!("bad time origin '{b}'"))?;
                (scale, base.and_utc().timestamp())
            }
            None => (1.0, 0),
        };
        let times = raw.iter().map(|v| base + (v * scale).round() as i64).collect();
        let vars = ROUTE_VARS
            .iter()
            .filter(|v| file.variable(v).is_some())
            .map(|v| v.to_string())
            .collect();
        Ok(Self { path: path.to_path_buf(), feature_ids, times, vars })
    }

    pub fn series(&self, feature_idx: usize, var: &str) -> Result<Series> {
        let file = netcdf::open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let v = file.variable(var).with_context(|| format!("no variable {var}"))?;
        let units = str_attr(&v, "units").unwrap_or_default();
        let fill = v.fill_value::<f32>().ok().flatten().unwrap_or(-9999.0);
        let ext: [netcdf::Extent; 2] = [feature_idx.into(), (..).into()];
        let vals: Vec<f32> = v.get_values::<f32, _>(ext)?;
        let y = vals
            .into_iter()
            .map(|x| if x == fill || x <= -9999.0 { f64::NAN } else { x as f64 })
            .collect();
        Ok(Series {
            title: format!("wb-{} {var}", self.feature_ids[feature_idx]),
            units,
            t: self.times.clone(),
            y,
        })
    }
}

pub fn list_troute(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "nc"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// bmi-driver catchment CSVs: "Time Step,Time,<var>,<var>..."
// ---------------------------------------------------------------------------

pub struct CatCsv {
    pub id: String,
    pub columns: Vec<String>,
    pub times: Vec<i64>,
    pub data: Vec<Vec<f64>>,
}

impl CatCsv {
    pub fn load(path: &Path) -> Result<Self> {
        let mut rdr = csv::ReaderBuilder::new()
            .trim(csv::Trim::All)
            .from_path(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let headers = rdr.headers()?.clone();
        let time_col = headers.iter().position(|h| h.eq_ignore_ascii_case("time"));
        let skip = |i: usize, h: &str| Some(i) == time_col || h.eq_ignore_ascii_case("time step");
        let value_cols: Vec<usize> =
            headers.iter().enumerate().filter(|(i, h)| !skip(*i, h)).map(|(i, _)| i).collect();
        let columns = value_cols.iter().map(|&i| headers[i].to_string()).collect();
        let mut times = Vec::new();
        let mut data = vec![Vec::new(); value_cols.len()];
        for (row_no, rec) in rdr.records().enumerate() {
            let rec = rec?;
            let t = time_col
                .and_then(|c| rec.get(c))
                .and_then(parse_dt)
                .map(|d| d.and_utc().timestamp())
                .unwrap_or(row_no as i64 * 3600);
            times.push(t);
            for (k, &c) in value_cols.iter().enumerate() {
                data[k].push(rec.get(c).and_then(|s| s.parse().ok()).unwrap_or(f64::NAN));
            }
        }
        let id = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        Ok(Self { id, columns, times, data })
    }

    pub fn series(&self, col: usize) -> Option<Series> {
        Some(Series {
            title: format!("{} {}", self.id, self.columns.get(col)?),
            units: String::new(),
            t: self.times.clone(),
            y: self.data.get(col)?.clone(),
        })
    }
}

/// `(id, path)` for every `cat-*.csv` (also `nex-*`) in `dir`, sorted naturally by id.
pub fn list_catchments(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "csv"))
                .filter_map(|p| {
                    let stem = p.file_stem()?.to_string_lossy().to_string();
                    (stem.starts_with("cat-") || stem.starts_with("nex-")).then_some((stem, p))
                })
                .collect()
        })
        .unwrap_or_default();
    let key = |s: &str| {
        let (pre, num) = s.split_once('-').unwrap_or((s, ""));
        (pre.to_string(), num.parse::<u64>().unwrap_or(u64::MAX), s.to_string())
    };
    v.sort_by_cached_key(|(id, _)| key(id));
    v
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Hydrograph,
    Catchment,
}

pub struct EvalState {
    pub tab: Tab,
    pub troute_dir: PathBuf,
    pub cat_dir: PathBuf,
    // hydrograph
    pub nc_files: Vec<PathBuf>,
    pub nc_idx: usize,
    pub troute: Option<TrouteFile>,
    pub var_idx: usize,
    // catchment
    pub cats: Vec<(String, PathBuf)>,
    pub csv: Option<CatCsv>,
    pub col_idx: usize,
    // shared list
    pub filter: String,
    pub filtering: bool,
    pub selected: usize,
    /// Indices into the current tab's item list after filtering.
    pub visible: Vec<usize>,
    pub series: Option<Series>,
    pub error: Option<String>,
    loaded_key: Option<(Tab, usize, usize, usize)>,
}

impl EvalState {
    pub fn new(troute_dir: PathBuf, cat_dir: PathBuf) -> Self {
        let mut s = Self {
            tab: Tab::Hydrograph,
            nc_files: list_troute(&troute_dir),
            cats: list_catchments(&cat_dir),
            troute_dir,
            cat_dir,
            nc_idx: 0,
            troute: None,
            var_idx: 0,
            csv: None,
            col_idx: 0,
            filter: String::new(),
            filtering: false,
            selected: 0,
            visible: vec![],
            series: None,
            error: None,
            loaded_key: None,
        };
        if s.nc_files.is_empty() && !s.cats.is_empty() {
            s.tab = Tab::Catchment;
        }
        s.open_nc();
        s.refilter();
        s.load_selected();
        s
    }

    fn open_nc(&mut self) {
        self.troute = None;
        if let Some(p) = self.nc_files.get(self.nc_idx) {
            match TrouteFile::open(p) {
                Ok(f) => {
                    self.var_idx = self.var_idx.min(f.vars.len().saturating_sub(1));
                    self.troute = Some(f);
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }
    }

    pub fn item_label(&self, i: usize) -> String {
        match self.tab {
            Tab::Hydrograph => self
                .troute
                .as_ref()
                .and_then(|f| f.feature_ids.get(i))
                .map(|id| format!("wb-{id}"))
                .unwrap_or_default(),
            Tab::Catchment => self.cats.get(i).map(|c| c.0.clone()).unwrap_or_default(),
        }
    }

    fn item_count(&self) -> usize {
        match self.tab {
            Tab::Hydrograph => self.troute.as_ref().map_or(0, |f| f.feature_ids.len()),
            Tab::Catchment => self.cats.len(),
        }
    }

    pub fn refilter(&mut self) {
        let f = self.filter.to_lowercase();
        self.visible = (0..self.item_count())
            .filter(|&i| f.is_empty() || self.item_label(i).to_lowercase().contains(&f))
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    pub fn current_var(&self) -> Option<String> {
        match self.tab {
            Tab::Hydrograph => self.troute.as_ref().and_then(|f| f.vars.get(self.var_idx).cloned()),
            Tab::Catchment => self.csv.as_ref().and_then(|c| c.columns.get(self.col_idx).cloned()),
        }
    }

    pub fn load_selected(&mut self) {
        let Some(&item) = self.visible.get(self.selected) else {
            self.series = None;
            self.loaded_key = None;
            return;
        };
        let key = (self.tab, self.nc_idx, item, if self.tab == Tab::Hydrograph { self.var_idx } else { self.col_idx });
        if self.loaded_key == Some(key) {
            return;
        }
        let res = match self.tab {
            Tab::Hydrograph => match &self.troute {
                Some(f) => match f.vars.get(self.var_idx) {
                    Some(var) => f.series(item, var),
                    None => Err(anyhow::anyhow!("no flow/velocity/depth variables")),
                },
                None => Err(anyhow::anyhow!("no NetCDF open")),
            },
            Tab::Catchment => {
                let (_, path) = &self.cats[item];
                let need_load = self.csv.as_ref().is_none_or(|c| Some(&c.id) != Some(&self.cats[item].0));
                if need_load {
                    match CatCsv::load(path) {
                        Ok(c) => {
                            self.col_idx = self.col_idx.min(c.columns.len().saturating_sub(1));
                            self.csv = Some(c);
                        }
                        Err(e) => {
                            self.csv = None;
                            self.error = Some(format!("{e:#}"));
                            self.series = None;
                            self.loaded_key = None;
                            return;
                        }
                    }
                }
                self.csv
                    .as_ref()
                    .and_then(|c| c.series(self.col_idx))
                    .ok_or_else(|| anyhow::anyhow!("no value columns in csv"))
            }
        };
        match res {
            Ok(s) => {
                self.series = Some(s);
                self.error = None;
            }
            Err(e) => {
                self.series = None;
                self.error = Some(format!("{e:#}"));
            }
        }
        // Recompute the key: loading a csv may have clamped col_idx.
        let var = if self.tab == Tab::Hydrograph { self.var_idx } else { self.col_idx };
        self.loaded_key = Some((self.tab, self.nc_idx, item, var));
    }

    pub fn switch_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Hydrograph => Tab::Catchment,
            Tab::Catchment => Tab::Hydrograph,
        };
        self.filter.clear();
        self.selected = 0;
        self.refilter();
        self.load_selected();
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let n = self.visible.len() as isize;
        self.selected = (self.selected as isize + delta).clamp(0, n - 1) as usize;
        self.load_selected();
    }

    /// Cycle the plotted variable (flow/velocity/depth or csv column).
    pub fn cycle_var(&mut self, delta: isize) {
        let n = match self.tab {
            Tab::Hydrograph => self.troute.as_ref().map_or(0, |f| f.vars.len()),
            Tab::Catchment => self.csv.as_ref().map_or(0, |c| c.columns.len()),
        };
        if n == 0 {
            return;
        }
        let idx = match self.tab {
            Tab::Hydrograph => &mut self.var_idx,
            Tab::Catchment => &mut self.col_idx,
        };
        *idx = (*idx as isize + delta).rem_euclid(n as isize) as usize;
        self.load_selected();
    }

    /// Cycle between multiple troute output files.
    pub fn cycle_file(&mut self) {
        if self.nc_files.len() > 1 && self.tab == Tab::Hydrograph {
            self.nc_idx = (self.nc_idx + 1) % self.nc_files.len();
            self.open_nc();
            self.refilter();
            self.load_selected();
        }
    }

    pub fn reload(&mut self) {
        self.nc_files = list_troute(&self.troute_dir);
        self.cats = list_catchments(&self.cat_dir);
        self.nc_idx = self.nc_idx.min(self.nc_files.len().saturating_sub(1));
        self.csv = None;
        self.loaded_key = None;
        self.open_nc();
        self.refilter();
        self.load_selected();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimation_keeps_extremes() {
        let t: Vec<i64> = (0..10_000).collect();
        let mut y: Vec<f64> = vec![1.0; 10_000];
        y[5123] = 100.0;
        y[777] = -5.0;
        let s = Series { t, y, ..Default::default() };
        let d = s.decimated(100);
        assert!(d.len() <= 200);
        assert!(d.iter().any(|p| p.1 == 100.0));
        assert!(d.iter().any(|p| p.1 == -5.0));
        assert!(d.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn csv_and_listing() {
        let d = std::env::temp_dir().join(format!("ngiab-rs-eval-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("cat-10.csv"),
            "Time Step,Time,Q_OUT,SOIL\n0,2010-01-01 00:00:00,1.5,0.2\n1,2010-01-01 01:00:00,2.5,0.3\n",
        )
        .unwrap();
        std::fs::write(d.join("cat-9.csv"), "Time Step,Time,Q_OUT\n").unwrap();
        std::fs::write(d.join("other.csv"), "x").unwrap();
        let l = list_catchments(&d);
        assert_eq!(l.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["cat-9", "cat-10"]);
        let c = CatCsv::load(&d.join("cat-10.csv")).unwrap();
        assert_eq!(c.columns, ["Q_OUT", "SOIL"]);
        assert_eq!(c.times[1] - c.times[0], 3600);
        let s = c.series(1).unwrap();
        assert_eq!(s.y, vec![0.2, 0.3]);
        let st = s.stats().unwrap();
        assert_eq!(st.max, 0.3);
        let _ = std::fs::remove_dir_all(d);
    }
}
