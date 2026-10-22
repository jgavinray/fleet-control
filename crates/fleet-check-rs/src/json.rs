//! The JSON output: `{"exit_code": N, "engines": [...]}`, pretty-printed with
//! two spaces and sorted keys, matching Python's
//! `json.dumps(doc, indent=2, sort_keys=True)`.

use fleet_core::CheckResult;
use serde_json::{json, Value};

/// The whole document, with each engine's `CheckResult::to_json` record.
pub fn document(results: &[CheckResult], code: i32) -> Value {
    json!({
        "exit_code": code,
        "engines": results.iter().map(CheckResult::to_json).collect::<Vec<_>>(),
    })
}

/// The document as the text printed to stdout. serde_json's default map is a
/// BTreeMap, so every object's keys come out sorted (preserve_order stays off).
pub fn render(results: &[CheckResult], code: i32) -> String {
    serde_json::to_string_pretty(&document(results, code)).expect("JSON values serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::Feature;

    #[test]
    fn top_level_keys_are_sorted_and_exit_code_is_present() {
        let mut res = CheckResult::new("alpha", "http://127.0.0.1:1", "h", "tensorfold");
        res.features.insert(
            "logprobs".to_string(),
            Feature::record("no", "absent", Some("yes")),
        );

        let text = render(&[res], 1);
        let engines_at = text.find("\"engines\"").expect("engines key");
        let exit_at = text.find("\"exit_code\"").expect("exit_code key");
        assert!(
            engines_at < exit_at,
            "sorted keys put engines before exit_code:\n{text}"
        );
        assert!(text.contains("\"exit_code\": 1"), "{text}");

        let value: Value = serde_json::from_str(&text).expect("output is JSON");
        let engines = value["engines"].as_array().expect("engines array");
        assert_eq!(engines.len(), 1);
        assert_eq!(engines[0]["engine"], "alpha");
        assert_eq!(engines[0]["verdict"], "drift");
        assert_eq!(engines[0]["features"]["logprobs"]["drift"], true);
        assert!(text.contains("\"features\": {"), "{text}");
    }

    #[test]
    fn one_entry_per_result_in_order() {
        let first = CheckResult::new("beta", "u", "h", "tensorfold");
        let second = CheckResult::new("alpha", "u", "h", "tensorfold");
        let text = render(&[first, second], 0);
        let value: Value = serde_json::from_str(&text).expect("output is JSON");
        let engines = value["engines"].as_array().expect("engines array");
        assert_eq!(engines.len(), 2);
        assert_eq!(engines[0]["engine"], "beta");
        assert_eq!(engines[1]["engine"], "alpha");
        assert_eq!(value["exit_code"], 0);
        let sanity = &engines[0]["sanity"];
        assert_eq!(sanity["latency_s"], Value::Null);
        assert_eq!(sanity["result"], "n/a");
    }
}
