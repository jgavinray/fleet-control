//! Secret masking: one rule table, typed markers, a CLI. Port of
//! `lib/fleetlib/redact.py`, rule for rule. Every masked span becomes
//! `[REDACTED:<type>]`; rules run in order, most specific first, each `sub`
//! seeing the previous rule's output.
//!
//! Fidelity notes (the Python original uses `re` lookarounds, which the
//! `regex` crate lacks):
//! - Lookbehind/lookahead are emulated by inspecting the bytes around the
//!   match. A *regex-level* rejection (the engine would have refused the
//!   match) re-searches from `match_start + 1`, exactly like Python's engine
//!   advancing its scan position; an *fn-level* rejection (`fn` returned
//!   `None`) consumes the match and keeps it verbatim, like the `sub`
//!   wrapper. See [`Outcome`].
//! - `\s`/`\w`/`\d` follow the Unicode tables on both sides (Python's
//!   default, the `regex` crate default).
//! - Whitespace tests on byte values cover the ASCII space set plus the
//!   common separators; exotic Unicode spaces before a `=` are rare enough
//!   to note rather than chase.

use regex::Captures;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

pub const MARK: &str = "[REDACTED:";

pub fn marker(kind: &str) -> String {
    format!("{MARK}{kind}]")
}

fn count_marks(s: &str) -> usize {
    s.matches(MARK).count()
}

/// Outcome of one candidate match, mirroring the two rejection layers of the
/// Python implementation.
enum Outcome {
    /// The Python regex engine would not have matched here: advance the scan
    /// to `match_start + 1` and look for the next candidate.
    NoMatch,
    /// The rule's `fn` returned `None`: the `sub` wrapper keeps the matched
    /// text and the scan continues after it.
    Keep,
    /// Replace the whole match.
    Replace(String),
    /// Replace an explicit byte region of the match (used where Python's
    /// greedy quantifier backtracks, e.g. base64 padding).
    ReplaceRegion(usize, usize, String),
}

type Action = Box<dyn Fn(&Captures, &str) -> Outcome + Send + Sync>;

struct Rule {
    id: &'static str,
    kind: &'static str,
    re: Regex,
    action: Action,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(build_rules);

fn rules() -> &'static [Rule] {
    &RULES
}

/// The rule ids and marker types, in table order (for `redact --list-rules`).
pub fn rule_table() -> Vec<(&'static str, &'static str)> {
    rules().iter().map(|r| (r.id, r.kind)).collect()
}

// ------------------------------------------------------------------ scanning

fn sub_rule(rule: &Rule, text: &str, counts: &mut HashMap<&'static str, u64>) -> String {
    let mut out = String::new();
    // Everything before out_pos is written; search emulates the engine scan
    // position (a refused match advances the search by one, never the output).
    let mut out_pos = 0usize;
    let mut search = 0usize;
    loop {
        let Some(m) = rule.re.find_at(text, search) else {
            break;
        };
        let caps = rule
            .re
            .captures_at(text, search)
            .expect("match implies caps");
        let start = m.start();
        match (rule.action)(&caps, text) {
            Outcome::NoMatch => search = start + 1,
            Outcome::Keep => {
                out.push_str(&text[out_pos..m.end()]);
                out_pos = m.end();
                search = m.end();
            }
            Outcome::Replace(repl) => {
                let added = count_marks(&repl) - count_marks(m.as_str());
                out.push_str(&text[out_pos..start]);
                out.push_str(&repl);
                if added > 0 {
                    *counts.entry(rule.kind).or_default() += added as u64;
                }
                out_pos = m.end();
                search = m.end();
            }
            Outcome::ReplaceRegion(rs, re_, repl) => {
                let added = count_marks(&repl) - count_marks(&text[rs..re_]);
                out.push_str(&text[out_pos..rs]);
                out.push_str(&repl);
                if added > 0 {
                    *counts.entry(rule.kind).or_default() += added as u64;
                }
                out_pos = re_;
                search = re_;
            }
        }
    }
    out.push_str(&text[out_pos..]);
    out
}

/// Mask `text`. Returns the masked text and the number of masked spans, the
/// same contract as `redact(text)` in the Python module.
pub fn redact(text: &str) -> (String, u64) {
    if text.is_empty() {
        return (String::new(), 0);
    }
    let mut counts: HashMap<&'static str, u64> = HashMap::new();
    let mut cur = text.to_string();
    for rule in rules() {
        // secret-flag needs Python's prefix/core backtracking, so it runs as
        // a hand-written scan instead of one regex (see sub_flag).
        cur = if rule.id == "secret-flag" {
            sub_flag(&cur, &mut counts)
        } else {
            sub_rule(rule, &cur, &mut counts)
        };
    }
    (cur, counts.values().sum())
}

// ------------------------------------------------------------------ byte helpers

fn byte_at(text: &str, i: usize) -> Option<u8> {
    text.as_bytes().get(i).copied()
}

fn before_in(text: &str, start: usize, is: impl Fn(u8) -> bool) -> bool {
    matches!(start.checked_sub(1).and_then(|i| byte_at(text, i)), Some(b) if is(b))
}

fn after_in(text: &str, end: usize, is: impl Fn(u8) -> bool) -> bool {
    matches!(byte_at(text, end), Some(b) if is(b))
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn starts_with_any(s: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| s.starts_with(p))
}

fn has(pattern: &Regex, s: &str) -> bool {
    pattern.is_match(s)
}

fn re_of(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static rule pattern compiles")
}

// ------------------------------------------------------------------ predicates

const NOT_A_VALUE_START: &[&str] = &["$", "{{", "<", "%", MARK];
const TYPE_WORDS: &[&str] = &[
    "str",
    "string",
    "int",
    "integer",
    "bool",
    "boolean",
    "none",
    "null",
    "nil",
    "true",
    "false",
    "undefined",
    "required",
    "optional",
    "bytes",
    "any",
    "self",
    "args",
    "kwargs",
    "env",
];

fn looks_like_code_or_placeholder(v: &str, env_assignment: bool) -> bool {
    if v.is_empty() {
        return true;
    }
    if starts_with_any(v, NOT_A_VALUE_START) {
        return true;
    }
    if !env_assignment && TYPE_WORDS.contains(&&*v.to_lowercase()) {
        return true;
    }
    if code_start(v) {
        return true;
    }
    ["os.environ", "getenv", "process.env", "import.meta.env"]
        .iter()
        .any(|w| v.contains(w))
}

/// `re.match(r"[A-Za-z_][\w.]*[(\[]", v)`: a code-ish prefix ending in `(` or `[`.
fn code_start(v: &str) -> bool {
    let bytes = v.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    let mut i = 1;
    while i < bytes.len() {
        match v[i..].chars().next() {
            Some(c) if c.is_alphanumeric() || c == '_' || c == '.' => i += c.len_utf8(),
            _ => break,
        }
    }
    // The engine can only satisfy [(\[] at the end of the greedy [\w.]* run.
    matches!(v[i..].chars().next(), Some('(') | Some('['))
}

fn digit(s: &str) -> bool {
    static RE_DIGIT: LazyLock<Regex> = LazyLock::new(|| re_of(r"\d"));
    RE_DIGIT.is_match(s)
}

fn has_lower(s: &str) -> bool {
    s.is_ascii() && s.bytes().any(|b| b.is_ascii_lowercase())
        || !s.is_ascii() && s.chars().any(|c| c.is_ascii_lowercase())
}

fn has_upper(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_uppercase())
}

fn has_alpha(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_alphabetic())
}

// The skip lists, verbatim from the Python module.
fn skip_values() -> &'static std::collections::HashSet<&'static str> {
    static S: LazyLock<std::collections::HashSet<&'static str>> = LazyLock::new(|| {
        "true false yes no ok none null nil nan undefined pass passed fail failed password passwd pwd pw user username \
         xxx xxxx secret-key"
            .split_whitespace()
            .collect()
    });
    &S
}

fn stop_words() -> &'static std::collections::HashSet<&'static str> {
    static S: LazyLock<std::collections::HashSet<&'static str>> = LazyLock::new(|| {
        "a an the this that these those it its is was are be been being not no yes none null nil true false
         empty blank unset set missing required invalid wrong incorrect correct valid expired stored saved hashed encrypted salted
         plain plaintext cleartext visible hidden shown printed logged exposed leaked for in on at to of and or but if so then than
         too now here there what which who how why when where while with without from into by as your you my our their his her we
         they i he she one another same different new old default current former right okay fine good bad long short strong weak
         simple complex random generated needed asked entered typed sent read written changed reset updated rotated removed deleted
         rejected accepted ignored supplied provided given used unused only also still just very quite really actually likely
         probably maybe perhaps always never often sometimes already again yet please optional mandatory stdin env environment
         file prompt field form page screen box input output value string text example below above see shown via using through
         hard easy ready done fixed unchanged unknown unavailable disabled enabled allowed denied sufficient fed pasted cheap
         expensive secure insecure safe unsafe public private known lost forgotten reused shared temporary temp local remote locked
         unlocked pending authorised authorized accessible protected supposed meant important"
            .split_whitespace()
            .collect()
    });
    &S
}

fn unquote(v: &str) -> &str {
    let bytes = v.as_bytes();
    if v.chars().count() > 1
        && matches!(bytes.first(), Some(b'`' | b'\'' | b'"'))
        && matches!(bytes.last(), Some(b'`' | b'\'' | b'"'))
    {
        // Strip one char off each end (the value classes keep the quotes paired).
        let last = v.chars().next_back().unwrap_or(' ');
        &v[1..v.len() - last.len_utf8()]
    } else {
        v
    }
}

fn wrap(v: &str, kind: &str) -> String {
    let bytes = v.as_bytes();
    if v.chars().count() > 1
        && matches!(bytes.first(), Some(b'`' | b'\'' | b'"'))
        && matches!(bytes.last(), Some(b'`' | b'\'' | b'"'))
    {
        let first = v.chars().next().unwrap_or(' ');
        let last = v.chars().next_back().unwrap_or(' ');
        format!("{first}{}{last}", marker(kind))
    } else {
        marker(kind)
    }
}

fn prose_ok(v: &str, strong: bool) -> bool {
    let quoted = matches!(v.chars().next(), Some('`' | '\'' | '"'));
    let inner = unquote(v);
    if inner.is_empty()
        || inner.contains("REDACTED")
        || matches!(
            inner.chars().next(),
            Some('$' | '%' | '{' | '<' | '*' | '/')
        )
        || re_of(r"^[xX.\-_#*]+$").is_match(inner)
    {
        return false;
    }
    let low = inner.to_lowercase();
    if skip_values().contains(&&*low) {
        return false;
    }
    if quoted {
        return !stop_words().contains(&&*low);
    }
    if re_of(r"^[-+]?[\d.,:/%]+$").is_match(inner) && inner.chars().count() < 8 {
        return false;
    }
    if re_of(r"^[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+$").is_match(inner) || inner.contains("::") {
        return false;
    }
    if inner.chars().count() < 3 && !inner.chars().any(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let has_digit = digit(inner);
    let has_alpha = has_alpha(inner);
    let mixed_case = {
        let tail: String = inner.chars().skip(1).collect();
        tail.chars().any(|c| c.is_ascii_uppercase())
            && inner.chars().any(|c| c.is_ascii_lowercase())
    };
    if inner
        .chars()
        .any(|c| matches!(c, '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '~' | '?'))
    {
        return true;
    }
    if strong {
        if has_digit || mixed_case {
            return true;
        }
        return re_of(r"^[A-Za-z]+$").is_match(inner)
            && inner.chars().count() >= 3
            && !stop_words().contains(&&*low);
    }
    (has_digit && has_alpha && inner.chars().count() >= 6)
        || (mixed_case && inner.chars().count() >= 6)
}

fn shell_ok(v: &str, dash_ok: bool) -> bool {
    let inner = unquote(v);
    if inner.is_empty() || inner.contains("REDACTED") {
        return false;
    }
    if !v.starts_with('\'') {
        if let Some(first) = inner.chars().next() {
            if first == '<' {
                return false;
            }
            if first == '$' && !inner.starts_with("$'") {
                return false;
            }
        }
    }
    if !dash_ok && inner.starts_with('-') {
        return false;
    }
    true
}

fn is_word_dash_class(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

fn is_word_char_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn run_len(text: &str, from: usize, cls: impl Fn(u8) -> bool) -> usize {
    let b = text.as_bytes();
    let mut i = from;
    while i < b.len() && cls(b[i]) {
        i += 1;
    }
    i - from
}

fn is_blob(t: &str) -> bool {
    if digit(t) && has_alpha(t) {
        return true;
    }
    !t.contains('/')
        && t.chars().count() > 40
        && t.chars().any(|c| c.is_ascii_lowercase())
        && t.chars().any(|c| c.is_ascii_uppercase())
}

fn wordy_full(s: &str) -> bool {
    re_of(r"^(?:[A-Z]?[a-z]+){1,5}[0-9]{0,3}$").is_match(s)
}

fn flag_value_at(text: &str, pos: usize) -> Option<(usize, &'static str)> {
    // Returns (value_len, quote) for the _FLAG / _SV value alternatives.
    let b = text.as_bytes();
    let rest = &text[pos..];
    if rest.is_empty() {
        return None;
    }
    match b[pos] {
        b'"' => {
            let close = rest[1..].find('"')?;
            if rest[1..close + 1].contains('\n') {
                return None;
            }
            Some((close + 2, "\""))
        }
        b'\'' => {
            let close = rest[1..].find('\'')?;
            if rest[1..close + 1].contains('\n') {
                return None;
            }
            Some((close + 2, "'"))
        }
        c if !matches!(c, b'-' | b'$' | b'<' | b'[' | b'{' | b'(') => {
            let n = run_len(text, pos, |b| {
                !(b.is_ascii_whitespace()
                    || matches!(b, b'\'' | b'"' | b';' | b'&' | b'|' | b'<' | b'>' | b')'))
            });
            if n == 0 {
                None
            } else {
                Some((n, ""))
            }
        }
        _ => None,
    }
}

// ------------------------------------------------------------------ rule table

const _SV: &str = r##""[^"\n]*"|'[^'\n]*'|[^\s'"`;&|<>()]+"##;
const _VAL: &str = r##"(`[^`\s]{1,64}`|"[^"\s]{1,64}"|'[^'\s]{1,64}'|[^\s`'"<>(){}\[\],;]*[^\s`'"<>(){}\[\],;.:?*])"##;
const _VAL_Q: &str = r##"(`[^`\s]{1,64}`|"[^"\s]{1,64}"|'[^'\s]{1,64}'|[^\s`'"<>(){}\[\],;&]*[^\s`'"<>(){}\[\],;&.:?*])"##;
const _PW: &str = r"(?:password|passwd|passcode|passphrase|pwd|pw)";
const _USER: &str = r##"[`'"]?[\w.@\-]+[`'"]?"##;
const _PW_FOR: &str = r##"(?:[ \t]+(?:for|of)[ \t]+(?:the[ \t]+)?(?:(?:user|account|login|admin)[ \t]+)?[`'"]?[\w.@\-]+[`'"]?)?"##;
const _LOGIN: &str = r##"\blog(?:ged|ging)?[ \t-]?in[ \t]+(?:as|with|using)[ \t]+(?:user(?:name)?[ \t]+)?[`'"]?[\w.@\-]+[`'"]?"##;
const _GAP: &str = r"[^;&|\n()]{0,300}?";

fn value_rule(ok: fn(&str, bool) -> bool, arg: bool) -> Action {
    Box::new(move |c: &Captures, _t: &str| {
        let g1 = c.get(1).unwrap().as_str().to_string();
        let v = c.get(2).unwrap().as_str().to_string();
        if ok(&v, arg) {
            Outcome::Replace(format!("{g1}{}", wrap(&v, "password")))
        } else {
            Outcome::Keep
        }
    })
}

fn simple(kind: &'static str) -> Action {
    Box::new(move |_c: &Captures, _t: &str| Outcome::Replace(marker(kind)))
}

fn nb_before(c: &Captures, t: &str, is: impl Fn(u8) -> bool) -> bool {
    before_in(t, c.get(0).unwrap().start(), is)
}

fn token_excluded(t: &str, pos: usize) -> bool {
    match byte_at(t, pos) {
        Some(b) if b == b's' || b == b'S' => {
            !matches!(byte_at(t, pos + 1), Some(a) if a.is_ascii_alphabetic())
        }
        Some(b) if b == b'i' || b == b'I' => matches!(byte_at(t, pos + 1), Some(b'z' | b'Z')),
        _ => false,
    }
}

fn flag_core_excluded(t: &str, pos: usize) -> bool {
    if byte_at(t, pos) != Some(b'-') {
        return false;
    }
    let rest = &t[pos + 1..];
    [
        "stdin", "file", "env", "prompt", "cmd", "command", "helper", "store", "path", "dir",
        "length", "policy",
    ]
    .iter()
    .any(|w| {
        rest.len() >= w.len()
            && rest[..w.len()].eq_ignore_ascii_case(w)
            && !matches!(byte_at(t, pos + 1 + w.len()), Some(x) if x.is_ascii_alphanumeric())
    })
}

fn eq_assignment(g1: &str) -> bool {
    let b = g1.as_bytes();
    b.last() == Some(&b'=')
        && b.len() >= 2
        && !matches!(
            b[b.len() - 2],
            b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | b'"' | b'\'' | b'\\'
        )
}

const FLAG_ALTS: [&str; 6] = ["password", "passwd", "secret", "api-key", "apike", "token"];

fn alt_len_ci(t: &str, pos: usize, alt: &str) -> Option<usize> {
    if t.len() >= pos + alt.len() && t[pos..pos + alt.len()].eq_ignore_ascii_case(alt) {
        Some(alt.len())
    } else {
        None
    }
}

/// Hand-written scan for the `secret-flag` rule (`--token VALUE` and friends).
/// The Python regex backtracks `--?[A-Za-z0-9\-]{0,40}(key)` across every
/// split of the flag prefix; a single regex match cannot see the earlier key
/// positions after the rightmost one is refused by its lookaheads, so the
/// engine's order (dashes greedy, prefix longest-first, then alternatives) is
/// reproduced here.
fn sub_flag(text: &str, counts: &mut HashMap<&'static str, u64>) -> String {
    let mut out = String::new();
    let b = text.as_bytes();
    let mut pos = 0usize;
    while pos < b.len() {
        if b[pos] != b'-' {
            match text[pos..].find('-') {
                Some(off) => {
                    out.push_str(&text[pos..pos + off]);
                    pos += off;
                }
                None => {
                    out.push_str(&text[pos..]);
                    pos = b.len();
                }
            }
            continue;
        }
        let s = pos;
        if before_in(text, s, is_alnum) {
            out.push('-');
            pos += 1;
            continue;
        }
        let mut done = false;
        'cands: for &dl in &[2usize, 1] {
            if dl == 2 && b.get(s + 1) != Some(&b'-') {
                continue;
            }
            let pstart = s + dl;
            let prank = run_len(text, pstart, is_word_dash_class).min(40);
            for pl in (0..=prank).rev() {
                let cpos = pstart + pl;
                for alt in FLAG_ALTS {
                    let Some(alen) = alt_len_ci(text, cpos, alt) else {
                        continue;
                    };
                    let cend = cpos + alen;
                    if alt == "token" && token_excluded(text, cend) {
                        continue;
                    }
                    if flag_core_excluded(text, cend) {
                        continue;
                    }
                    let r2 = run_len(text, cend, is_word_dash_class);
                    if r2 > 40 {
                        continue;
                    }
                    let send = cend + r2;
                    if !matches!(byte_at(text, send), Some(x) if is_ws(x)) {
                        continue;
                    }
                    let wsend = send + run_len(text, send, is_ws);
                    let Some((vlen, quote)) = flag_value_at(text, wsend) else {
                        continue;
                    };
                    let vend = wsend + vlen;
                    let g1 = &text[s..send];
                    let g2 = &text[send..wsend];
                    let v = &text[wsend..vend];
                    let inner = if quote.is_empty() {
                        v
                    } else {
                        &v[1..v.len() - 1]
                    };
                    let repl =
                        if looks_like_code_or_placeholder(inner, false) || inner.starts_with('-') {
                            None
                        } else {
                            Some(format!("{g1}{g2}{quote}{}{quote}", marker("secret")))
                        };
                    match repl {
                        Some(repl) => {
                            let added = count_marks(&repl) - count_marks(&text[s..vend]);
                            out.push_str(&text[pos..s]);
                            out.push_str(&repl);
                            if added > 0 {
                                *counts.entry("secret").or_default() += added as u64;
                            }
                        }
                        None => {
                            out.push_str(&text[pos..vend]);
                        }
                    }
                    pos = vend;
                    done = true;
                    break 'cands;
                }
            }
        }
        if !done {
            out.push('-');
            pos += 1;
        }
    }
    out
}

fn build_rules() -> Vec<Rule> {
    fn kv_re() -> Regex {
        re_of(
            r#"(?i)(password|passwd|passphrase|passcode|secret|api[_\-]?key|apikey|access[_\-]?key|private[_\-]?key|token)([A-Za-z0-9_.\-]{0,64}\\?[\"']?\s*[=:]\s*)(\\\"[^\"\\\n]*\\\"|\"[^"\n]*\"|'[^'\n]*'|[^\s'"&;|,)}\]\\]+)"#,
        )
    }
    fn env_re() -> Regex {
        re_of(
            &[
                r"((?:(?:[A-Za-z0-9]{1,30}[_.\-]){0,6}(?i:pass|passwd|passcode|pw)[0-9]{0,2}",
                r"|(?:[A-Za-z0-9]{1,30}[_.\-]){1,6}(?i:pwd)[0-9]{0,2}",
                r"|(?i:[A-Za-z]{1,20}(?:pass|passwd|pwd|pw)))(?:[_.\-][A-Za-z0-9]{1,30}){0,4}=)(",
                _SV,
                ")",
            ]
            .concat(),
        )
    }
    static ENV_B1: LazyLock<Regex> = LazyLock::new(|| {
        re_of(
            r"^(?i:(?:[A-Za-z0-9]{1,30}[_.\-]){0,6}(?:pass|passwd|passcode|pw)[0-9]{0,2}(?:[_.\-][A-Za-z0-9]{1,30}){0,4})=$",
        )
    });
    static ENV_B2: LazyLock<Regex> = LazyLock::new(|| {
        re_of(
            r"^(?i:(?:[A-Za-z0-9]{1,30}[_.\-]){1,6}pwd[0-9]{0,2}(?:[_.\-][A-Za-z0-9]{1,30}){0,4})=$",
        )
    });
    static BEARER_SYM: LazyLock<Regex> = LazyLock::new(|| re_of(r"[._~+/=\-]"));
    vec![
        Rule { id: "pem", kind: "pem", re: re_of(r"(?s)-----BEGIN [A-Z0-9 ]+-----.*?(?:-----END [A-Z0-9 ]+-----|$)"), action: simple("pem") },
        Rule {
            id: "url-credential",
            kind: "url-password",
            re: re_of(r##"://([^/\s:@'"]+):([^@\s/'"]+)@"##),
            action: Box::new(|c: &Captures, _t: &str| {
                let g1 = c.get(1).unwrap().as_str();
                let pass = c.get(2).unwrap().as_str();
                if starts_with_any(pass, &["$", "{", "<", "%", MARK]) {
                    Outcome::Keep
                } else {
                    Outcome::Replace(format!("://{g1}:{}@", marker("url-password")))
                }
            }),
        },
        Rule { id: "github-token", kind: "github-token", re: re_of(r"(?:github_pat_[A-Za-z0-9_]{6,}|gh[pousr]_[A-Za-z0-9]{6,})"), action: Box::new(|c: &Captures, t: &str| {
            if nb_before(c, t, is_alnum) { Outcome::NoMatch } else { Outcome::Replace(marker("github-token")) }
        }) },
        Rule { id: "hf-token", kind: "hf-token", re: re_of(r"hf_([A-Za-z0-9]{6,})"), action: Box::new(|c: &Captures, t: &str| {
            if nb_before(c, t, is_alnum) {
                return Outcome::NoMatch;
            }
            let run = c.get(1).unwrap().as_str();
            if run.len() >= 20 || run.bytes().any(|b| b.is_ascii_digit()) {
                Outcome::Replace(marker("hf-token"))
            } else {
                Outcome::NoMatch
            }
        }) },
        Rule { id: "sk-key", kind: "api-key", re: re_of(r"sk-([A-Za-z0-9_\-]{6,})"), action: Box::new(|c: &Captures, t: &str| {
            if nb_before(c, t, is_alnum) {
                return Outcome::NoMatch;
            }
            let run = c.get(1).unwrap().as_str();
            if run.len() >= 20 || run.bytes().any(|b| b.is_ascii_digit()) {
                Outcome::Replace(marker("api-key"))
            } else {
                Outcome::NoMatch
            }
        }) },
        Rule { id: "aws-key", kind: "aws-key", re: re_of(r"(?:AKIA[A-Z0-9]{8,}|ASIA[A-Z0-9]{16})"), action: Box::new(|c: &Captures, t: &str| {
            if nb_before(c, t, is_alnum) { Outcome::NoMatch } else { Outcome::Replace(marker("aws-key")) }
        }) },
        Rule { id: "slack-token", kind: "slack-token", re: re_of(r"xox[A-Za-z]?-[A-Za-z0-9\-]{4,}"), action: Box::new(|c: &Captures, t: &str| {
            if nb_before(c, t, is_alnum) { Outcome::NoMatch } else { Outcome::Replace(marker("slack-token")) }
        }) },
        Rule { id: "google-key", kind: "google-key", re: re_of(r"AIza[0-9A-Za-z_\-]{35}"), action: Box::new(|c: &Captures, t: &str| {
            if nb_before(c, t, is_alnum) { Outcome::NoMatch } else { Outcome::Replace(marker("google-key")) }
        }) },
        Rule { id: "jwt", kind: "jwt", re: re_of(r"eyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"), action: Box::new(|c: &Captures, t: &str| {
            if nb_before(c, t, is_alnum) { Outcome::NoMatch } else { Outcome::Replace(marker("jwt")) }
        }) },
        Rule {
            id: "auth-header",
            kind: "auth",
            re: re_of(r##"(?i)\b(authorization["']?\s*[=:]\s*["']?)(basic|digest|token)(\s+)([^\s'"]{8,})"##),
            action: Box::new(|c: &Captures, _t: &str| {
                let v = c.get(4).unwrap().as_str();
                if starts_with_any(v, NOT_A_VALUE_START) {
                    Outcome::Keep
                } else {
                    Outcome::Replace(format!(
                        "{}{}{}{}",
                        c.get(1).unwrap().as_str(),
                        c.get(2).unwrap().as_str(),
                        c.get(3).unwrap().as_str(),
                        marker("auth")
                    ))
                }
            }),
        },
        Rule {
            id: "bearer",
            kind: "bearer",
            re: re_of(r"(?i)\b(bearer)(\s+)([A-Za-z0-9._~+/=\-]{8,})"),
            action: Box::new(|c: &Captures, _t: &str| {
                let g1 = c.get(1).unwrap().as_str();
                let g2 = c.get(2).unwrap().as_str();
                let v = c.get(3).unwrap().as_str();
                if digit(v) || v.chars().count() >= 20 || has(&BEARER_SYM, v) {
                    Outcome::Replace(format!("{g1}{g2}{}", marker("bearer")))
                } else {
                    Outcome::Keep
                }
            }),
        },
        Rule {
            id: "key-value",
            kind: "secret",
            re: kv_re(),
            action: Box::new(|c: &Captures, t: &str| {
                let (s1, e1) = c.get(1).map(|m| (m.start(), m.end())).unwrap();
                let e2 = c.get(2).map(|m| m.end()).unwrap();
                let (s3, e3) = c.get(3).map(|m| (m.start(), m.end())).unwrap();
                if c.get(1).unwrap().as_str().eq_ignore_ascii_case("token") && token_excluded(t, e1) {
                    return Outcome::NoMatch;
                }
                if byte_at(t, e2) == Some(b'=') {
                    return Outcome::NoMatch;
                }
                let g1 = &t[s1..e2];
                let v = &t[s3..e3];
                let (q, inner): (&str, &str) = if v.starts_with("\\\"") {
                    ("\\\"", &v[2..v.len() - 2])
                } else if v.starts_with('"') || v.starts_with('\'') {
                    (&v[..1], &v[1..v.len() - 1])
                } else {
                    ("", v)
                };
                if looks_like_code_or_placeholder(inner, eq_assignment(g1)) {
                    return Outcome::Keep;
                }
                Outcome::Replace(format!("{g1}{q}{}{q}", marker("secret")))
            }),
        },
        // Applied by sub_flag (dispatch in redact); the pattern never runs.
        Rule { id: "secret-flag", kind: "secret", re: re_of(r"$^"), action: Box::new(|_c: &Captures, _t: &str| Outcome::NoMatch) },
        Rule {
            id: "env-password",
            kind: "password",
            re: env_re(),
            action: Box::new(|c: &Captures, t: &str| {
                if nb_before(c, t, |b| b.is_ascii_alphanumeric() || matches!(b, b'$' | b'/')) {
                    return Outcome::NoMatch;
                }
                let g1 = c.get(1).unwrap().as_str();
                if !ENV_B1.is_match(g1) && !ENV_B2.is_match(g1) {
                    let low = g1.to_lowercase();
                    for w in ["compass", "bypass", "trespass", "surpass", "overpass", "underpass"] {
                        if let Some(rest) = low.strip_prefix(w) {
                            if !matches!(rest.chars().next(), Some(ch) if ch.is_ascii_alphabetic()) {
                                return Outcome::NoMatch;
                            }
                        }
                    }
                }
                let v = c.get(2).unwrap().as_str();
                if shell_ok(v, true) {
                    Outcome::Replace(format!("{}{}", g1, wrap(v, "password")))
                } else {
                    Outcome::Keep
                }
            }),
        },
        Rule { id: "pass-flag", kind: "password", re: re_of(&[r"(?i)(--(?:pass|pw|pwd)(?:=|[ \t]+))(", _SV, ")"].concat()), action: Box::new(|c: &Captures, t: &str| {
            if nb_before(c, t, |b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) {
                return Outcome::NoMatch; // (?<![A-Za-z0-9_\-])
            }
            let g1 = c.get(1).unwrap().as_str().to_string();
            let v = c.get(2).unwrap().as_str().to_string();
            if shell_ok(&v, false) {
                Outcome::Replace(format!("{g1}{}", wrap(&v, "password")))
            } else {
                Outcome::Keep
            }
        }) },
        Rule { id: "sshpass", kind: "password", re: re_of(&[r"(\bsshpass[ \t]+(?:(?:-[ev]|-[fdP][ \t]*\S+)[ \t]+)*-p[ \t]*)(", _SV, ")"].concat()), action: value_rule(shell_ok, true) },
        Rule {
            id: "mysql-password",
            kind: "password",
            re: re_of(
                &[
                    r"(\b(?:mysql|mysqldump|mysqladmin|mysqlimport|mysqlshow|mysqlcheck|mysqlpump|mysqlbinlog|mariadb|mariadb-dump|mariadb-admin|mariadb-import|mariadb-show|mariadb-check)\b",
                    _GAP,
                    r"[ \t]-p[ \t]*)(",
                    _SV,
                    ")",
                ]
                .concat(),
            ),
            action: Box::new(|c: &Captures, t: &str| {
                let e1 = c.get(1).unwrap().end();
                let b = t.as_bytes();
                let mut p = e1;
                while p > 0 && matches!(b[p - 1], b' ' | b'\t') {
                    p -= 1;
                }
                for w in ["rint", "rune", "erm"] {
                    if t[p..].starts_with(w) && !matches!(byte_at(t, p + w.len()), Some(x) if is_word_char_byte(x)) {
                        return Outcome::NoMatch;
                    }
                }
                let g1 = c.get(1).unwrap().as_str().to_string();
                let v = c.get(2).unwrap().as_str().to_string();
                if shell_ok(&v, false) {
                    Outcome::Replace(format!("{g1}{}", wrap(&v, "password")))
                } else {
                    Outcome::Keep
                }
            }),
        },
        Rule {
            id: "tool-password",
            kind: "password",
            re: re_of(
                &[
                    r"((?:\bdocker[ \t]+login\b",
                    _GAP,
                    r"[ \t]-p[ \t]*|\bredis-cli\b",
                    _GAP,
                    r"[ \t]-a[ \t]+|\bldap(?:search|modify|add|delete|passwd|compare|whoami|modrdn)\b",
                    _GAP,
                    r"[ \t]-w[ \t]*|-pass(?:in|out)?[ \t]+pass:))(",
                    _SV,
                    ")",
                ]
                .concat(),
            ),
            action: value_rule(shell_ok, false),
        },
        Rule {
            id: "curl-user",
            kind: "password",
            re: re_of(r##"(\bcurl\b[^;&|\n()]{0,300}?[ \t](?:-[A-Za-z]{0,10}u|--user)(?:[ \t]+|=)?["']?[^\s:'"]+:)([^\s'";&|]+)"##),
            action: value_rule(shell_ok, true),
        },
        Rule {
            id: "prose-pw-kv",
            kind: "password",
            re: re_of(&[r##"(?i)((?:[A-Za-z0-9]{1,30}[_\-]){0,6}(?:(?-i:pass|Pass)|pw|pwd)[`'"]?[ \t]*(?::=|[:=])[ \t]*)("##, _VAL_Q, ")"].concat()),
            action: Box::new(|c: &Captures, t: &str| {
                if nb_before(c, t, |b| b.is_ascii_alphanumeric() || b == b'$') {
                    return Outcome::NoMatch;
                }
                let g1 = c.get(1).unwrap().as_str().to_string();
                let v = c.get(2).unwrap().as_str().to_string();
                if prose_ok(&v, false) {
                    Outcome::Replace(format!("{g1}{}", wrap(&v, "password")))
                } else {
                    Outcome::Keep
                }
            }),
        },
        Rule { id: "prose-is", kind: "password", re: re_of(&[r"(?i)\b(", _PW, _PW_FOR, r"[ \t]+(?:is|was|are)\b[ \t]*:?[ \t]*)(", _VAL, ")"].concat()), action: value_rule(prose_ok, true) },
        Rule { id: "prose-set-to", kind: "password", re: re_of(&[r"(?i)\b(", _PW, _PW_FOR, r"[ \t]+(?:(?:is|was|has been|will be|should be)[ \t]+)?(?:set|changed|reset|updated|rotated)?[ \t]*(?:to|->|→)[ \t]*)(", _VAL, ")"].concat()), action: value_rule(prose_ok, false) },
        Rule { id: "prose-login-sep", kind: "password", re: re_of(&[r"(?i)(", _LOGIN, r"[ \t]*[/:,][ \t]*)(", _VAL, ")"].concat()), action: value_rule(prose_ok, false) },
        Rule { id: "prose-login-pw", kind: "password", re: re_of(&[r"(?i)(", _LOGIN, r"[,;]?[ \t]+(?:(?:with|and)[ \t]+)?(?:the[ \t]+)?", _PW, r"[ \t]*(?:is|=|:)?[ \t]*)(", _VAL, ")"].concat()), action: value_rule(prose_ok, true) },
        Rule { id: "prose-user-pw", kind: "password", re: re_of(&[r"(?i)\b((?:user(?:name)?|login|account|acct|id)[ \t]*[:=]?[ \t]*(?:is[ \t]+)?", _USER, r"[,;]?[ \t]+(?:(?:and|with)[ \t]+)?", _PW, r"[ \t]*[:=]?[ \t]*)(", _VAL, ")"].concat()), action: value_rule(prose_ok, false) },
        Rule { id: "prose-creds", kind: "password", re: re_of(&[r"(?i)\b((?:credentials?|creds?)\b[ \t]*(?:(?:are|is)[ \t]*)?[:=]?[ \t]*", _USER, r"[ \t]*[:/][ \t]*)(", _VAL, ")"].concat()), action: value_rule(prose_ok, false) },
        Rule {
            id: "email",
            kind: "email",
            re: re_of(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)*\.[A-Za-z]{2,}"),
            action: Box::new(|c: &Captures, t: &str| {
                if nb_before(c, t, |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-')) {
                    return Outcome::NoMatch;
                }
                let e = c.get(0).unwrap().end();
                if after_in(t, e, |b| b.is_ascii_alphanumeric() || b == b'-') {
                    return Outcome::NoMatch;
                }
                if byte_at(t, e) == Some(b':')
                    && matches!(byte_at(t, e + 1), Some(x) if x.is_ascii_alphanumeric() || matches!(x, b'_' | b'~' | b'/' | b'.' | b'-'))
                {
                    return Outcome::NoMatch;
                }
                Outcome::Replace(marker("email"))
            }),
        },
        Rule { id: "hex-run", kind: "hex", re: re_of(r"[A-Fa-f0-9]{33,}"), action: Box::new(|c: &Captures, t: &str| {
            if nb_before(c, t, is_alnum) || after_in(t, c.get(0).unwrap().end(), is_alnum) {
                Outcome::NoMatch
            } else {
                Outcome::Replace(marker("hex"))
            }
        }) },
        Rule {
            id: "base64-run",
            kind: "base64",
            re: re_of(r"[A-Za-z0-9+/]{33,}={0,2}"),
            action: Box::new(|c: &Captures, t: &str| {
                let (s, e) = c.get(0).map(|m| (m.start(), m.end())).unwrap();
                if before_in(t, s, is_alnum) {
                    return Outcome::NoMatch;
                }
                let m = &t[s..e];
                let eq = m.as_bytes().iter().rev().take_while(|&&b| b == b'=').count().min(2);
                let base_end = e - eq;
                let mut end = None;
                for k in (0..=eq).rev() {
                    if !after_in(t, base_end + k, is_alnum) {
                        end = Some(base_end + k);
                        break;
                    }
                }
                let Some(end) = end else { return Outcome::NoMatch };
                let tt = &t[s..end];
                if tt.contains('/') {
                    let segs: Vec<&str> = tt.split('/').filter(|x: &&str| !x.is_empty()).collect();
                    let wordy = segs.iter().filter(|x| wordy_full(x)).count();
                    if segs.len() >= 3 && wordy * 2 >= segs.len() {
                        let parts: Vec<String> = tt
                            .split('/')
                            .map(|x: &str| if x.chars().count() > 32 && is_blob(x) { marker("base64") } else { x.to_string() })
                            .collect();
                        let r = parts.join("/");
                        return if r == tt {
                            Outcome::Keep
                        } else {
                            Outcome::ReplaceRegion(s, end, r)
                        };
                    }
                }
                if is_blob(tt) {
                    Outcome::ReplaceRegion(s, end, marker("base64"))
                } else {
                    Outcome::Keep
                }
            }),
        },
        Rule {
            id: "base64url-run",
            kind: "base64",
            re: re_of(r"[A-Za-z0-9_\-]{40,}"),
            action: Box::new(|c: &Captures, t: &str| {
                if nb_before(c, t, is_alnum) || after_in(t, c.get(0).unwrap().end(), is_alnum) {
                    return Outcome::NoMatch;
                }
                let tt = c.get(0).unwrap().as_str();
                if !tt.bytes().any(|b| b == b'-' || b == b'_') {
                    return Outcome::NoMatch;
                }
                if digit(tt) && has_lower(tt) && has_upper(tt) {
                    Outcome::Replace(marker("base64"))
                } else {
                    Outcome::Keep
                }
            }),
        },
        Rule {
            id: "mixed-token",
            kind: "token",
            re: re_of(r"[A-Za-z0-9]{20,32}"),
            action: Box::new(|c: &Captures, t: &str| {
                if nb_before(c, t, is_alnum) || after_in(t, c.get(0).unwrap().end(), is_alnum) {
                    return Outcome::NoMatch;
                }
                let tt = c.get(0).unwrap().as_str();
                if tt.bytes().any(|b| b.is_ascii_lowercase())
                    && tt.bytes().any(|b| b.is_ascii_uppercase())
                    && tt.bytes().any(|b| b.is_ascii_digit())
                {
                    Outcome::Replace(marker("token"))
                } else {
                    Outcome::NoMatch
                }
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_ok_and_dash_guards() {
        assert!(shell_ok("hunter2", false));
        assert!(!shell_ok("-x", false)); // --pass --help style flags survive
        assert!(shell_ok("-x", true)); // mysql -p -pxy passes like Python's _shell_dash
        assert!(shell_ok("-x-y", true));
        assert!(!shell_ok("", false)); // empty after unquoting is not a secret
    }

    #[test]
    fn b64url_class_extension_replicates_the_lookahead() {
        // (?![A-Za-z0-9]) after '=': '=' itself ends the token; alnum after it does not.
        let b = |c: char| c.is_ascii_alphanumeric();
        assert!(!b('='));
        assert!(b('a'));
        // The candidate run may end in '=' (up to two) and still be a token.
        assert_eq!(count_marks("[REDACTED:x][REDACTED:y]"), 2);
        assert_eq!(count_marks("[REDACTED:secret]"), 1);
    }

    #[test]
    fn token_exclusion_matches_the_negative_lookahead() {
        assert!(token_excluded("tokens=x", 5)); // tokens= stays
        assert!(token_excluded("token;s", 6)); // 's' at a token boundary excludes
        assert!(token_excluded("tokenize", 5)); // tokenize stays
        assert!(!token_excluded("token=1", 5)); // no s/i continuation: masks
        assert!(!token_excluded("tokenx", 5)); // tokenx masks
        assert!(!token_excluded("token", 5)); // token= masks
    }
}
