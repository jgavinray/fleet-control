//! One engine check, dispatched by registry `kind` (Python's `check_engine`).
//!
//! The result is built from the registry entry, the counting client is created
//! with the shared `--timeout`, and the kind decides what runs. An unknown kind
//! is a `wrong` result with zero requests, exactly like Python.

use crate::http::Client;
use crate::opts::Opts;
use crate::registry::EngineConfig;
use crate::result::{CheckResult, Status};

/// Check one engine and return its report record.
pub fn check_engine(name: &str, cfg: &EngineConfig, opts: &Opts) -> CheckResult {
    let mut res = CheckResult::new(name, cfg.url.clone(), cfg.host.clone(), cfg.kind.clone());
    let mut client = Client::new(cfg.base_url(), opts.timeout);

    match cfg.kind.as_str() {
        "tensorfold" => crate::kinds_tensorfold::check_tensorfold(&mut res, cfg, &mut client, opts),
        kind => match crate::kinds_rest::rest_kind_check(kind) {
            Some(check) => check(&mut res, cfg, &mut client, opts),
            None => res.fail(Status::Wrong, format!("unknown kind '{kind}' in registry")),
        },
    }

    res.requests = client.count;
    res
}
