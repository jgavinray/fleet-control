//! The table output, a 1:1 port of `render_table` and `feature_cell` in
//! `check/fleet_check.py`: same headers, same status text, same column widths
//! (widest cell, two-space join, right-trimmed lines), same detail-line order,
//! and the same summary line.

use fleet_core::{py_str, CheckResult, Status};

/// Python's `FEATURES` tuple: the probe order used in cells and detail lines.
pub const FEATURES: [&str; 4] = [
    "guided_choice",
    "background_priority",
    "logprobs",
    "prompt_logprobs",
];

/// Python's `SHORT` map.
fn short_name(feature: &str) -> &str {
    match feature {
        "guided_choice" => "guided",
        "background_priority" => "bg",
        "logprobs" => "logprobs",
        "prompt_logprobs" => "prompt_lp",
        other => other,
    }
}

/// Python's `feature_cell`: the FEATURES column for one engine.
pub fn feature_cell(res: &CheckResult, quick: bool) -> String {
    if res.features.is_empty() {
        if res.status == Status::SkippedBusy {
            return "skipped (busy)".to_string();
        }
        return if quick && res.kind == "tensorfold" {
            "(--quick)".to_string()
        } else {
            "n/a".to_string()
        };
    }
    if res
        .features
        .values()
        .all(|feature| feature.result == "skipped")
    {
        return "skipped (busy)".to_string();
    }
    let mut parts = Vec::new();
    for name in FEATURES {
        if let Some(feature) = res.features.get(name) {
            parts.push(format!(
                "{}={}{}",
                short_name(name),
                feature.result,
                if feature.drift { "!" } else { "" }
            ));
        }
    }
    parts.join(" ")
}

/// Python's `render_table`: headers and rows, then the detail lines separated
/// from them by a blank line. No line carries trailing spaces.
pub fn render_table(results: &[CheckResult], quick: bool) -> String {
    let headers: Vec<String> = [
        "ENGINE", "STATUS", "MODEL", "LOAD", "SANITY", "LATENCY", "FEATURES",
    ]
    .iter()
    .map(|header| header.to_string())
    .collect();

    let rows: Vec<Vec<String>> = results
        .iter()
        .map(|res| {
            let latency = match res.sanity.latency_s {
                None => "-".to_string(),
                Some(seconds) => format!("{seconds:.2}s"),
            };
            let mut model = if res.model.is_empty() {
                "-".to_string()
            } else {
                res.model.clone()
            };
            if !res.version.is_empty() {
                model.push_str(&format!(" v{}", res.version));
            }
            let status = if res.status == Status::SkippedBusy {
                "BUSY".to_string()
            } else {
                res.verdict().to_uppercase()
            };
            let load = if res.load.is_empty() {
                "-".to_string()
            } else {
                res.load.clone()
            };
            vec![
                res.name.clone(),
                status,
                model,
                load,
                res.sanity.result.clone(),
                latency,
                feature_cell(res, quick),
            ]
        })
        .collect();

    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut lines: Vec<String> = Vec::with_capacity(rows.len() + 1);
    lines.push(format_row(&headers, &widths));
    for row in &rows {
        lines.push(format_row(row, &widths));
    }

    let mut detail: Vec<String> = Vec::new();
    for res in results {
        for problem in &res.problems {
            detail.push(format!("{}: PROBLEM {problem}", res.name));
        }
        for name in FEATURES {
            if let Some(feature) = res.features.get(name) {
                if feature.result != "yes" && feature.result != "skipped" {
                    let tag = if feature.drift { "DRIFT " } else { "" };
                    let expected = if feature.drift {
                        format!(
                            " (expected {})",
                            feature
                                .expected
                                .as_ref()
                                .map(py_str)
                                .unwrap_or_else(|| "None".to_string())
                        )
                    } else {
                        String::new()
                    };
                    detail.push(format!(
                        "{}: {}{} = {}{}: {}",
                        res.name, tag, name, feature.result, expected, feature.reason
                    ));
                }
            }
        }
        for note in &res.notes {
            detail.push(format!("{}: note: {note}", res.name));
        }
    }

    let mut out = lines.join("\n");
    if !detail.is_empty() {
        out.push_str("\n\n");
        out.push_str(&detail.join("\n"));
    }
    out
}

/// Python's summary text for the exit code printed under the table.
pub fn summary(code: i32) -> &'static str {
    match code {
        0 => "ALL GOOD",
        1 => "PROBLEM: an engine is down or answered wrong",
        _ => "DRIFT: engines work, but a feature differs from the registry",
    }
}

/// One row: cells left-justified to the column widths and joined with two
/// spaces, then right-trimmed like Python's `.rstrip()`.
fn format_row(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        for _ in cell.chars().count()..widths[index] {
            line.push(' ');
        }
    }
    line.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::Feature;
    use serde_json::json;

    fn result(name: &str) -> CheckResult {
        CheckResult::new(name, "http://127.0.0.1:1", "h", "tensorfold")
    }

    #[test]
    fn feature_cells_match_python() {
        let plain = result("a");
        assert_eq!(feature_cell(&plain, true), "(--quick)");
        assert_eq!(feature_cell(&plain, false), "n/a");

        let mut busy = result("a");
        busy.status = Status::SkippedBusy;
        assert_eq!(feature_cell(&busy, false), "skipped (busy)");

        let mut skipped = result("a");
        for name in FEATURES {
            skipped
                .features
                .insert(name.to_string(), Feature::record("skipped", "busy", None));
        }
        assert_eq!(feature_cell(&skipped, false), "skipped (busy)");

        let mut drifted = result("a");
        drifted.features.insert(
            "logprobs".to_string(),
            Feature::record("no", "absent", Some(&json!("yes"))),
        );
        assert_eq!(feature_cell(&drifted, false), "logprobs=no!");
    }

    #[test]
    fn table_widths_padding_and_rstrip() {
        let mut res = result("a");
        res.model = "m".to_string();
        res.version = "1.0".to_string();
        res.load = "idle".to_string();
        res.sanity.result = "pass".to_string();
        res.sanity.latency_s = Some(0.5);

        let out = render_table(&[res], true);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "ENGINE  STATUS  MODEL   LOAD  SANITY  LATENCY  FEATURES"
        );
        assert_eq!(
            lines[1],
            "a       OK      m v1.0  idle  pass    0.50s    (--quick)"
        );
        assert!(out.lines().all(|line| !line.ends_with(' ')), "{out:?}");
    }

    #[test]
    fn busy_row_and_problem_detail() {
        let mut res = result("b");
        res.status = Status::SkippedBusy;
        res.load = "busy (1 running, 0 waiting)".to_string();
        res.sanity.result = "skipped".to_string();
        res.problems.push("boom".to_string());

        let out = render_table(&[res], false);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("BUSY"), "{:?}", lines[1]);
        assert!(lines[1].contains("-"), "model shows as - when empty");
        let blank = lines.iter().position(|line| line.is_empty());
        assert_eq!(blank, Some(2), "blank line before detail");
        assert_eq!(lines[3], "b: PROBLEM boom");
    }

    #[test]
    fn drift_detail_uses_tag_and_expected_then_notes() {
        let mut res = result("c");
        res.features.insert(
            "logprobs".to_string(),
            Feature::record("no", "absent", Some(&json!("yes"))),
        );
        res.features.insert(
            "guided_choice".to_string(),
            Feature::record("yes", "picked", Some(&json!("yes"))),
        );
        res.notes.push("starved once".to_string());

        let out = render_table(&[res], false);
        assert!(
            out.contains("c: DRIFT logprobs = no (expected yes): absent"),
            "{out:?}"
        );
        assert!(
            !out.contains("c: guided_choice"),
            "yes features are not detailed"
        );
        assert!(out.ends_with("c: note: starved once"));
    }

    #[test]
    fn drift_detail_renders_a_raw_expected_value() {
        let mut res = result("d");
        res.features.insert(
            "logprobs".to_string(),
            Feature::record("no", "absent", Some(&json!(1))),
        );

        let out = render_table(&[res], false);
        assert!(
            out.contains("d: DRIFT logprobs = no (expected 1): absent"),
            "{out:?}"
        );
    }

    #[test]
    fn summary_texts() {
        assert_eq!(summary(0), "ALL GOOD");
        assert_eq!(summary(1), "PROBLEM: an engine is down or answered wrong");
        assert_eq!(
            summary(2),
            "DRIFT: engines work, but a feature differs from the registry"
        );
    }
}
