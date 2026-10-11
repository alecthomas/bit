//! Maps a test command's JSON stdout to bit's test reporting with a jq filter.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use jaq_core::data::JustLut;
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, unwrap_valr};
use jaq_json::Val;
use serde::Deserialize;

use crate::output::BlockWriter;
use crate::provider::BoxError;
use crate::providers::ReportMode;

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

/// One value produced by the `results` filter. A record with a `name` is
/// about a test; one without is about its suite.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    suite: Option<String>,
    #[serde(default)]
    event: Option<Event>,
    #[serde(default)]
    status: Option<Status>,
    /// Seconds.
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Event {
    Start,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    #[serde(alias = "passed")]
    Pass,
    #[serde(alias = "failed")]
    Fail,
    #[serde(alias = "skipped")]
    Skip,
}

/// What a record does to its test or suite.
enum Step {
    Start,
    Output,
    End(Option<Status>),
}

impl Record {
    fn step(&self) -> Result<Step, &'static str> {
        match (self.event, self.status) {
            (Some(Event::Start), None) => Ok(Step::Start),
            (Some(Event::Start), Some(_)) => Err("a start record can't have a status"),
            (Some(Event::End), status) => Ok(Step::End(status)),
            // A status alone implies the end.
            (None, Some(status)) => Ok(Step::End(Some(status))),
            (None, None) if self.output.is_some() => Ok(Step::Output),
            (None, None) => Err("a record needs an event, status, or output"),
        }
    }
}

#[derive(Default)]
struct Suite {
    started: Option<Instant>,
    passed: usize,
    failed: usize,
    skipped: usize,
    /// Sum of known test durations, for suites that report no time of their
    /// own. `None` when no test had one.
    test_time: Option<Duration>,
    failures: Vec<(String, Option<Duration>, String)>,
}

#[derive(Default)]
struct PendingTest {
    started: Option<Instant>,
    output: String,
}

/// Result of a mapped test run.
pub struct Outcome {
    /// Whether no test or suite failed.
    pub passed: bool,
    /// Logs held back from streaming, to show if the run fails.
    pub logs: Vec<String>,
}

/// Consumes stdout line by line, feeds each JSON value through the filter,
/// and reports the resulting tests.
///
/// A value is either one JSONL line or a multi-line document. Only lines
/// starting at column 0 with `{` or `[` begin a value, and a multi-line
/// document must close with `}` or `]` at column 0, as pretty-printers emit.
/// This keeps log lines that merely look like JSON (e.g. `true`) away from the
/// filter. Anything that doesn't form a value is a log line.
///
/// Suites are summarized as soon as they end, so runners that interleave
/// suites must mark each suite's end for summaries to stream; unended suites
/// are summarized when the command exits.
pub struct Report<'a> {
    filter: &'a ResultsFilter,
    writer: &'a BlockWriter,
    mode: ReportMode,
    /// Lines of a multi-line document still being read.
    document: Vec<String>,
    /// Suites still accepting results, in order of first appearance.
    suites: IndexMap<String, Suite>,
    /// Suites that have ended. Later results for them are filter bugs.
    ended: HashSet<String>,
    /// Tests that have started or produced output but not ended, keyed by
    /// (suite, name).
    pending: IndexMap<(String, String), PendingTest>,
    logs: Vec<String>,
    passed: bool,
}

impl<'a> Report<'a> {
    pub fn new(filter: &'a ResultsFilter, writer: &'a BlockWriter, mode: ReportMode) -> Self {
        Self {
            filter,
            writer,
            mode,
            document: Vec::new(),
            suites: IndexMap::new(),
            ended: HashSet::new(),
            pending: IndexMap::new(),
            logs: Vec::new(),
            passed: true,
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
            self.log(line);
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

    /// End every suite still open, once the command has exited.
    pub fn finish(mut self) -> Outcome {
        self.flush_document();
        let open: Vec<String> = self.suites.keys().cloned().collect();
        for suite in open {
            self.end_suite(&suite, None, None);
        }
        Outcome {
            passed: self.passed,
            logs: self.logs,
        }
    }

    /// Stream log text in verbose mode; otherwise hold it for the outcome.
    fn log(&mut self, text: &str) {
        for line in text.lines() {
            if self.mode == ReportMode::Verbose {
                self.writer.line(line);
            } else {
                self.logs.push(line.to_owned());
            }
        }
    }

    /// Treat an unfinished document as log lines.
    fn flush_document(&mut self) {
        for line in std::mem::take(&mut self.document) {
            self.log(&line);
        }
    }

    fn input(&mut self, val: Val) -> Result<(), BoxError> {
        for record in self.filter.run(val)? {
            self.record(record)?;
        }
        Ok(())
    }

    fn record(&mut self, record: Record) -> Result<(), BoxError> {
        let step = record.step().map_err(|e| format!("invalid test result: {e}"))?;
        let suite = record.suite.unwrap_or_else(|| DEFAULT_SUITE.to_owned());
        let subject = match &record.name {
            Some(name) => format!("test `{suite}/{name}`"),
            None => format!("suite `{suite}`"),
        };
        if self.ended.contains(&suite) {
            return Err(format!("result for {subject} arrived after its suite ended").into());
        }
        let duration = record
            .duration
            .map(Duration::try_from_secs_f64)
            .transpose()
            .map_err(|e| format!("invalid duration for {subject}: {e}"))?;

        self.suites.entry(suite.clone()).or_default();
        match record.name {
            Some(name) => self.test_record(suite, name, step, duration, record.output),
            None => {
                self.suite_record(suite, step, duration, record.output);
                Ok(())
            }
        }
    }

    fn test_record(
        &mut self,
        suite: String,
        name: String,
        step: Step,
        duration: Option<Duration>,
        output: Option<String>,
    ) -> Result<(), BoxError> {
        if let (Some(chunk), ReportMode::Verbose) = (&output, self.mode) {
            self.log(chunk);
        }
        let key = (suite, name);
        let pending = self.pending.entry(key.clone()).or_default();
        if let Some(chunk) = output {
            append(&mut pending.output, &chunk);
        }
        match step {
            Step::Start => pending.started = Some(Instant::now()),
            Step::Output => {}
            Step::End(None) => {
                return Err(format!("invalid test result: test `{}/{}` ended without a status", key.0, key.1).into());
            }
            Step::End(Some(status)) => {
                let pending = self.pending.shift_remove(&key).unwrap_or_default();
                let duration = duration.or_else(|| pending.started.map(|t| t.elapsed()));
                let (suite, name) = key;
                self.end_test(&suite, name, status, duration, pending.output);
            }
        }
        Ok(())
    }

    fn suite_record(&mut self, suite: String, step: Step, duration: Option<Duration>, output: Option<String>) {
        // Suite-level output isn't tied to a test, so it's always a log.
        if let Some(chunk) = output {
            self.log(&chunk);
        }
        match step {
            Step::Start => {
                if let Some(stats) = self.suites.get_mut(&suite) {
                    stats.started = Some(Instant::now());
                }
            }
            Step::Output => {}
            Step::End(status) => {
                self.end_suite(&suite, status, duration);
                self.ended.insert(suite);
            }
        }
    }

    fn end_test(&mut self, suite: &str, name: String, status: Status, duration: Option<Duration>, output: String) {
        let output = output.trim_end();
        let per_test = self.mode.per_test();
        let stats = self.suites.entry(suite.to_owned()).or_default();
        if let Some(duration) = duration {
            stats.test_time = Some(stats.test_time.unwrap_or_default() + duration);
        }
        match status {
            Status::Pass => {
                stats.passed += 1;
                if per_test {
                    self.writer.test_passed(suite, &name, duration);
                }
            }
            Status::Fail => {
                stats.failed += 1;
                if per_test {
                    self.writer.test_failed(suite, &name, duration, output);
                } else {
                    stats.failures.push((name, duration, output.to_owned()));
                }
            }
            Status::Skip => {
                stats.skipped += 1;
                if per_test {
                    self.writer.test_skipped(suite, &name);
                }
            }
        }
    }

    /// Close a suite, failing tests it started that never ended, and report it.
    /// `status` and `duration` are the suite's own, if it reported them.
    fn end_suite(&mut self, suite: &str, status: Option<Status>, duration: Option<Duration>) {
        self.end_unfinished_tests(suite);
        let Some(stats) = self.suites.shift_remove(suite) else {
            return;
        };
        let duration = duration
            .or_else(|| stats.started.map(|t| t.elapsed()))
            .or(stats.test_time);
        let failed = stats.failed > 0 || status == Some(Status::Fail);
        self.passed &= !failed;

        if self.mode.per_test() {
            // Per-test lines already covered the tests; only a failure of the
            // suite itself still needs reporting.
            if status == Some(Status::Fail) {
                self.writer
                    .test_suite_failed(suite, duration, stats.passed, stats.failed, &[]);
            }
            return;
        }
        if failed {
            self.writer
                .test_suite_failed(suite, duration, stats.passed, stats.failed, &stats.failures);
        } else if status == Some(Status::Skip) || (status.is_none() && stats.passed == 0) {
            self.writer.test_suite_skipped(suite);
        } else {
            self.writer
                .test_suite_passed(suite, duration, stats.passed, stats.skipped);
        }
    }

    /// Fail tests in `suite` that started but never ended. Output from tests
    /// that never started becomes logs instead: it may trail a test that has
    /// already ended, which mustn't count as a failure.
    fn end_unfinished_tests(&mut self, suite: &str) {
        let keys: Vec<_> = self.pending.keys().filter(|(s, _)| s == suite).cloned().collect();
        for key in keys {
            let pending = self.pending.shift_remove(&key).unwrap_or_default();
            let (_, name) = key;
            match pending.started {
                Some(started) => {
                    let mut output = pending.output;
                    append(&mut output, "no result reported");
                    self.end_test(suite, name, Status::Fail, Some(started.elapsed()), output);
                }
                // Verbose mode streamed this output as it arrived.
                None if self.mode != ReportMode::Verbose => self.log(&pending.output),
                None => {}
            }
        }
    }
}

/// Append a chunk of output on a new line.
fn append(output: &mut String, chunk: &str) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(chunk);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Output;

    fn report_lines(filter: &str, lines: &[&str]) -> Result<Outcome, BoxError> {
        let filter = ResultsFilter::compile(filter)?;
        let out = Output::silent();
        let writer = out.writer("test");
        let mut report = Report::new(&filter, &writer, ReportMode::Suites);
        for line in lines {
            report.line(line)?;
        }
        Ok(report.finish())
    }

    /// Feed JSONL records through an identity filter and return the report
    /// before `finish`, so tests can inspect what ended mid-stream.
    fn with_report<T>(lines: &[&str], check: impl FnOnce(Report) -> T) -> T {
        let filter = ResultsFilter::compile(".").unwrap();
        let out = Output::silent();
        let writer = out.writer("test");
        let mut report = Report::new(&filter, &writer, ReportMode::Suites);
        for line in lines {
            report.line(line).unwrap();
        }
        check(report)
    }

    const GO_FILTER: &str = r#"{suite: .Package} + (if .Test then {name: .Test} else {} end) +
        if .Action == "start" or .Action == "run" then {event: "start"}
        elif .Action == "output" and (.Output | test("^(=== |--- )") | not) then {output: .Output}
        elif .Action == "pass" or .Action == "fail" or .Action == "skip" then {status: .Action, duration: .Elapsed}
        else empty end"#;

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
        assert_eq!(out[0].name.as_deref(), Some("a"));
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
        let outcome = report_lines(
            r#".[] | {name: ., status: "pass"}"#,
            &["> pkg@1.0.0 test", "[", r#"  "a","#, r#"  "b""#, "]"],
        )
        .unwrap();
        assert!(outcome.passed);
        assert_eq!(outcome.logs, ["> pkg@1.0.0 test"]);
    }

    #[test]
    fn go_test_json_suites_end_on_package_events() {
        let filter = ResultsFilter::compile(GO_FILTER).unwrap();
        let out = Output::silent();
        let writer = out.writer("test");
        let mut report = Report::new(&filter, &writer, ReportMode::Suites);
        for line in [
            r#"{"Action":"start","Package":"p"}"#,
            r#"{"Action":"start","Package":"q"}"#,
            r#"{"Action":"run","Package":"p","Test":"TestA"}"#,
            r#"{"Action":"output","Package":"p","Test":"TestA","Output":"=== RUN   TestA\n"}"#,
            r#"{"Action":"output","Package":"p","Test":"TestA","Output":"boom\n"}"#,
            r#"{"Action":"output","Package":"p","Test":"TestA","Output":"--- FAIL: TestA (0.01s)\n"}"#,
            r#"{"Action":"fail","Package":"p","Test":"TestA","Elapsed":0.01}"#,
        ] {
            report.line(line).unwrap();
        }
        // The filter drops go's framing lines from the failure output.
        assert_eq!(
            report.suites["p"].failures,
            [("TestA".to_owned(), Some(Duration::from_millis(10)), "boom".to_owned())]
        );
        for line in [
            r#"{"Action":"output","Package":"p","Output":"FAIL\n"}"#,
            r#"{"Action":"fail","Package":"p","Elapsed":0.03}"#,
        ] {
            report.line(line).unwrap();
        }
        // `p` was summarized when its end arrived, while `q` is still running.
        assert!(report.ended.contains("p"));
        assert!(report.suites.contains_key("q"));
        assert!(!report.passed);
        report
            .line(r#"{"Action":"pass","Package":"q","Test":"TestB","Elapsed":0.02}"#)
            .unwrap();
        let outcome = report.finish();
        assert!(!outcome.passed);
        assert_eq!(outcome.logs, ["FAIL"]);
    }

    #[test]
    fn output_accumulates_until_status() {
        let lines = [
            r#"{"name":"a","output":"one"}"#,
            r#"{"name":"a","output":"two\n"}"#,
            r#"{"name":"a","status":"fail"}"#,
        ];
        with_report(&lines, |report| {
            let stats = &report.suites[DEFAULT_SUITE];
            assert_eq!(stats.failures, [("a".to_owned(), None, "one\ntwo".to_owned())]);
            assert!(report.pending.is_empty());
        });
    }

    #[test]
    fn records_after_suite_end_are_errors() {
        let filter = ResultsFilter::compile(".").unwrap();
        let out = Output::silent();
        let writer = out.writer("test");
        let mut report = Report::new(&filter, &writer, ReportMode::Suites);
        report.line(r#"{"suite":"s","event":"end"}"#).unwrap();
        let err = report
            .line(r#"{"suite":"s","name":"a","status":"pass"}"#)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(err, "result for test `s/a` arrived after its suite ended");
    }

    #[test]
    fn suite_status_fails_run_without_failing_tests() {
        let outcome = report_lines(
            ".",
            &[
                r#"{"suite":"s","name":"a","status":"pass"}"#,
                r#"{"suite":"s","status":"fail","output":"setup failed"}"#,
            ],
        )
        .unwrap();
        assert!(!outcome.passed);
        assert_eq!(outcome.logs, ["setup failed"]);
    }

    #[test]
    fn started_test_without_result_fails() {
        let lines = [
            r#"{"suite":"s","name":"a","event":"start"}"#,
            r#"{"suite":"s","event":"end"}"#,
        ];
        assert!(!report_lines(".", &lines).unwrap().passed);
    }

    #[test]
    fn output_without_start_or_result_is_logged() {
        let outcome = report_lines(".", &[r#"{"name":"a","output":"trailing"}"#]).unwrap();
        assert!(outcome.passed);
        assert_eq!(outcome.logs, ["trailing"]);
    }

    #[test]
    fn invalid_record_combinations_are_errors() {
        for (line, message) in [
            (r#"{"name":"a","event":"end"}"#, "test `tests/a` ended without a status"),
            (
                r#"{"name":"a","event":"start","status":"pass"}"#,
                "a start record can't have a status",
            ),
            (r#"{"name":"a"}"#, "a record needs an event, status, or output"),
        ] {
            let err = report_lines(".", &[line]).err().unwrap().to_string();
            assert_eq!(err, format!("invalid test result: {message}"));
        }
    }

    #[test]
    fn non_json_lines_are_not_filtered() {
        // `true` and indented JSON aren't treated as values, and an unclosed
        // document becomes log lines, so the filter never sees them.
        let outcome = report_lines(
            r#"{name: .n, status: "pass"}"#,
            &["true", r#"  {"n": "indented"}"#, "[INFO] starting", r#"{"n": "a"}"#],
        )
        .unwrap();
        assert!(outcome.passed);
        assert_eq!(outcome.logs, ["true", r#"  {"n": "indented"}"#, "[INFO] starting"]);
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
