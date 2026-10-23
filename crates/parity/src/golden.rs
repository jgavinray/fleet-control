//! The committed golden records: one file per scenario, one or two runs
//! inside. The recorder is the Python tool itself; these files are data.

use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub struct GoldenFile {
    pub scenario: String,
    pub runs: Vec<GoldenRun>,
}

#[derive(Deserialize)]
pub struct GoldenRun {
    pub label: String,
    /// quick | full | help (informational; `style` drives the comparison).
    pub mode: String,
    /// json | usage | help: which of parity.py's comparison paths applies.
    pub style: String,
    /// The command line after the binary; `{registry}` and `{registry_dir}`
    /// stand for the run's temp paths.
    pub args: Vec<String>,
    /// `{registry_dir}` for the relative-registry case, else None.
    pub cwd: Option<String>,
    pub env_reg: bool,
    pub exit_code: i32,
    pub stdout_empty: bool,
    pub stderr_empty: bool,
    #[serde(default)]
    pub python_stderr_head: Option<String>,
    #[serde(default)]
    pub json_exit_code: Option<i64>,
    #[serde(default)]
    pub engine_order: Vec<String>,
    #[serde(default)]
    pub engines: Vec<Value>,
    #[serde(default)]
    pub help_text: Option<String>,
}
