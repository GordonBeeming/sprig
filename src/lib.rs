pub mod backup;
pub mod config;
pub mod git;
pub mod operations;
pub mod platform;
pub mod state;

use serde::Serialize;
use serde_json::Value;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Serialize)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub data: Value,
}

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: serde_json::json!({}),
        }
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }
    pub fn exit_code(&self) -> i32 {
        match self.code {
            "E_ARGUMENT" | "E_CONFIG" => 2,
            "E_UNSUPPORTED" | "E_CLONE" => 3,
            "E_BUSY" | "E_EXISTS" | "E_CHANGED" | "E_DIRTY" => 4,
            _ => 5,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new("E_IO", e.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new("E_STATE", e.to_string())
    }
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub schema_version: u32,
    pub outcome: &'static str,
    pub code: &'static str,
    pub data: Value,
    pub warnings: Vec<Warning>,
}
#[derive(Debug, Serialize)]
pub struct Warning {
    pub code: &'static str,
    pub message: String,
}
impl Output {
    pub fn success(data: Value) -> Self {
        Self {
            schema_version: 1,
            outcome: "success",
            code: "OK",
            data,
            warnings: vec![],
        }
    }
    pub fn warn(&mut self, message: impl Into<String>) {
        self.warn_code("W_CONFIGURATION", message);
    }
    pub fn warn_code(&mut self, code: &'static str, message: impl Into<String>) {
        self.outcome = "partial_success";
        self.code = "W_PARTIAL";
        self.warnings.push(Warning {
            code,
            message: message.into(),
        });
    }
}
