//! Maps a test command's JSON stdout to bit's test reporting with a jq filter.

use std::collections::HashMap;
use std::time::Duration;

use indexmap::IndexMap;
use jaq_core::data::JustLut;
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, unwrap_valr};
use jaq_json::Val;
use serde::Deserialize;

use crate::output::BlockWriter;
use crate::provider::BoxError;

/// Suite for results that don't name one.
const DEFAULT_SUITE: &str = "tests";

/// A compiled `results` filter.
pub struct ResultsFilter(jaq_core::Filter<JustLut<Val>>);

impl ResultsFilter {
    pub fn compile(code: &str) -> Result<Self, BoxError> {
        let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
        let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());
        let arena = Arena::default();
        let modules = Loader::new(defs)
            .load(&arena, File { code, path: () })
            .map_err(load_errors)?;
        let filter = Compiler::default()
            .with_funs(funs)
            .compile(modules)
            .map_err(compile_errors)?;
        Ok(Self(filter))
    }

    fn run(&self, input: Val) -> Result<Vec<Record>, BoxError> {
        let ctx = Ctx::<JustLut<Val>>::new(&self.0.lut, Vars::new([]));
        self.0
            .id
            .run((ctx, input))
            .map(unwrap_valr)
            .map(|out| {
                let val = out.map_err(|e| format!("results filter failed: {e}"))?;
                // jaq values display as JSON, which lets serde validate the record shape.
                serde_json::from_str(&val.to_string())
                    .map_err(|e| format!("results filter produced an invalid test result {val}: {e}").into())
            })
            .collect()
    }
}

fn load_errors(errs: jaq_core::load::Errors<&str, ()>) -> BoxError {
    use jaq_core::load::Error;
    let mut msgs = Vec::new();
    for (_, err) in errs {
        match err {
            Error::Io(errs) => msgs.extend(errs.into_iter().map(|(path, e)| format!("cannot load `{path}`: {e}"))),
            Error::Lex(errs) => msgs.extend(
                errs.into_iter()
                    .map(|(expect, found)| format!("expected {}, found {}", expect.as_str(), snippet(found))),
            ),
            Error::Parse(errs) => msgs.extend(
                errs.into_iter()
                    .map(|(expect, found)| format!("expected {}, found {}", expect.as_str(), snippet(found))),
            ),
        }
    }
    format!("invalid results filter: {}", msgs.join("; ")).into()
}

fn compile_errors(errs: jaq_core::compile::Errors<&str, ()>) -> BoxError {
    let msgs: Vec<_> = errs
        .into_iter()
        .flat_map(|(_, errs)| errs)
        .map(|(name, undefined)| format!("undefined {} `{name}`", undefined.as_str()))
        .collect();
    format!("invalid results filter: {}", msgs.join("; ")).into()
}

/// Lexer errors carry the rest of the filter from the error position, so
/// show only its start.
fn snippet(found: &str) -> String {
    let first_line = found.lines().next().unwrap_or_default();
    if first_line.is_empty() {
        return "end of filter".to_owned();
    }
    let short: String = first_line.chars().take(20).collect();
    format!("`{short}`")
}

/// One value produced by the `results` filter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    name: String,
    #[serde(default)]
    suite: Option<String>,
    #[serde(default)]
    status: Option<Status>,
    /// Seconds.
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    #[serde(alias = "passed")]
    Pass,
    #[serde(alias = "failed")]
    Fail,
    #[serde(alias = "skipped")]
    Skip,
}

#[derive(Default)]
struct SuiteStats {
    passed: usize,
    failed: usize,
    skipped: usize,
    duration: Duration,
    failures: Vec<(String, Duration, String)>,
}

/// Consumes stdout line by line, feeds each JSON value through the filter,
/// and reports the resulting tests.
///
/// A value is either one JSONL line or a multi-line document. Only lines
/// starting at column 0 with `{` or `[` begin a value, and a multi-line
/// document must close with `}` or `]` at column 0, as pretty-printers emit.
/// This keeps log lines that merely look like JSON (e.g. `true`) away from the
/// filter. Anything that doesn't form a value is shown as ordinary output.
pub struct Report<'a> {
    filter: &'a ResultsFilter,
    writer: &'a BlockWriter,
    verbose: bool,
    /// Lines of a multi-line document still being read.
    document: Vec<String>,
    /// Output for tests whose status hasn't arrived yet, keyed by (suite, name).
    pending: HashMap<(String, String), String>,
    suites: IndexMap<String, SuiteStats>,
}

impl<'a> Report<'a> {
    pub fn new(filter: &'a ResultsFilter, writer: &'a BlockWriter, verbose: bool) -> Self {
        Self {
            filter,
            writer,
            verbose,
            document: Vec::new(),
            pending: HashMap::new(),
            suites: IndexMap::new(),
        }
    }

    pub fn line(&mut self, line: &str) -> Result<(), BoxError> {
        if line.starts_with(['{', '[']) {
            self.flush_document();
            if let Ok(val) = jaq_json::read::parse_single(line.as_bytes()) {
                return self.input(val);
            }
            self.document.push(line.to_owned());
            return Ok(());
        }
        if self.document.is_empty() {
            self.writer.line(line);
            return Ok(());
        }
        self.document.push(line.to_owned());
        if line.starts_with(['}', ']']) {
            let text = self.document.join("\n");
            if let Ok(val) = jaq_json::read::parse_single(text.as_bytes()) {
                self.document.clear();
                return self.input(val);
            }
            self.flush_document();
        }
        Ok(())
    }

    /// Emit suite summaries and return whether every reported test passed.
    pub fn finish(mut self) -> bool {
        self.flush_document();
        let mut all_passed = true;
        for (suite, stats) in &self.suites {
            all_passed &= stats.failed == 0;
            if self.verbose {
                continue;
            }
            if stats.failed > 0 {
                self.writer
                    .test_suite_failed(suite, stats.duration, stats.passed, stats.failed, &stats.failures);
            } else if stats.passed == 0 {
                self.writer.test_suite_skipped(suite);
            } else {
                self.writer
                    .test_suite_passed(suite, stats.duration, stats.passed, stats.skipped);
            }
        }
        all_passed
    }

    /// Show an unfinished document as ordinary output.
    fn flush_document(&mut self) {
        for line in self.document.drain(..) {
            self.writer.line(&line);
        }
    }

    fn input(&mut self, val: Val) -> Result<(), BoxError> {
        for record in self.filter.run(val)? {
            self.record(record)?;
        }
        Ok(())
    }

    fn record(&mut self, record: Record) -> Result<(), BoxError> {
        let key = (record.suite.unwrap_or_else(|| DEFAULT_SUITE.to_owned()), record.name);
        let mut output = self.pending.remove(&key).unwrap_or_default();
        if let Some(chunk) = record.output {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&chunk);
        }
        let Some(status) = record.status else {
            self.pending.insert(key, output);
            return Ok(());
        };

        let (suite, name) = key;
        let duration = Duration::try_from_secs_f64(record.duration.unwrap_or(0.0))
            .map_err(|e| format!("invalid duration for test `{suite}/{name}`: {e}"))?;
        let output = output.trim_end();
        let stats = self.suites.entry(suite.clone()).or_default();
        stats.duration += duration;
        match status {
            Status::Pass => {
                stats.passed += 1;
                if self.verbose {
                    self.writer.test_passed(&suite, &name, duration);
                }
            }
            Status::Fail => {
                stats.failed += 1;
                if self.verbose {
                    self.writer.test_failed(&suite, &name, duration, output);
                } else {
                    stats.failures.push((name, duration, output.to_owned()));
                }
            }
            Status::Skip => {
                stats.skipped += 1;
                if self.verbose {
                    self.writer.test_skipped(&suite, &name);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Output;

    fn report_lines(filter: &str, lines: &[&str]) -> Result<bool, BoxError> {
        let filter = ResultsFilter::compile(filter)?;
        let out = Output::silent();
        let writer = out.writer("test");
        let mut report = Report::new(&filter, &writer, false);
        for line in lines {
            report.line(line)?;
        }
        Ok(report.finish())
    }

    fn records(filter: &str, input: &str) -> Vec<Record> {
        let filter = ResultsFilter::compile(filter).unwrap();
        filter
            .run(jaq_json::read::parse_single(input.as_bytes()).unwrap())
            .unwrap()
    }

    #[test]
    fn compile_reports_syntax_errors() {
        let err = ResultsFilter::compile(".tests[] | {").err().unwrap().to_string();
        assert!(err.starts_with("invalid results filter: expected"), "{err}");
    }

    #[test]
    fn compile_reports_undefined_filters() {
        let err = ResultsFilter::compile("nosuchfilter").err().unwrap().to_string();
        assert_eq!(err, "invalid results filter: undefined filter `nosuchfilter`");
    }

    #[test]
    fn run_maps_document_to_records() {
        let out = records(
            r#".tests[] | {suite: "s", name: .n, status: (if .ok then "passed" else "failed" end), duration: (.ms / 1000)}"#,
            r#"{"tests": [{"n": "a", "ok": true, "ms": 1500}, {"n": "b", "ok": false, "ms": 0}]}"#,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "a");
        assert_eq!(out[0].suite.as_deref(), Some("s"));
        assert!(matches!(out[0].status, Some(Status::Pass)));
        assert_eq!(out[0].duration, Some(1.5));
        assert!(matches!(out[1].status, Some(Status::Fail)));
    }

    #[test]
    fn run_rejects_unknown_fields() {
        let filter = ResultsFilter::compile(r#"{name: "a", state: "pass"}"#).unwrap();
        let err = filter.run(Val::Null).err().unwrap().to_string();
        assert!(err.contains("unknown field `state`"), "{err}");
    }

    #[test]
    fn run_rejects_unknown_status() {
        let filter = ResultsFilter::compile(r#"{name: "a", status: "pending"}"#).unwrap();
        assert!(filter.run(Val::Null).is_err());
    }

    #[test]
    fn run_reports_filter_errors() {
        let filter = ResultsFilter::compile(".[0]").unwrap();
        let err = filter.run(Val::from("x".to_owned())).err().unwrap().to_string();
        assert!(err.starts_with("results filter failed:"), "{err}");
    }

    #[test]
    fn single_document_all_passing() {
        let passed = report_lines(
            r#".[] | {name: ., status: "pass"}"#,
            &["> pkg@1.0.0 test", "[", r#"  "a","#, r#"  "b""#, "]"],
        )
        .unwrap();
        assert!(passed);
    }

    #[test]
    fn jsonl_event_stream_with_failure() {
        // Shaped like `go test -json`.
        let filter = r#"select(.Test) | {suite: .Package, name: .Test} +
            if .Action == "output" then {output: .Output}
            elif .Action == "pass" or .Action == "fail" or .Action == "skip" then {status: .Action, duration: .Elapsed}
            else empty end"#;
        let lines = [
            r#"{"Action":"run","Package":"p","Test":"TestA"}"#,
            r#"{"Action":"output","Package":"p","Test":"TestA","Output":"boom\n"}"#,
            r#"{"Action":"fail","Package":"p","Test":"TestA","Elapsed":0.01}"#,
            r#"{"Action":"pass","Package":"p","Test":"TestB","Elapsed":0.02}"#,
            r#"{"Action":"fail","Package":"p","Elapsed":0.03}"#,
        ];
        assert!(!report_lines(filter, &lines).unwrap());
    }

    #[test]
    fn output_accumulates_until_status() {
        let filter = ResultsFilter::compile(".").unwrap();
        let out = Output::silent();
        let writer = out.writer("test");
        let mut report = Report::new(&filter, &writer, false);
        report.line(r#"{"name":"a","output":"one"}"#).unwrap();
        report.line(r#"{"name":"a","output":"two\n"}"#).unwrap();
        report.line(r#"{"name":"a","status":"fail"}"#).unwrap();
        let stats = &report.suites[DEFAULT_SUITE];
        assert_eq!(
            stats.failures,
            [("a".to_owned(), Duration::ZERO, "one\ntwo".to_owned())]
        );
        assert!(report.pending.is_empty());
    }

    #[test]
    fn non_json_lines_are_not_filtered() {
        // `true` and indented JSON aren't treated as values, and an unclosed
        // document is shown as output, so the filter never sees them.
        let passed = report_lines(
            r#"{name: .n, status: "pass"}"#,
            &["true", r#"  {"n": "indented"}"#, "[INFO] starting", r#"{"n": "a"}"#],
        )
        .unwrap();
        assert!(passed);
    }

    #[test]
    fn negative_duration_is_an_error() {
        let err = report_lines(".", &[r#"{"name":"a","status":"pass","duration":-1}"#])
            .err()
            .unwrap()
            .to_string();
        assert!(err.starts_with("invalid duration for test `tests/a`"), "{err}");
    }
}
