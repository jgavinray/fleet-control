//! Command-line parsing, compatible with what `check/fleet_check.py` accepts.
//!
//! argparse semantics that matter here: `--opt value` and `--opt=value` both
//! work, long options may be abbreviated to a unique prefix, `-h`/`--help`
//! prints usage and exits 0, `--` ends option parsing, and a value that starts
//! with `-` is still a value when it is a negative number. Engine names are
//! positional and must form ONE run: an option after them ends the run, and a
//! later bare word is "unrecognized arguments" (exit 1).

use std::path::{Path, PathBuf};

use fleet_core::opts::Opts;
use fleet_core::registry::{load_registry, Registry};

/// argparse's usage block.
pub const USAGE: &str = "\
usage: fleet-check [-h] [--json] [--quick] [--registry REGISTRY]
                   [--timeout TIMEOUT] [--bg-timeout BG_TIMEOUT]
                   [--force-shared]
                   [engine ...]";

/// argparse's `--help` output: usage, description, option help, epilog.
pub const HELP: &str = "\
usage: fleet-check [-h] [--json] [--quick] [--registry REGISTRY]
                   [--timeout TIMEOUT] [--bg-timeout BG_TIMEOUT]
                   [--force-shared]
                   [engine ...]

Check that every model engine in fleet.toml is up and behaving.

positional arguments:
  engine                engine name(s) to check (default: all)

options:
  -h, --help            show this help message and exit
  --json                print JSON instead of a table
  --quick               skip the feature probes
  --registry REGISTRY   path to fleet.toml
  --timeout TIMEOUT     seconds per request (default 60)
  --bg-timeout BG_TIMEOUT
                        seconds a background-priority sanity request may wait
                        before it is re-sent once at normal priority (default
                        20)
  --force-shared        run sanity and probes on a shared engine even if it is
                        busy

Exit 0 all good, 1 an engine is down or wrong, 2 only feature drift.
";

/// Why parsing stopped. `Help` is not an error: it prints `HELP` and exits 0.
#[derive(Debug)]
pub enum ParseError {
    Help,
    Usage(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flag {
    Help,
    Json,
    Quick,
    Registry,
    Timeout,
    BgTimeout,
    ForceShared,
}

const LONG_OPTIONS: [(&str, Flag); 7] = [
    ("--help", Flag::Help),
    ("--json", Flag::Json),
    ("--quick", Flag::Quick),
    ("--registry", Flag::Registry),
    ("--timeout", Flag::Timeout),
    ("--bg-timeout", Flag::BgTimeout),
    ("--force-shared", Flag::ForceShared),
];

fn flag_name(flag: Flag) -> &'static str {
    match flag {
        Flag::Help => "--help",
        Flag::Json => "--json",
        Flag::Quick => "--quick",
        Flag::Registry => "--registry",
        Flag::Timeout => "--timeout",
        Flag::BgTimeout => "--bg-timeout",
        Flag::ForceShared => "--force-shared",
    }
}

enum Classified {
    Positional,
    Option(Flag, Option<String>),
    Unknown,
}

/// argparse's `_parse_optional`, restricted to the options this tool has.
fn classify(arg: &str) -> Result<Classified, ParseError> {
    if arg == "-" || !arg.starts_with('-') {
        return Ok(Classified::Positional);
    }
    if let Some(body) = arg.strip_prefix("--") {
        let (name, inline) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (body, None),
        };
        let long = format!("--{name}");
        if let Some((_, flag)) = LONG_OPTIONS.iter().find(|(option, _)| *option == long) {
            return Ok(Classified::Option(*flag, inline));
        }
        let candidates: Vec<&str> = LONG_OPTIONS
            .iter()
            .filter(|(option, _)| option.starts_with(&long))
            .map(|(option, _)| *option)
            .collect();
        return match candidates.as_slice() {
            [] => Ok(Classified::Unknown),
            [only] => {
                let flag = LONG_OPTIONS
                    .iter()
                    .find(|(option, _)| option == only)
                    .map(|(_, flag)| *flag)
                    .expect("the candidate came from LONG_OPTIONS");
                Ok(Classified::Option(flag, inline))
            }
            many => Err(usage(format!(
                "ambiguous option: {long} could match {}",
                many.join(", ")
            ))),
        };
    }
    if arg == "-h" {
        return Ok(Classified::Option(Flag::Help, None));
    }
    if let Some(value) = arg.strip_prefix("-h") {
        return Ok(Classified::Option(Flag::Help, Some(value.to_string())));
    }
    Ok(Classified::Unknown)
}

fn usage(message: String) -> ParseError {
    ParseError::Usage(message)
}

/// True when the token cannot be the value of the preceding option: it looks
/// like an option, except for a bare "-" and negative numbers (argparse's
/// `_negative_number_matcher`).
fn is_option_like(arg: &str) -> bool {
    arg != "-" && arg.starts_with('-') && !is_negative_number(arg)
}

/// Python's `^-\d+$|^-\d*\.\d+$`.
fn is_negative_number(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    match rest.split_once('.') {
        Some((int_part, frac_part)) => {
            int_part.bytes().all(|b| b.is_ascii_digit())
                && !frac_part.is_empty()
                && frac_part.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Parse one command line into the shared options. The defaults are Python's:
/// all engines in sorted order, `--timeout` 60, `--bg-timeout` 20, registry
/// from the environment or the default path.
pub fn parse(args: &[String]) -> Result<Opts, ParseError> {
    let mut quick = false;
    let mut json = false;
    let mut force_shared = false;
    let mut registry: Option<PathBuf> = None;
    let mut timeout = 60.0;
    let mut bg_timeout = 20.0;
    let mut engines: Vec<String> = Vec::new();
    let mut extras: Vec<String> = Vec::new();
    let mut run_ended = false;
    let mut positional_only = false;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if positional_only {
            if run_ended {
                extras.push(arg.to_string());
            } else {
                engines.push(arg.to_string());
            }
            i += 1;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            i += 1;
            continue;
        }

        match classify(arg)? {
            Classified::Positional => {
                if run_ended {
                    extras.push(arg.to_string());
                } else {
                    engines.push(arg.to_string());
                }
                i += 1;
            }
            Classified::Unknown => {
                extras.push(arg.to_string());
                if !engines.is_empty() {
                    run_ended = true;
                }
                i += 1;
            }
            Classified::Option(flag, inline) => {
                if !engines.is_empty() {
                    run_ended = true;
                }
                match flag {
                    Flag::Help => return Err(ParseError::Help),
                    Flag::Json | Flag::Quick | Flag::ForceShared => {
                        if let Some(value) = inline {
                            return Err(usage(format!(
                                "argument {}: ignored explicit argument '{value}'",
                                flag_name(flag)
                            )));
                        }
                        match flag {
                            Flag::Json => json = true,
                            Flag::Quick => quick = true,
                            _ => force_shared = true,
                        }
                        i += 1;
                    }
                    Flag::Registry | Flag::Timeout | Flag::BgTimeout => {
                        let (value, consumed) = match inline {
                            Some(value) => (value, 1),
                            None => match args.get(i + 1) {
                                Some(next) if !is_option_like(next) => (next.clone(), 2),
                                _ => {
                                    return Err(usage(format!(
                                        "argument {}: expected one argument",
                                        flag_name(flag)
                                    )))
                                }
                            },
                        };
                        match flag {
                            Flag::Registry => registry = Some(PathBuf::from(value)),
                            Flag::Timeout => {
                                timeout = parse_py_float(&value).ok_or_else(|| {
                                    usage(format!(
                                        "argument --timeout: invalid float value: '{value}'"
                                    ))
                                })?
                            }
                            _ => {
                                bg_timeout = parse_py_float(&value).ok_or_else(|| {
                                    usage(format!(
                                        "argument --bg-timeout: invalid float value: '{value}'"
                                    ))
                                })?
                            }
                        }
                        i += consumed;
                    }
                }
            }
        }
    }

    if !extras.is_empty() {
        return Err(usage(format!(
            "unrecognized arguments: {}",
            extras.join(" ")
        )));
    }

    Ok(Opts {
        quick,
        json,
        registry,
        timeout,
        bg_timeout,
        force_shared,
        engines,
    })
}

/// Python's `float(text)`, for the two timeout options. Python accepts
/// surrounding whitespace, a sign, `inf`/`infinity`/`nan` in any case,
/// underscores between digits, and forms like `.5`, `5.` and `1e1`. Rust's
/// `f64::from_str` rejects several of those, so the text is validated and
/// normalized first.
pub fn parse_py_float(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (sign, body) = match text.as_bytes()[0] {
        b'+' => (1.0, &text[1..]),
        b'-' => (-1.0, &text[1..]),
        _ => (1.0, text),
    };
    if body.is_empty() {
        return None;
    }
    let lower = body.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" {
        return Some(sign * f64::INFINITY);
    }
    if lower == "nan" {
        return Some(sign * f64::NAN);
    }

    let bytes = body.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'_' {
            let before = index
                .checked_sub(1)
                .and_then(|index| bytes.get(index))
                .is_some_and(u8::is_ascii_digit);
            let after = bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
            if !before || !after {
                return None;
            }
        }
    }
    let cleaned: String = body.chars().filter(|c| *c != '_').collect();

    let (mantissa, exponent) = match cleaned.find(['e', 'E']) {
        Some(index) => (&cleaned[..index], Some(&cleaned[index + 1..])),
        None => (cleaned.as_str(), None),
    };
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }

    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((int_part, frac_part)) => (int_part, Some(frac_part)),
        None => (mantissa, None),
    };
    if !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if frac_part.is_some_and(|frac| !frac.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    if int_part.is_empty() && frac_part.is_none_or(str::is_empty) {
        return None;
    }

    let mut normalized = String::new();
    if int_part.is_empty() {
        normalized.push('0');
    } else {
        normalized.push_str(int_part);
    }
    if let Some(frac) = frac_part {
        normalized.push('.');
        if frac.is_empty() {
            normalized.push('0');
        } else {
            normalized.push_str(frac);
        }
    }
    if let Some(exponent) = exponent {
        normalized.push('e');
        normalized.push_str(exponent);
    }
    let value: f64 = normalized.parse().ok()?;
    Some(sign * value)
}

/// Load the registry at `path`, applying the local LAN override exactly as
/// `fleet_core::registry::load_registry` does. The error text is Python's
/// `cannot read registry {path}: {error}`.
pub fn load_registry_at(path: &Path) -> Result<Registry, String> {
    load_registry(path).map_err(|error| error.to_string())
}

/// The engines to check, in report order: every registry engine sorted by name
/// when none were named, else the names as given (duplicates included, each
/// checked). Unknown names are one error listing them and the known engines.
pub fn select_engines(registry: &Registry, requested: &[String]) -> Result<Vec<String>, String> {
    let names: Vec<String> = if requested.is_empty() {
        registry.engines.keys().cloned().collect()
    } else {
        requested.to_vec()
    };
    let unknown: Vec<&str> = names
        .iter()
        .filter(|name| !registry.engines.contains_key(*name))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let known: Vec<&str> = registry.engines.keys().map(String::as_str).collect();
        return Err(format!(
            "unknown engine {}; known: {}",
            unknown.join(", "),
            known.join(", ")
        ));
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn fixture_registry() -> Registry {
        fleet_core::parse_registry(
            "[engines.alpha]\nurl = \"http://127.0.0.1:1\"\nmodel = \"m\"\nkind = \"tensorfold\"\n\n\
             [engines.beta]\nurl = \"http://127.0.0.1:1\"\nmodel = \"m\"\nkind = \"tensorfold\"\n",
        )
        .expect("fixture registry parses")
    }

    #[test]
    fn python_float_syntax_is_accepted() {
        for (text, want) in [
            ("5", 5.0),
            (".5", 0.5),
            ("0.25", 0.25),
            ("1e1", 10.0),
            ("5.", 5.0),
            (" 5 ", 5.0),
            ("+5", 5.0),
            ("-5", -5.0),
            ("-.5", -0.5),
            ("1_0", 10.0),
            ("1E-2", 0.01),
        ] {
            assert_eq!(parse_py_float(text), Some(want), "{text:?}");
        }
        assert!(parse_py_float("nan").is_some_and(f64::is_nan));
        assert_eq!(parse_py_float("inf"), Some(f64::INFINITY));
    }

    #[test]
    fn python_float_junk_is_rejected() {
        for text in [
            "abc", "/x", "", " ", "1__0", "_5", "5_", "1e", "--5", ".", "e1", "5 5",
        ] {
            assert_eq!(parse_py_float(text), None, "{text:?}");
        }
    }

    #[test]
    fn positionals_form_one_run() {
        let opts = parse(&argv(&["--quick", "a", "b"])).expect("flags first");
        assert_eq!(opts.engines, vec!["a", "b"]);
        assert!(opts.quick);

        let opts = parse(&argv(&["a", "b", "--quick"])).expect("flags last");
        assert_eq!(opts.engines, vec!["a", "b"]);
        assert!(opts.quick);

        for broken in [
            &["a", "--quick", "b"][..],
            &["a", "--json", "--registry", "r", "b"][..],
            &["a", "--timeout", "5", "b"][..],
        ] {
            match parse(&argv(broken)) {
                Err(ParseError::Usage(message)) => {
                    assert!(message.starts_with("unrecognized arguments:"), "{message}");
                }
                other => panic!("{broken:?} should be a usage error, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_flag_is_a_usage_error() {
        match parse(&argv(&["--no-such-flag"])) {
            Err(ParseError::Usage(message)) => {
                assert_eq!(message, "unrecognized arguments: --no-such-flag");
            }
            other => panic!("unknown flag should fail, got {other:?}"),
        }
        assert!(matches!(
            parse(&argv(&["-x"])),
            Err(ParseError::Usage(message)) if message == "unrecognized arguments: -x"
        ));
    }

    #[test]
    fn registry_takes_both_forms() {
        let opts = parse(&argv(&["--registry=/tmp/x", "--quick"])).expect("equals form");
        assert_eq!(opts.registry, Some(PathBuf::from("/tmp/x")));
        assert!(opts.quick);

        let opts = parse(&argv(&["--registry", "/tmp/y"])).expect("separate value");
        assert_eq!(opts.registry, Some(PathBuf::from("/tmp/y")));

        let opts = parse(&argv(&["--reg=/tmp/z"])).expect("abbreviated equals form");
        assert_eq!(opts.registry, Some(PathBuf::from("/tmp/z")));

        assert!(matches!(
            parse(&argv(&["--registry"])),
            Err(ParseError::Usage(message)) if message == "argument --registry: expected one argument"
        ));
    }

    #[test]
    fn timeout_values_and_errors() {
        let opts = parse(&argv(&["--timeout", ".5", "--bg-timeout=1e1"])).expect("python floats");
        assert_eq!(opts.timeout, 0.5);
        assert_eq!(opts.bg_timeout, 10.0);

        let opts = parse(&argv(&["--timeout", "-5"])).expect("negative number is a value");
        assert_eq!(opts.timeout, -5.0);

        for bad in ["abc", "/x", ""] {
            let broken = if bad.is_empty() {
                argv(&["--timeout="])
            } else {
                argv(&["--timeout", bad])
            };
            assert!(
                matches!(parse(&broken), Err(ParseError::Usage(_))),
                "{bad:?} should be an invalid float"
            );
        }
        assert!(matches!(
            parse(&argv(&["--timeout"])),
            Err(ParseError::Usage(_))
        ));
        assert!(matches!(
            parse(&argv(&["--timeout", "--json"])),
            Err(ParseError::Usage(_))
        ));
    }

    #[test]
    fn help_prints_usage_and_succeeds() {
        assert!(matches!(parse(&argv(&["--help"])), Err(ParseError::Help)));
        assert!(matches!(parse(&argv(&["-h"])), Err(ParseError::Help)));
    }

    #[test]
    fn flags_do_not_take_values() {
        match parse(&argv(&["--json=true"])) {
            Err(ParseError::Usage(message)) => {
                assert_eq!(message, "argument --json: ignored explicit argument 'true'");
            }
            other => panic!("--json=true should fail, got {other:?}"),
        }
    }

    #[test]
    fn engine_selection_sorts_by_default_and_keeps_duplicates() {
        let registry = fixture_registry();
        assert_eq!(
            select_engines(&registry, &[]).expect("default selection"),
            vec!["alpha", "beta"]
        );
        assert_eq!(
            select_engines(&registry, &argv(&["beta", "beta"])).expect("duplicates allowed"),
            vec!["beta", "beta"],
            "two entries mean two checks"
        );
    }

    #[test]
    fn unknown_engine_names_the_known_ones() {
        let registry = fixture_registry();
        assert_eq!(
            select_engines(&registry, &argv(&["nope", "alpha"])).unwrap_err(),
            "unknown engine nope; known: alpha, beta"
        );
    }

    #[test]
    fn registry_fixture_loads_from_a_tempdir() {
        let dir =
            std::env::temp_dir().join(format!("fleet-check-rs-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join("fleet.toml");
        std::fs::write(
            &path,
            "[engines.alpha]\nurl = \"http://127.0.0.1:1\"\nmodel = \"m\"\nkind = \"tensorfold\"\n",
        )
        .expect("write fixture");
        let registry = load_registry_at(&path).expect("fixture loads");
        assert_eq!(
            select_engines(&registry, &[]).expect("selection"),
            vec!["alpha"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_registry_is_an_error() {
        let dir =
            std::env::temp_dir().join(format!("fleet-check-rs-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("missing.toml");
        let message = load_registry_at(&path).unwrap_err();
        assert!(
            message.starts_with(&format!("cannot read registry {}", path.display())),
            "{message}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
