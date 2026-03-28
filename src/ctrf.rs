//! CTRF (Common Test Results Format) types.
//!
//! A minimal implementation of the CTRF spec for parsing and producing
//! test result reports. See <https://ctrf.io> for the full specification.

use serde::{Deserialize, Serialize};

/// A CTRF report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub report_format: String,
    pub spec_version: String,
    pub results: Results,
}

/// The results section of a CTRF report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Results {
    pub tool: Tool,
    pub summary: Summary,
    pub tests: Vec<Test>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,
}

/// The tool that produced the test results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Aggregate summary of test results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub tests: u64,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<u64>,
}

/// A single test result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Test {
    pub name: String,
    pub status: Status,
    pub duration: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flaky: Option<bool>,
}

/// Test status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Passed,
    Failed,
    Skipped,
    Pending,
    Other,
}

/// Optional environment metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
}

impl Report {
    /// Whether all tests passed (no failures).
    pub fn all_passed(&self) -> bool {
        self.results.summary.failed == 0
    }

    /// Parse a CTRF report from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

impl Summary {
    /// Build a summary by counting test statuses.
    pub fn from_tests(tests: &[Test]) -> Self {
        let mut passed = 0u64;
        let mut failed = 0u64;
        let mut skipped = 0u64;
        let mut pending = 0u64;
        let mut other = 0u64;
        for test in tests {
            match test.status {
                Status::Passed => passed += 1,
                Status::Failed => failed += 1,
                Status::Skipped => skipped += 1,
                Status::Pending => pending += 1,
                Status::Other => other += 1,
            }
        }
        Self {
            tests: tests.len() as u64,
            passed,
            failed,
            skipped,
            pending: if pending > 0 { Some(pending) } else { None },
            other: if other > 0 { Some(other) } else { None },
            start: None,
            stop: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_report() {
        let report = Report {
            report_format: "CTRF".into(),
            spec_version: "0.0.1".into(),
            results: Results {
                tool: Tool {
                    name: "cargo-test".into(),
                    version: Some("1.0.0".into()),
                },
                summary: Summary {
                    tests: 3,
                    passed: 2,
                    failed: 1,
                    skipped: 0,
                    pending: None,
                    other: None,
                    start: Some(1000),
                    stop: Some(2000),
                },
                tests: vec![
                    Test {
                        name: "test_add".into(),
                        status: Status::Passed,
                        duration: 50,
                        suite: None,
                        message: None,
                        trace: None,
                        file_path: None,
                        flaky: None,
                    },
                    Test {
                        name: "test_sub".into(),
                        status: Status::Passed,
                        duration: 30,
                        suite: Some("math".into()),
                        message: None,
                        trace: None,
                        file_path: Some("src/math.rs".into()),
                        flaky: None,
                    },
                    Test {
                        name: "test_div_zero".into(),
                        status: Status::Failed,
                        duration: 10,
                        suite: None,
                        message: Some("division by zero".into()),
                        trace: Some("at src/math.rs:42".into()),
                        file_path: None,
                        flaky: Some(false),
                    },
                ],
                environment: None,
            },
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed = Report::from_json(&json).unwrap();
        assert_eq!(parsed.report_format, "CTRF");
        assert_eq!(parsed.results.summary.tests, 3);
        assert_eq!(parsed.results.summary.failed, 1);
        assert!(!parsed.all_passed());
    }

    #[test]
    fn parse_minimal_report() {
        let json = r#"{
            "reportFormat": "CTRF",
            "specVersion": "0.0.1",
            "results": {
                "tool": { "name": "test" },
                "summary": { "tests": 1, "passed": 1, "failed": 0, "skipped": 0 },
                "tests": [
                    { "name": "it_works", "status": "passed", "duration": 5 }
                ]
            }
        }"#;
        let report = Report::from_json(json).unwrap();
        assert!(report.all_passed());
        assert_eq!(report.results.tests.len(), 1);
        assert_eq!(report.results.tests[0].status, Status::Passed);
    }

    #[test]
    fn summary_from_tests() {
        let tests = vec![
            Test {
                name: "a".into(),
                status: Status::Passed,
                duration: 10,
                suite: None,
                message: None,
                trace: None,
                file_path: None,
                flaky: None,
            },
            Test {
                name: "b".into(),
                status: Status::Failed,
                duration: 20,
                suite: None,
                message: None,
                trace: None,
                file_path: None,
                flaky: None,
            },
            Test {
                name: "c".into(),
                status: Status::Skipped,
                duration: 0,
                suite: None,
                message: None,
                trace: None,
                file_path: None,
                flaky: None,
            },
        ];
        let summary = Summary::from_tests(&tests);
        assert_eq!(summary.tests, 3);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1);
    }

    #[test]
    fn status_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Status::Passed).unwrap(), "\"passed\"");
        assert_eq!(serde_json::to_string(&Status::Failed).unwrap(), "\"failed\"");
    }
}
