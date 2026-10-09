//! The report model and its two renderings: Markdown for people, JSON for
//! tools. Every number in either comes from a [`Point`] the harness measured
//! or from a ratio of two such numbers (marked derived); nothing is typed in.

use std::fmt::Write as _;

use crate::env::Env;
use crate::json::Json;
use crate::stats::{Summary, fmt};

/// Which implementation a measurement exercised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    /// store-io through its public API.
    StoreIo,
    /// The raw platform primitive.
    Raw,
    /// fsys 1.1.3.
    Fsys,
}

impl System {
    /// Short name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::StoreIo => "store-io",
            Self::Raw => "raw",
            Self::Fsys => "fsys",
        }
    }
}

/// The data-integrity check of one measurement.
#[derive(Debug, Clone)]
pub enum Verify {
    /// Read back and compared; the text says what was checked.
    Passed(String),
    /// A mismatch or read failure; the text says what.
    Failed(String),
    /// Not checked, and why.
    NotChecked(String),
}

impl Verify {
    fn text(&self) -> String {
        match self {
            Self::Passed(s) => format!("passed: {s}"),
            Self::Failed(s) => format!("FAILED: {s}"),
            Self::NotChecked(s) => format!("not checked: {s}"),
        }
    }
}

/// One measured configuration.
#[derive(Debug, Clone)]
pub struct Point {
    /// Implementation exercised.
    pub system: System,
    /// What was called, in API terms.
    pub variant: String,
    /// Workload parameters.
    pub params: Vec<(&'static str, Json)>,
    /// Metrics, each named with its unit (`ops_per_s`, `p99_us`, ...).
    pub metrics: Vec<(&'static str, Option<f64>)>,
    /// Integrity check.
    pub verify: Verify,
    /// The error that ended the measurement, if any.
    pub error: Option<String>,
    /// Notes (capacity stops, configuration).
    pub notes: Vec<String>,
}

impl Point {
    /// A point with no metrics yet.
    pub fn new(system: System, variant: impl Into<String>) -> Self {
        Self {
            system,
            variant: variant.into(),
            params: Vec::new(),
            metrics: Vec::new(),
            verify: Verify::NotChecked("measurement did not complete".to_owned()),
            error: None,
            notes: Vec::new(),
        }
    }

    /// Adds a parameter.
    #[must_use]
    pub fn param(mut self, k: &'static str, v: Json) -> Self {
        self.params.push((k, v));
        self
    }

    /// Records a metric.
    pub fn metric(&mut self, k: &'static str, v: Option<f64>) {
        self.metrics.push((k, v));
    }

    /// Records the latency summary (microseconds) under the usual names.
    pub fn latency(&mut self, s: Option<&Summary>) {
        self.metric("samples", s.map(|s| s.n as f64));
        self.metric("p50_us", s.map(|s| s.p50));
        self.metric("p90_us", s.map(|s| s.p90));
        self.metric("p99_us", s.and_then(|s| s.p99));
        self.metric("p999_us", s.and_then(|s| s.p999));
        self.metric("max_us", s.map(|s| s.max));
        self.metric("mean_us", s.map(|s| s.mean));
    }

    /// A metric's value, or else a numeric parameter's.
    #[must_use]
    pub fn get(&self, k: &str) -> Option<f64> {
        self.metrics
            .iter()
            .find(|(n, _)| *n == k)
            .and_then(|(_, v)| *v)
            .or_else(|| {
                self.params
                    .iter()
                    .find(|(n, _)| *n == k)
                    .and_then(|(_, v)| match v {
                        Json::Num(x) => Some(*x),
                        _ => None,
                    })
            })
    }

    /// The metric formatted with `decimals` places (`n/a`, or `error` when
    /// the measurement failed).
    #[must_use]
    pub fn cell(&self, k: &str, decimals: usize) -> String {
        match (self.get(k), &self.error) {
            (None, Some(_)) => "error".to_owned(),
            (v, _) => fmt(v, decimals),
        }
    }

    fn json(&self) -> Json {
        Json::obj([
            ("system", Json::str(self.system.name())),
            ("variant", Json::str(&self.variant)),
            (
                "params",
                Json::obj(self.params.iter().map(|(k, v)| (*k, v.clone()))),
            ),
            (
                "metrics",
                Json::obj(self.metrics.iter().map(|(k, v)| (*k, Json::opt(*v)))),
            ),
            ("verify", Json::str(self.verify.text())),
            ("error", self.error.as_ref().map_or(Json::Null, Json::str)),
            (
                "notes",
                Json::Arr(self.notes.iter().map(Json::str).collect()),
            ),
        ])
    }
}

/// Which direction of a metric is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Better {
    /// Throughput-like.
    Higher,
    /// Latency-like.
    Lower,
}

/// One store-io vs raw vs fsys comparison for the summary table.
#[derive(Debug, Clone)]
pub struct Comparison {
    /// The case, in workload terms.
    pub case: String,
    /// Metric name with unit.
    pub metric: String,
    /// Better direction.
    pub better: Better,
    /// store-io value.
    pub store_io: Option<f64>,
    /// Raw primitive value.
    pub raw: Option<f64>,
    /// fsys value (`None`: no comparable fsys operation, or not measured).
    pub fsys: Option<f64>,
    /// Which variants were compared.
    pub note: String,
}

impl Comparison {
    /// store-io's advantage over `other` in percent, positive when store-io
    /// is better: `(s/o - 1) * 100` for higher-is-better metrics and
    /// `(1 - s/o) * 100` for lower-is-better ones.
    #[must_use]
    pub fn advantage(&self, other: Option<f64>) -> Option<f64> {
        let (s, o) = (self.store_io?, other?);
        if o == 0.0 || !s.is_finite() || !o.is_finite() {
            return None;
        }
        Some(match self.better {
            Better::Higher => (s / o - 1.0) * 100.0,
            Better::Lower => (1.0 - s / o) * 100.0,
        })
    }

    fn json(&self) -> Json {
        Json::obj([
            ("case", Json::str(&self.case)),
            ("metric", Json::str(&self.metric)),
            (
                "better",
                Json::str(match self.better {
                    Better::Higher => "higher",
                    Better::Lower => "lower",
                }),
            ),
            ("store_io", Json::opt(self.store_io)),
            ("raw", Json::opt(self.raw)),
            ("fsys", Json::opt(self.fsys)),
            (
                "store_io_vs_raw_ratio",
                Json::opt(ratio(self.store_io, self.raw)),
            ),
            (
                "store_io_vs_fsys_ratio",
                Json::opt(ratio(self.store_io, self.fsys)),
            ),
            (
                "store_io_advantage_vs_raw_pct",
                Json::opt(self.advantage(self.raw)),
            ),
            (
                "store_io_advantage_vs_fsys_pct",
                Json::opt(self.advantage(self.fsys)),
            ),
            ("note", Json::str(&self.note)),
        ])
    }
}

fn ratio(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    let (a, b) = (a?, b?);
    (b != 0.0).then_some(a / b)
}

/// A Markdown table.
#[derive(Debug, Clone, Default)]
pub struct Table {
    /// Caption printed above the table.
    pub caption: String,
    /// Column headers.
    pub headers: Vec<String>,
    /// Rows of cells.
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// A table with these headers.
    pub fn new(caption: impl Into<String>, headers: &[&str]) -> Self {
        Self {
            caption: caption.into(),
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            rows: Vec::new(),
        }
    }

    /// Adds a row.
    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    fn markdown(&self, out: &mut String) {
        if !self.caption.is_empty() {
            let _ = writeln!(out, "{}\n", self.caption);
        }
        let _ = writeln!(out, "| {} |", self.headers.join(" | "));
        let _ = writeln!(
            out,
            "|{}",
            self.headers.iter().map(|_| "---|").collect::<String>()
        );
        for r in &self.rows {
            let cells: Vec<String> = r.iter().map(|c| c.replace('|', "\\|")).collect();
            let _ = writeln!(out, "| {} |", cells.join(" | "));
        }
        out.push('\n');
    }
}

/// One workload's results.
#[derive(Debug, Clone)]
pub struct Section {
    /// Command name (`lone`, `concurrent`, ...).
    pub key: &'static str,
    /// Heading.
    pub title: String,
    /// How it was measured.
    pub method: Vec<String>,
    /// Result tables.
    pub tables: Vec<Table>,
    /// Every measurement.
    pub points: Vec<Point>,
    /// Summary rows.
    pub comparisons: Vec<Comparison>,
    /// Observations derived from the points.
    pub findings: Vec<String>,
    /// Errors, integrity failures and other anomalies.
    pub anomalies: Vec<String>,
    /// Wall time the workload took, seconds.
    pub elapsed_s: f64,
}

impl Section {
    /// An empty section.
    pub fn new(key: &'static str, title: impl Into<String>) -> Self {
        Self {
            key,
            title: title.into(),
            method: Vec::new(),
            tables: Vec::new(),
            points: Vec::new(),
            comparisons: Vec::new(),
            findings: Vec::new(),
            anomalies: Vec::new(),
            elapsed_s: 0.0,
        }
    }

    /// Records every failed check and error of the points as anomalies.
    pub fn collect_point_anomalies(&mut self) {
        for p in &self.points {
            let what = format!("{} / {}", p.system.name(), p.variant);
            if let Some(e) = &p.error {
                self.anomalies.push(format!("{what}: error: {e}"));
            }
            if let Verify::Failed(f) = &p.verify {
                self.anomalies
                    .push(format!("{what}: data integrity check FAILED: {f}"));
            }
        }
    }
}

/// Everything above the workload sections.
#[derive(Debug, Clone)]
pub struct Header {
    /// Machine and directory facts.
    pub env: Env,
    /// Short device name used in the file name.
    pub device: String,
    /// The backend under test.
    pub backend: String,
    /// `Store::report()` text.
    pub store_report: String,
    /// Raw evidence dump.
    pub evidence: String,
    /// `StoreOptions` dump.
    pub options: String,
    /// Class, label and durable-open outcome.
    pub class: String,
    /// The refusal that forced the override, if any.
    pub refusal: Option<String>,
    /// fsys configuration facts.
    pub fsys: String,
    /// Measured seconds per point.
    pub secs: f64,
    /// Warm-up seconds per point.
    pub warmup: f64,
    /// Workloads requested.
    pub command: String,
    /// One-off diagnostics run before the workloads.
    pub diagnostics: Vec<String>,
    /// Raw primitive descriptions.
    pub raw: Vec<String>,
}

/// Renders the Markdown report.
#[must_use]
pub fn markdown(
    h: &Header,
    sections: &[Section],
    total_s: f64,
    incomplete: Option<&str>,
) -> String {
    let mut o = String::new();
    let e = &h.env;
    let _ = writeln!(
        o,
        "# store-io performance harness: {} / {} ({})\n",
        e.os, h.device, e.date
    );
    if let Some(why) = incomplete {
        let _ = writeln!(o, "> **INCOMPLETE RUN:** {why}\n");
    }
    if let Some(r) = &h.refusal {
        let _ = writeln!(
            o,
            "> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:\n\
             > `{r}`\n>\n\
             > Every store in this run was created with the documented labelled override \
             (`Trust {{ override_refusal: true, .. }}`): the class stays as decided, every receipt is \
             labelled `overridden`, and the most conservative primitive (write + device flush) is \
             used. Nothing else was weakened. The numbers below are therefore **not** gate numbers \
             for a certified durability class; they measure software cost on this stack.\n"
        );
    }
    let _ = writeln!(o, "## Run\n");
    let mut t = Table::new("", &["item", "value"]);
    let rows: [(&str, String); 16] = [
        ("date (UTC)", e.started.clone()),
        ("command", h.command.clone()),
        ("store-io commit", e.commit.clone()),
        ("OS", e.os_version.clone()),
        ("kernel / build", e.kernel.clone()),
        ("CPU", e.cpu.clone()),
        ("logical CPUs", e.cpus.to_string()),
        ("filesystem", e.filesystem.clone()),
        ("directory under test", e.dir.clone()),
        ("backend", h.backend.clone()),
        ("durability class", h.class.clone()),
        (
            "time per point",
            format!(
                "{:.1} s measured after {:.1} s warm-up (time-boxed; a point may end early at region capacity, noted where it does)",
                h.secs, h.warmup
            ),
        ),
        ("fsys", h.fsys.clone()),
        ("raw primitives", h.raw.join("; ")),
        (
            "harness build",
            "release, lto=fat, codegen-units=1 (same as store-io's release profile)".to_owned(),
        ),
        ("total wall time", format!("{total_s:.1} s")),
    ];
    for (k, v) in rows {
        t.row(vec![k.to_owned(), v]);
    }
    t.markdown(&mut o);

    let _ = writeln!(
        o,
        "## Device report (`Store::report()`)\n\n```text\n{}\n```\n",
        h.store_report
    );
    let _ = writeln!(
        o,
        "## Store options (every store in this run)\n\n```text\n{}\n```\n",
        h.options
    );
    let _ = writeln!(
        o,
        "<details><summary>Raw evidence from the probe</summary>\n\n```text\n{}\n```\n</details>\n",
        h.evidence
    );
    let _ = writeln!(o, "## How to read the numbers\n");
    let _ = writeln!(
        o,
        "- Every value in the workload tables is **[measured]** by this harness in this run; \
         ratios and percentages are **[derived]** from those measurements.\n\
         - Latencies are per operation (per commit for batches), in microseconds, nearest-rank \
         percentiles over every measured operation; p99 needs at least 100 samples and p99.9 at \
         least 1000, otherwise `n/a`.\n\
         - Throughput is operations completed inside the measured window divided by the window \
         (wall clock).\n\
         - `write calls` / `other calls` are the OS per-process counters ({}), read before and \
         after the measured window; they count system calls, not device commands.\n\
         - **store-io advantage**: positive means store-io is better (more throughput, or less \
         latency) by that percentage; negative means worse.\n",
        crate::raw::COUNTER_SOURCE
    );

    let _ = writeln!(o, "## Summary: store-io vs raw vs fsys\n");
    let mut s = Table::new(
        "",
        &[
            "workload",
            "case",
            "metric",
            "store-io",
            "raw",
            "fsys",
            "store-io ÷ raw",
            "store-io advantage vs raw",
            "store-io ÷ fsys",
            "store-io advantage vs fsys",
            "compared",
        ],
    );
    for sec in sections {
        for c in &sec.comparisons {
            let pct = |x: Option<f64>| x.map_or("n/a".to_owned(), |v| format!("{v:+.1}%"));
            s.row(vec![
                sec.key.to_owned(),
                c.case.clone(),
                format!(
                    "{} ({} is better)",
                    c.metric,
                    if c.better == Better::Higher {
                        "higher"
                    } else {
                        "lower"
                    }
                ),
                fmt(c.store_io, 1),
                fmt(c.raw, 1),
                fmt(c.fsys, 1),
                fmt(ratio(c.store_io, c.raw), 3),
                pct(c.advantage(c.raw)),
                fmt(ratio(c.store_io, c.fsys), 3),
                pct(c.advantage(c.fsys)),
                c.note.clone(),
            ]);
        }
    }
    s.markdown(&mut o);

    let _ = writeln!(o, "## Anomalies, errors and integrity failures\n");
    let mut any = false;
    for d in &h.diagnostics {
        let _ = writeln!(o, "- diagnostic: {d}");
        any = true;
    }
    for sec in sections {
        for a in &sec.anomalies {
            let _ = writeln!(o, "- **{}**: {a}", sec.key);
            any = true;
        }
    }
    if !any {
        let _ = writeln!(
            o,
            "- None: every measurement completed and every integrity check passed."
        );
    }
    o.push('\n');

    for sec in sections {
        let _ = writeln!(o, "## {} (`{}`)\n", sec.title, sec.key);
        let _ = writeln!(o, "Wall time: {:.1} s.\n", sec.elapsed_s);
        if !sec.method.is_empty() {
            let _ = writeln!(o, "**Method.**\n");
            for m in &sec.method {
                let _ = writeln!(o, "- {m}");
            }
            o.push('\n');
        }
        for t in &sec.tables {
            t.markdown(&mut o);
        }
        let mut v = Table::new(
            "Integrity checks and notes per measurement:",
            &["system", "variant", "integrity", "notes"],
        );
        for p in &sec.points {
            let mut notes = p.notes.clone();
            if let Some(e) = &p.error {
                notes.insert(0, format!("ERROR: {e}"));
            }
            v.row(vec![
                p.system.name().to_owned(),
                p.variant.clone(),
                p.verify.text(),
                notes.join("; "),
            ]);
        }
        v.markdown(&mut o);
        if !sec.findings.is_empty() {
            let _ = writeln!(o, "**Findings.**\n");
            for f in &sec.findings {
                let _ = writeln!(o, "- {f}");
            }
            o.push('\n');
        }
    }
    o
}

/// Renders the JSON report.
#[must_use]
pub fn json(h: &Header, sections: &[Section], total_s: f64, incomplete: Option<&str>) -> String {
    let e = &h.env;
    let doc = Json::obj([
        ("schema", Json::str("store-io-harness/1")),
        ("incomplete", incomplete.map_or(Json::Null, Json::str)),
        (
            "run",
            Json::obj([
                ("date", Json::str(&e.started)),
                ("command", Json::str(&h.command)),
                ("commit", Json::str(&e.commit)),
                ("os", Json::str(e.os)),
                ("os_version", Json::str(&e.os_version)),
                ("kernel", Json::str(&e.kernel)),
                ("cpu", Json::str(&e.cpu)),
                ("cpus", Json::Num(e.cpus as f64)),
                ("filesystem", Json::str(&e.filesystem)),
                ("dir", Json::str(&e.dir)),
                ("device", Json::str(&h.device)),
                ("backend", Json::str(&h.backend)),
                ("secs_per_point", Json::Num(h.secs)),
                ("warmup_secs", Json::Num(h.warmup)),
                ("total_secs", Json::Num(total_s)),
                ("fsys", Json::str(&h.fsys)),
                (
                    "raw_primitives",
                    Json::Arr(h.raw.iter().map(Json::str).collect()),
                ),
            ]),
        ),
        (
            "store",
            Json::obj([
                ("report", Json::str(&h.store_report)),
                ("class", Json::str(&h.class)),
                ("override_in_effect", Json::Bool(h.refusal.is_some())),
                (
                    "override_refusal",
                    h.refusal.as_ref().map_or(Json::Null, Json::str),
                ),
                ("options", Json::str(&h.options)),
                ("evidence", Json::str(&h.evidence)),
            ]),
        ),
        (
            "diagnostics",
            Json::Arr(h.diagnostics.iter().map(Json::str).collect()),
        ),
        (
            "workloads",
            Json::Arr(
                sections
                    .iter()
                    .map(|s| {
                        Json::obj([
                            ("key", Json::str(s.key)),
                            ("title", Json::str(&s.title)),
                            ("elapsed_secs", Json::Num(s.elapsed_s)),
                            (
                                "method",
                                Json::Arr(s.method.iter().map(Json::str).collect()),
                            ),
                            (
                                "points",
                                Json::Arr(s.points.iter().map(Point::json).collect()),
                            ),
                            (
                                "comparisons",
                                Json::Arr(s.comparisons.iter().map(Comparison::json).collect()),
                            ),
                            (
                                "findings",
                                Json::Arr(s.findings.iter().map(Json::str).collect()),
                            ),
                            (
                                "anomalies",
                                Json::Arr(s.anomalies.iter().map(Json::str).collect()),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ]);
    doc.pretty()
}
