//! Aggregate results per perturbation type.

use std::collections::BTreeMap;
use std::fmt::Write;

use subsyncer_core::gate::Outcome;

use crate::CaseResult;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Summary {
    pub method: String,
    pub kind: String,
    pub cases: usize,
    /// Cue-weighted fraction within tolerance (effective output).
    pub within: f64,
    /// Fraction of cases with >= 95 % of cues within tolerance.
    pub pass_rate: f64,
    /// Median over cases of the per-case median abs error.
    pub median_err_ms: i64,
    pub raw_within: f64,
    pub synced: usize,
    pub already_synced: usize,
    pub low_confidence: usize,
    pub failed: usize,
    pub mean_splits: f64,
    pub mean_elapsed_ms: f64,
    /// Cases where the subtitle was rewritten.
    pub written: usize,
}

pub fn summarize(method: &str, results: &[CaseResult]) -> Vec<Summary> {
    let mut by: BTreeMap<String, Vec<&CaseResult>> = BTreeMap::new();
    for r in results {
        by.entry(format!("{:02}{}", r.kind as u8, r.kind.name())).or_default().push(r);
    }
    let mut all: Vec<&CaseResult> = results.iter().collect();
    let mut out: Vec<Summary> = by.into_values().map(|v| summary(method, v[0].kind.name(), &v)).collect();
    if out.len() > 1 {
        all.sort_by_key(|r| r.kind);
        out.push(summary(method, "ALL", &all));
    }
    out
}

fn summary(method: &str, kind: &str, v: &[&CaseResult]) -> Summary {
    let n_truth: usize = v.iter().map(|r| r.n_truth).sum();
    let within: usize = v.iter().map(|r| r.within).sum();
    let raw: usize = v.iter().map(|r| r.raw_within).sum();
    let mut meds: Vec<i64> = v.iter().map(|r| r.median_err_ms).collect();
    meds.sort_unstable();
    let count = |o: Outcome| v.iter().filter(|r| r.outcome == o).count();
    Summary {
        method: method.to_string(),
        kind: kind.to_string(),
        cases: v.len(),
        within: within as f64 / n_truth.max(1) as f64,
        pass_rate: v.iter().filter(|r| r.within_frac() >= 0.95).count() as f64 / v.len().max(1) as f64,
        median_err_ms: meds.get(meds.len() / 2).copied().unwrap_or(0),
        raw_within: raw as f64 / n_truth.max(1) as f64,
        synced: count(Outcome::Synced),
        already_synced: count(Outcome::AlreadySynced),
        low_confidence: count(Outcome::LowConfidence),
        failed: count(Outcome::Failed),
        mean_splits: v.iter().map(|r| r.splits as f64).sum::<f64>() / v.len().max(1) as f64,
        mean_elapsed_ms: v.iter().map(|r| r.elapsed_ms as f64).sum::<f64>() / v.len().max(1) as f64,
        written: v.iter().filter(|r| r.written).count(),
    }
}

pub fn table(rows: &[Summary]) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{:<10} {:<17} {:>5} {:>8} {:>8} {:>8} {:>9} {:>6} {:>6} {:>6} {:>6} {:>7} {:>7} {:>8}",
        "method",
        "perturbation",
        "cases",
        "<=200ms",
        "pass95",
        "raw",
        "med err",
        "sync",
        "already",
        "lowc",
        "fail",
        "written",
        "splits",
        "ms/case"
    );
    for r in rows {
        let _ = writeln!(
            s,
            "{:<10} {:<17} {:>5} {:>7.1}% {:>7.1}% {:>7.1}% {:>7}ms {:>6} {:>6} {:>6} {:>6} {:>7} {:>7.2} {:>8.0}",
            r.method,
            r.kind,
            r.cases,
            r.within * 100.0,
            r.pass_rate * 100.0,
            r.raw_within * 100.0,
            r.median_err_ms,
            r.synced,
            r.already_synced,
            r.low_confidence,
            r.failed,
            r.written,
            r.mean_splits,
            r.mean_elapsed_ms
        );
    }
    s
}
