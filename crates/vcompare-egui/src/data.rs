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
        Ok(Series { metric: metric.to_string(), frames, values, pyramid, min, max })
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
    pub path: String,
    pub summary: Value,
    pub mode: String,
    pub metrics: Vec<String>,
    pub timeline: Option<TimelineInfo>,
    pub rows: FrameRows,
    /// Files in the analysis (thumbnail rows per strip).
    pub files: usize,
}

impl Loaded {
    pub fn open(svc: &Service, path: &str) -> Result<(Loaded, Series), String> {
        let summary = svc.load_saved_analysis(path)?;
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
        let rows = FrameRows::load(svc, &metrics)?;
        let series = Series::load(svc, &first)?;
        let files = if mode == "levels" || mode == "timeline" { 1 } else { 2 };
        Ok((Loaded { path: path.to_string(), summary, mode, metrics, timeline, rows, files }, series))
    }

    pub fn file_name(&self) -> String {
        std::path::Path::new(&self.path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.clone())
    }
}
