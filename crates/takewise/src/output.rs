//! Printing results as human text or `--json`.

use std::process::ExitCode;

use serde::Serialize;
use serde_json::{Value, json};

/// A command's result: data for `--json`, text for people, and an optional
/// finding (exit code 1).
pub struct Outcome {
    pub data: Value,
    pub text: String,
    pub finding: Option<Finding>,
}

pub struct Finding {
    pub code: &'static str,
    pub message: String,
}

impl Outcome {
    pub fn new(data: impl Serialize, text: impl Into<String>) -> Self {
        Self {
            data: serde_json::to_value(data).unwrap_or(Value::Null),
            text: text.into(),
            finding: None,
        }
    }

    pub fn with_finding(mut self, code: &'static str, message: impl Into<String>) -> Self {
        self.finding = Some(Finding {
            code,
            message: message.into(),
        });
        self
    }

    pub fn print(self, as_json: bool) -> ExitCode {
        if as_json {
            let value = match &self.finding {
                None => json!({ "ok": true, "data": self.data }),
                Some(finding) => json!({
                    "ok": false,
                    "error": { "code": finding.code, "message": finding.message },
                    "data": self.data,
                }),
            };
            println!("{value}");
        } else {
            if !self.text.is_empty() {
                println!("{}", self.text.trim_end());
            }
            if let Some(finding) = &self.finding {
                eprintln!("{}", finding.message);
            }
        }
        if self.finding.is_some() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        }
    }
}

/// Prints a usage or setup error: JSON on stdout, or text on stderr.
pub fn print_error(as_json: bool, code: &str, message: &str) {
    if as_json {
        println!(
            "{}",
            json!({ "ok": false, "error": { "code": code, "message": message.trim_end() } })
        );
    } else {
        eprintln!("error: {}", message.trim_end());
    }
}
