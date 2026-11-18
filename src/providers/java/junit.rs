//! Test results from the JUnit XML reports the JUnit Platform's legacy
//! reporter writes: the Ant format, with one `testcase` per test.

use std::path::Path;
use std::time::Duration;

use indexmap::IndexMap;

use crate::output::TargetWriter;
use crate::provider::BoxError;
use crate::providers::ReportMode;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Outcome {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct TestCase {
    pub name: String,
    pub duration: Option<Duration>,
    pub outcome: Outcome,
    /// The failure's message and stack trace, then anything the test
    /// printed.
    pub output: String,
}

/// The tests of one class, the suite bit reports.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Suite {
    pub name: String,
    pub tests: Vec<TestCase>,
}

impl Suite {
    fn count(&self, outcome: &Outcome) -> usize {
        self.tests.iter().filter(|test| test.outcome == *outcome).count()
    }

    fn duration(&self) -> Option<Duration> {
        self.tests.iter().map(|test| test.duration).sum()
    }
}

/// Every test in the `.xml` reports in `dir`, grouped by class in the order
/// the reports list them.
pub(super) fn read_reports(dir: &Path) -> Result<Vec<Suite>, BoxError> {
    let mut suites: IndexMap<String, Vec<TestCase>> = IndexMap::new();
    let reports = crate::providers::files_under(dir)?
        .into_iter()
        .filter(|file| file.extension().is_some_and(|e| e == "xml"));
    for report in reports {
        let xml = std::fs::read_to_string(&report)?;
        let cases = parse(&xml).map_err(|e| format!("cannot read test report {}: {e}", report.display()))?;
        for (class, case) in cases {
            suites.entry(class).or_default().push(case);
        }
    }
    Ok(suites.into_iter().map(|(name, tests)| Suite { name, tests }).collect())
}

/// Each `testcase` in a report, with its class name.
fn parse(xml: &str) -> Result<Vec<(String, TestCase)>, BoxError> {
    let document = roxmltree::Document::parse(xml)?;
    let mut cases = Vec::new();
    for case in document.descendants().filter(|node| node.has_tag_name("testcase")) {
        let class = case.attribute("classname").unwrap_or("tests").to_owned();
        let name = case.attribute("name").unwrap_or("").to_owned();
        let duration = case
            .attribute("time")
            .and_then(|time| time.parse::<f64>().ok())
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
        let mut outcome = Outcome::Passed;
        let mut failure = Vec::new();
        let mut printed = Vec::new();
        for child in case.children().filter(|node| node.is_element()) {
            let text = child.text().unwrap_or("").trim();
            match child.tag_name().name() {
                "failure" | "error" => {
                    outcome = Outcome::Failed;
                    // The stack trace starts with the message, so it is only
                    // shown on its own when there is no trace.
                    match (child.attribute("message"), text) {
                        (Some(message), "") => failure.push(message.to_owned()),
                        _ => failure.push(text.to_owned()),
                    }
                }
                "skipped" if outcome != Outcome::Failed => outcome = Outcome::Skipped,
                "system-out" | "system-err" if !text.is_empty() => printed.push(text.to_owned()),
                _ => {}
            }
        }
        failure.extend(printed);
        let output = failure.join("\n");
        cases.push((
            class,
            TestCase {
                name,
                duration,
                outcome,
                output,
            },
        ));
    }
    Ok(cases)
}

/// Show `suites` as `mode` asks and return whether every test passed or was
/// skipped.
pub(super) fn report(suites: &[Suite], writer: &TargetWriter, mode: ReportMode) -> bool {
    let per_test = mode.per_test();
    let mut all_passed = true;
    for suite in suites {
        let passed = suite.count(&Outcome::Passed);
        let failed = suite.count(&Outcome::Failed);
        let skipped = suite.count(&Outcome::Skipped);
        all_passed &= failed == 0;
        if per_test {
            for test in &suite.tests {
                match test.outcome {
                    Outcome::Passed => writer.test_passed(&suite.name, &test.name, test.duration),
                    Outcome::Failed => writer.test_failed(&suite.name, &test.name, test.duration, &test.output),
                    Outcome::Skipped => writer.test_skipped(&suite.name, &test.name),
                }
            }
        } else if failed > 0 {
            let failures: Vec<_> = suite
                .tests
                .iter()
                .filter(|test| test.outcome == Outcome::Failed)
                .map(|test| (test.name.clone(), test.duration, test.output.clone()))
                .collect();
            writer.test_suite_failed(&suite.name, suite.duration(), passed, failed, &failures);
        } else if passed == 0 {
            writer.test_suite_skipped(&suite.name);
        } else {
            writer.test_suite_passed(&suite.name, suite.duration(), passed, skipped);
        }
    }
    all_passed
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="JUnit Jupiter" tests="4" skipped="1" failures="1" errors="0">
  <properties><property name="java.version" value="25"/></properties>
  <testcase name="adds()" classname="com.example.CalculatorTest" time="0.012">
    <system-out><![CDATA[computed 2]]></system-out>
  </testcase>
  <testcase name="divides()" classname="com.example.CalculatorTest" time="0.5">
    <failure message="expected: &lt;2&gt; but was: &lt;3&gt;" type="org.opentest4j.AssertionFailedError"><![CDATA[org.opentest4j.AssertionFailedError: expected: <2> but was: <3>
	at com.example.CalculatorTest.divides(CalculatorTest.java:12)]]></failure>
    <system-out><![CDATA[dividing]]></system-out>
  </testcase>
  <testcase name="later()" classname="com.example.CalculatorTest" time="0">
    <skipped><![CDATA[not yet]]></skipped>
  </testcase>
  <testcase name="[1] 1, 2" classname="com.example.Other$Nested" time="0.001"/>
</testsuite>
"#;

    #[test]
    fn reads_tests_grouped_by_class() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("TEST-junit-jupiter.xml"), REPORT).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a report").unwrap();
        let suites = read_reports(dir.path()).unwrap();

        assert_eq!(
            suites.iter().map(|suite| suite.name.as_str()).collect::<Vec<_>>(),
            ["com.example.CalculatorTest", "com.example.Other$Nested"]
        );
        let calculator = &suites[0];
        assert_eq!(calculator.tests[0].outcome, Outcome::Passed);
        assert_eq!(calculator.tests[0].duration, Some(Duration::from_millis(12)));
        assert_eq!(calculator.tests[1].outcome, Outcome::Failed);
        assert!(
            calculator.tests[1]
                .output
                .starts_with("org.opentest4j.AssertionFailedError: expected: <2> but was: <3>\n\tat "),
            "{}",
            calculator.tests[1].output
        );
        assert!(calculator.tests[1].output.ends_with("\ndividing"));
        assert_eq!(calculator.tests[2].outcome, Outcome::Skipped);
        assert_eq!(suites[1].tests[0].name, "[1] 1, 2");
    }

    #[test]
    fn a_failure_without_a_trace_shows_its_message() {
        let cases = parse(
            r#"<testsuite><testcase name="t" classname="C"><error message="boom" type="E"/></testcase></testsuite>"#,
        )
        .unwrap();
        assert_eq!(cases[0].1.outcome, Outcome::Failed);
        assert_eq!(cases[0].1.output, "boom");
    }

    #[test]
    fn report_passes_only_without_failures() {
        let output = crate::output::Output::new(&[]);
        let writer = output.writer("test");
        let suite = |outcome: Outcome| Suite {
            name: "C".into(),
            tests: vec![TestCase {
                name: "t".into(),
                duration: None,
                outcome,
                output: String::new(),
            }],
        };
        for mode in [ReportMode::Suites, ReportMode::Tests] {
            assert!(report(
                &[suite(Outcome::Passed), suite(Outcome::Skipped)],
                &writer,
                mode
            ));
            assert!(!report(
                &[suite(Outcome::Passed), suite(Outcome::Failed)],
                &writer,
                mode
            ));
        }
    }

    #[test]
    fn rejects_malformed_reports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("TEST-broken.xml"), "<testsuite>").unwrap();
        let error = read_reports(dir.path()).unwrap_err().to_string();
        assert!(error.contains("cannot read test report"), "{error}");
    }
}
