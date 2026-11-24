//! A loaded analysis, in the shapes the UI draws from. Built on a worker
//! thread from the service's JSON.

use crate::pyramid::Pyramid;
use serde_json::Value;
use vcompare_service::engine::types::TimelineInfo;
use vcompare_service::Service;

/// One metric as parallel arrays, with its pyramid.
pub struct Series {
    pub metric: String,
    pub frames: Vec<i64>,
    pub values: Vec<f64>,
    pub pyramid: Pyramid,
    pub min: f64,
    pub max: f64,
}

impl Series {
    pub fn load(svc: &Service, metric: &str) -> Result<Series, String> {
        let (frames, values) = svc.metric_series(metric)?;
        Ok(Series::from_arrays(metric.to_string(), frames, values))
    }

    /// A series from arrays (min / max and pyramid computed).
    pub fn from_arrays(metric: String, frames: Vec<i64>, values: Vec<f64>) -> Series {
        let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
        for &x in &values {
            if x.is_finite() {
                min = min.min(x);
                max = max.max(x);
            }
        }
        if min > max {
            (min, max) = (0.0, 0.0);
        }
        let pyramid = Pyramid::new(&values);
        Series { metric, frames, values, pyramid, min, max }
    }

    /// The same series through a running median of `window` points (odd;
    /// <= 1 = unchanged). App.svelte's `f` smoothing.
    pub fn smoothed(&self, window: usize) -> Series {
        Series::from_arrays(self.metric.clone(), self.frames.clone(), running_median(&self.values, window))
    }

    /// Index of the first point at or after `frame`.
    pub fn lower_bound(&self, frame: f64) -> usize {
        self.frames.partition_point(|&f| (f as f64) < frame)
    }

    /// The data frame nearest to `frame`.
    pub fn nearest(&self, frame: f64) -> Option<i64> {
        if self.frames.is_empty() {
            return None;
        }
        let i = self.lower_bound(frame);
        let cands = [i.checked_sub(1), Some(i).filter(|&i| i < self.frames.len())];
        cands
            .into_iter()
            .flatten()
            .min_by(|&a, &b| {
                let da = (self.frames[a] as f64 - frame).abs();
                let db = (self.frames[b] as f64 - frame).abs();
                da.partial_cmp(&db).unwrap()
            })
            .map(|i| self.frames[i])
    }

    pub fn value_at(&self, frame: i64) -> Option<f64> {
        self.frames.binary_search(&frame).ok().map(|i| self.values[i])
    }
}

/// Median of each point's neighbourhood (`window` points centred on it,
/// clipped at the ends). Same result as App.svelte's `runningMedian`.
pub fn running_median(values: &[f64], window: usize) -> Vec<f64> {
    if window <= 1 {
        return values.to_vec();
    }
    let half = (window - 1) / 2;
    let mut buf = Vec::with_capacity(window);
    (0..values.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half).min(values.len() - 1);
            buf.clear();
            buf.extend_from_slice(&values[lo..=hi]);
            let mid = buf.len() / 2;
            *buf.select_nth_unstable_by(mid, |a, b| a.total_cmp(b)).1
        })
        .collect()
}

/// Every frame's row for the table, column-wise.
pub struct FrameRows {
    pub frames: Vec<i64>,
    pub tcs: Vec<String>,
    /// (metric, values) in `metrics_available` order; NaN where missing.
    pub columns: Vec<(String, Vec<f64>)>,
}

impl FrameRows {
    fn load(svc: &Service, metrics: &[String]) -> Result<FrameRows, String> {
        let c = svc.frame_columns(metrics)?;
        Ok(FrameRows { frames: c.frames, tcs: c.tcs, columns: c.columns })
    }

    /// Row index of `frame` (rows are in frame order).
    pub fn index_of(&self, frame: i64) -> Option<usize> {
        self.frames.binary_search(&frame).ok()
    }
}

pub struct Loaded {
    /// The analysis file (cache) it lives in; empty for a dry run.
    pub path: String,
    /// The media: reference (or the one file) and distorted ("" for one-file modes).
    pub ref_path: String,
    pub dist_path: String,
    pub summary: Value,
    pub mode: String,
    pub metrics: Vec<String>,
    pub timeline: Option<TimelineInfo>,
    pub rows: FrameRows,
    /// Files in the analysis (thumbnail rows per strip).
    pub files: usize,
}

impl Loaded {
    /// Read a saved analysis.
    pub fn open(svc: &Service, path: &str) -> Result<(Loaded, Series), String> {
        let summary = svc.load_saved_analysis(path)?;
        Self::from_summary(svc, summary, path)
    }

    /// Build from what the engine has loaded (after `analyze` or
    /// `load_saved_analysis`).
    pub fn from_summary(svc: &Service, summary: Value, path: &str) -> Result<(Loaded, Series), String> {
        let mode = summary["mode"].as_str().unwrap_or("diff").to_string();
        let metrics: Vec<String> = summary["metrics_available"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let first = metrics.first().cloned().ok_or("The analysis has no metrics")?;
        let timeline = summary
            .get("timeline")
            .filter(|t| !t.is_null())
            .and_then(|t| serde_json::from_value::<TimelineInfo>(t.clone()).ok())
            // Timeline (Resolve) mode reuses the field for something else.
            .filter(|_| mode != "timeline");
        let (ref_path, dist_path) = svc
            .engine()
            .mpv_context()
            .map(|c| (c.ref_path, c.dist_path))
            .unwrap_or_default();
        let rows = FrameRows::load(svc, &metrics)?;
        let series = Series::load(svc, &first)?;
        let files = if mode == "levels" || mode == "timeline" { 1 } else { 2 };
        Ok((Loaded { path: path.to_string(), ref_path, dist_path, summary, mode, metrics, timeline, rows, files }, series))
    }

    /// "a.mov" or "a.mov vs b.mov"; the analysis file's name if the media
    /// isn't known.
    pub fn title(&self) -> String {
        let name = |p: &str| {
            std::path::Path::new(p).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string())
        };
        match (self.ref_path.is_empty(), self.dist_path.is_empty()) {
            (true, _) => name(&self.path),
            (false, true) => name(&self.ref_path),
            (false, false) => format!("{} vs {}", name(&self.ref_path), name(&self.dist_path)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_median_matches_the_web_ui() {
        // JS: sort the clipped window, take slice[floor(len / 2)].
        let v = [1.0, 9.0, 2.0, 8.0, 3.0, 7.0, 4.0];
        assert_eq!(running_median(&v, 1), v.to_vec());
        assert_eq!(running_median(&v, 3), vec![9.0, 2.0, 8.0, 3.0, 7.0, 4.0, 7.0]);
        assert_eq!(running_median(&v, 5)[0], 2.0);
        assert_eq!(running_median(&[], 5), Vec::<f64>::new());
        // A single-frame spike disappears.
        let mut s = vec![10.0; 50];
        s[25] = 900.0;
        assert!(running_median(&s, 5).iter().all(|&x| x == 10.0));
    }
}
