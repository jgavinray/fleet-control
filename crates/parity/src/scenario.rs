//! The mock-engine fleet from `crates/parity.py`'s `scen()`, ported verbatim:
//! one scenario per engine, each with its registry fields and its HTTP
//! behaviours. The registry is written to a temp file at run time because the
//! mock ports are ephemeral.

use serde_json::{json, Value};

use crate::mock::{ok_chat, Chat, Completions, Predicate, Reply};

pub const MODEL: &str = "M-1";
pub const SANITY_PROMPT: &str = "What is 17 plus 25? Reply with the number only.";

/// A TOML scalar, limited to what the scenarios write: strings, booleans and
/// integers (`expected_features` may pin a raw number).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    Str(&'static str),
    Bool(bool),
    Int(i64),
}

/// Registry fields beyond the defaults (`model`, `host`, `kind`, `url`).
#[derive(Clone, Copy, Default)]
pub struct Overrides {
    pub kind: Option<&'static str>,
    pub model: Option<&'static str>,
    pub shared: Option<Scalar>,
    pub health_path: Option<&'static str>,
    pub read: Option<&'static str>,
    pub exporter_self: bool,
    pub expected: &'static [(&'static str, Scalar)],
}

#[derive(Clone)]
pub struct Scenario {
    pub name: &'static str,
    pub overrides: Overrides,
    /// The port is held bound but never listened on.
    pub refused: bool,
    pub root: Option<Reply>,
    pub health: Option<Reply>,
    pub models: Option<Reply>,
    pub opinion_specs: Option<Reply>,
    pub ready: Option<Reply>,
    pub healthz: Option<Reply>,
    pub chat: Option<Chat>,
    /// A chat body that is not valid UTF-8, sent verbatim.
    pub chat_raw: Option<&'static [u8]>,
    pub completions: Option<Completions>,
    pub systemone: Option<Reply>,
    pub spans: Option<Reply>,
}

fn new(name: &'static str) -> Scenario {
    Scenario {
        name,
        overrides: Overrides::default(),
        refused: false,
        root: None,
        health: None,
        models: None,
        opinion_specs: None,
        ready: None,
        healthz: None,
        chat: None,
        chat_raw: None,
        completions: None,
        systemone: None,
        spans: None,
    }
}

impl Scenario {
    fn overrides(mut self, overrides: Overrides) -> Self {
        self.overrides = overrides;
        self
    }

    fn refused(mut self) -> Self {
        self.refused = true;
        self
    }

    fn root(mut self, status: u16, body: Value) -> Self {
        self.root = Some(Reply::json(status, &body.to_string()));
        self
    }

    fn health(mut self, status: u16, body: Value) -> Self {
        self.health = Some(Reply::json(status, &body.to_string()));
        self
    }

    /// A health body carrying a bare `NaN` / `-Infinity` token.
    fn health_json(mut self, status: u16, body: &'static str) -> Self {
        self.health = Some(Reply::json(status, body));
        self
    }

    fn models(mut self, status: u16, body: Value) -> Self {
        self.models = Some(Reply::json(status, &body.to_string()));
        self
    }

    fn opinion_specs(mut self, status: u16, body: Value) -> Self {
        self.opinion_specs = Some(Reply::json(status, &body.to_string()));
        self
    }

    fn ready(mut self, status: u16, body: Value) -> Self {
        self.ready = Some(Reply::json(status, &body.to_string()));
        self
    }

    fn systemone(mut self, status: u16, body: Value) -> Self {
        self.systemone = Some(Reply::json(status, &body.to_string()));
        self
    }

    /// A `/v1/systemone` body carrying a bare `NaN` / `-Infinity` token.
    fn systemone_json(mut self, status: u16, body: &'static str) -> Self {
        self.systemone = Some(Reply::json(status, body));
        self
    }

    fn spans(mut self, status: u16, body: Value) -> Self {
        self.spans = Some(Reply::json(status, &body.to_string()));
        self
    }

    fn healthz(mut self, status: u16, body: &'static str) -> Self {
        self.healthz = Some(Reply::text(status, body));
        self
    }

    fn chat(mut self, chat: Chat) -> Self {
        self.chat = Some(chat);
        self
    }

    fn chat_raw(mut self, bytes: &'static [u8]) -> Self {
        self.chat_raw = Some(bytes);
        self
    }

    fn completions(mut self, completions: Completions) -> Self {
        self.completions = Some(completions);
        self
    }
}

fn fixed(reply: Reply) -> Chat {
    Chat::Fixed(reply)
}

/// `lambda b: ... if predicate else ideal_chat(b)`.
fn when(predicate: Predicate, then: Reply) -> Chat {
    Chat::When {
        predicate,
        then,
        otherwise: Box::new(Chat::Ideal),
    }
}

/// `lambda b: ... if predicate else (200, OK_CHAT)`.
fn when_else(predicate: Predicate, then: Reply, otherwise: Reply) -> Chat {
    Chat::When {
        predicate,
        then,
        otherwise: Box::new(Chat::Fixed(otherwise)),
    }
}

fn chat_text(text: &str) -> Reply {
    Reply::json(
        200,
        &json!({"choices": [{"message": {"content": text}}]}).to_string(),
    )
}

/// `def scen()`: name -> scenario, in parity.py's insertion order.
pub fn scenarios() -> Vec<Scenario> {
    let mut all: Vec<Scenario> = Vec::new();

    // --- the healthy/ill TensorFold engines (quick and full) ---
    all.push(
        new("ok_ids_in_health")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("ok_status_shape")
            .health(200, json!({"status": "ok"}))
            .models(200, json!({"data": [{"id": MODEL}]}))
            .chat(fixed(chat_text("The answer is 42."))),
    );
    all.push(
        new("busy_load")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 2, "requests_waiting": 1}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("live_shape")
            .health(
                200,
                json!({"ok": true, "model_ids": [MODEL], "live": {"connections": 3, "waiting": 1}}),
            )
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("wrong_model")
            .health(200, json!({"ok": true, "model_ids": ["Other"], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("not_ok")
            .health(200, json!({"ok": false}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("warming")
            .health(
                200,
                json!({"ok": true, "warming": true, "model_ids": [MODEL]}),
            )
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("health_500")
            .health(500, json!({"error": "boom"}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("models_404")
            .health(200, json!({"ok": true}))
            .models(404, json!({"error": "no"}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("models_no_ids")
            .health(200, json!({"ok": true}))
            .models(200, json!({"x": 1}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("sanity_wrong")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(chat_text("41"))),
    );
    all.push(
        new("sanity_null")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(Reply::json(200, r#"{"choices": [{"message": {"content": null}}]}"#))),
    );
    all.push(
        new("sanity_500")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(Reply::json(500, r#"{"error": "x"}"#))),
    );
    all.push(
        new("sanity_404")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(Reply::json(404, r#"{"error": "x"}"#))),
    );
    all.push(
        new("bg_accepted")
            .overrides(Overrides {
                expected: &[("background_priority", Scalar::Str("yes"))],
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("bg_refused")
            .overrides(Overrides {
                expected: &[("background_priority", Scalar::Str("yes"))],
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(when_else(
                Predicate::PriorityPresent,
                Reply::json(400, r#"{"error": "priority not supported"}"#),
                ok_chat(),
            )),
    );
    all.push(
        new("shared_busy")
            .overrides(Overrides {
                shared: Some(Scalar::Bool(true)),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 2, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("shared_idle")
            .overrides(Overrides {
                shared: Some(Scalar::Bool(true)),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("unknown_kind")
            .overrides(Overrides {
                kind: Some("mystery"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(new("refused").refused());

    // messy engine names: leading digit, upper vs lower case ordering, dash and underscore
    for name in ["9lives", "Zeta", "alpha", "a-b_c"] {
        all.push(
            new(name)
                .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
                .chat(fixed(ok_chat())),
        );
    }

    // --- tensorfold full mode: the probes run ---
    all.push(
        new("guided_yes")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(when(Predicate::GuidedChoice, chat_text("apple"))),
    );
    all.push(
        new("guided_refused")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(when(
                Predicate::GuidedChoice,
                Reply::json(400, r#"{"error": "guided_choice not supported"}"#),
            )),
    );
    all.push(
        new("guided_null")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(when(
                Predicate::GuidedChoice,
                Reply::json(200, r#"{"choices": [{"message": {"content": null}}]}"#),
            )),
    );
    all.push(
        new("guided_offlist")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(when(Predicate::GuidedChoice, chat_text("granite and apple"))),
    );
    all.push(
        new("logprobs_yes")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(Chat::Ideal),
    );
    all.push(
        new("logprobs_absent")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(when(Predicate::LogprobsTruthy, chat_text("ok"))),
    );
    all.push(
        new("plp_yes")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(Chat::Ideal)
            .completions(Completions::IdealPlp),
    );
    all.push(
        new("plp_absent")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(Chat::Ideal)
            .completions(Completions::Fixed(Reply::json(
                200,
                r#"{"choices": [], "id": "cmpl"}"#,
            ))),
    );
    all.push(
        new("bg_refused_sanity_yes")
            .overrides(Overrides {
                expected: &[("background_priority", Scalar::Str("yes"))],
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(when(
                Predicate::PriorityBackground,
                Reply::json(400, r#"{"error": "priority not supported"}"#),
            ))
            .completions(Completions::IdealPlp),
    );
    all.push(
        new("bg_refused_sanity_no")
            .overrides(Overrides {
                expected: &[("background_priority", Scalar::Str("no"))],
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(when(
                Predicate::PriorityBackground,
                Reply::json(400, r#"{"error": "priority not supported"}"#),
            ))
            .completions(Completions::IdealPlp),
    );
    all.push(
        new("lp_drift")
            .overrides(Overrides {
                expected: &[("logprobs", Scalar::Str("yes"))],
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(when(Predicate::LogprobsTruthy, chat_text("ok")))
            .completions(Completions::IdealPlp),
    );

    // --- the other kinds (full mode only; --quick does not touch them) ---
    all.push(
        new("laya_ok")
            .overrides(Overrides {
                kind: Some("laya"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "loaded": ["en"], "device": "cuda:0"}))
            .systemone(200, json!({"answers": {"intent": {"choice": "billing", "probabilities": {"billing": 0.87}}}, "model": MODEL})),
    );
    all.push(
        new("laya_wrong_choice")
            .overrides(Overrides {
                kind: Some("laya"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "loaded": [], "device": "cpu"}))
            .systemone(
                200,
                json!({"answers": {"intent": {"choice": "shipping"}}, "model": MODEL}),
            ),
    );
    all.push(
        new("laya_model_mismatch")
            .overrides(Overrides {
                kind: Some("laya"),
                ..Overrides::default()
            })
            .health(
                200,
                json!({"ok": true, "loaded": ["en"], "device": "cuda:0"}),
            )
            .systemone(
                200,
                json!({"answers": {"intent": {"choice": "billing"}}, "model": "Other"}),
            ),
    );
    all.push(
        new("laya_health_bad")
            .overrides(Overrides {
                kind: Some("laya"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": false})),
    );
    all.push(
        new("lfm2d_specs_ok")
            .overrides(Overrides {
                kind: Some("lfm2d"),
                ..Overrides::default()
            })
            .models(200, json!([{"id": MODEL}]))
            .opinion_specs(200, json!([{"id": "s1", "name": "bash-risk"}])),
    );
    all.push(
        new("lfm2d_wrong_model")
            .overrides(Overrides {
                kind: Some("lfm2d"),
                ..Overrides::default()
            })
            .models(200, json!([{"id": "Other"}])),
    );
    all.push(
        new("lfm2d_specs_empty")
            .overrides(Overrides {
                kind: Some("lfm2d"),
                ..Overrides::default()
            })
            .models(200, json!([{"id": MODEL}]))
            .opinion_specs(200, json!([])),
    );
    all.push(
        new("lfm2d_spans_ok")
            .overrides(Overrides {
                kind: Some("lfm2d"),
                read: Some("spans"),
                ..Overrides::default()
            })
            .models(200, json!([{"id": MODEL}]))
            .spans(200, json!([[{"text": "sk-1", "start": 0, "end": 5}]])),
    );
    all.push(
        new("lfm2d_exporter_ok")
            .overrides(Overrides {
                kind: Some("lfm2d"),
                exporter_self: true,
                ..Overrides::default()
            })
            .models(200, json!([{"id": MODEL}]))
            .opinion_specs(200, json!([{"id": "s1"}]))
            .healthz(200, "ok"),
    );
    all.push(
        new("lfm2d_exporter_bad")
            .overrides(Overrides {
                kind: Some("lfm2d"),
                exporter_self: true,
                ..Overrides::default()
            })
            .models(200, json!([{"id": MODEL}]))
            .opinion_specs(200, json!([{"id": "s1"}]))
            .healthz(200, "starting"),
    );
    all.push(
        new("jev_ok")
            .overrides(Overrides {
                kind: Some("jev-glm"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model": MODEL}))
            .ready(200, json!({"status": "ok"}))
            .systemone(200, json!({"answers": {"verdict": {"choice": "read", "probabilities": {"read": 0.9}}}, "model": MODEL})),
    );
    all.push(
        new("jev_ready_fail")
            .overrides(Overrides {
                kind: Some("jev-glm"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model": MODEL}))
            .ready(500, json!({"error": "glm is down"})),
    );
    all.push(
        new("jev_wrong_choice")
            .overrides(Overrides {
                kind: Some("jev-glm"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model": MODEL}))
            .ready(200, json!({"status": "ok"}))
            .systemone(
                200,
                json!({"answers": {"verdict": {"choice": "destroy"}}, "model": MODEL}),
            ),
    );
    all.push(
        new("jev_model_mismatch")
            .overrides(Overrides {
                kind: Some("jev-glm"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model": MODEL}))
            .ready(200, json!({"status": "ok"}))
            .systemone(
                200,
                json!({"answers": {"verdict": {"choice": "read"}}, "model": "Other"}),
            ),
    );

    // --- the edge cases the fleet-check alignment pinned ---
    all.push(
        new("shared_float_busy")
            .overrides(Overrides {
                shared: Some(Scalar::Bool(true)),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 1.0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("model_ids_string")
            .health(200, json!({"ok": true, "model_ids": MODEL, "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("model_ids_object")
            .health(200, json!({"ok": true, "model_ids": {"a": 1}, "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("model_ids_int")
            .overrides(Overrides {
                model: Some("5"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [5], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("sanity_raw_bytes")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat_raw(b"\x80\x81\x82 not json"),
    );
    all.push(
        new("expected_logprobs_one")
            .overrides(Overrides {
                expected: &[("logprobs", Scalar::Int(1))],
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(Chat::Ideal)
            .completions(Completions::IdealPlp),
    );
    all.push(
        new("shared_string_false")
            .overrides(Overrides {
                shared: Some(Scalar::Str("false")),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 2, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("read_empty")
            .overrides(Overrides {
                kind: Some("lfm2d"),
                read: Some(""),
                ..Overrides::default()
            })
            .models(200, json!([{"id": MODEL}]))
            .spans(200, json!([[{"text": "sk-1", "start": 0, "end": 5}]])),
    );
    all.push(
        new("health_path_empty")
            .overrides(Overrides {
                health_path: Some(""),
                ..Overrides::default()
            })
            // health_path="" asks the base URL for health; only this mock
            // serves "/" with a healthy body.
            .root(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(ok_chat())),
    );

    // --- non-finite tokens (3d3e804) ---
    all.push(
        new("nonfinite_running")
            .health_json(200, r#"{"ok": true, "model_ids": ["M-1"], "version": "1.2.3", "requests_running": NaN, "requests_waiting": 0}"#)
            .chat(Chat::Ideal),
    );
    all.push(
        new("nonfinite_running_shared")
            .overrides(Overrides {
                shared: Some(Scalar::Bool(true)),
                ..Overrides::default()
            })
            .health_json(200, r#"{"ok": true, "model_ids": ["M-1"], "version": "1.2.3", "requests_running": NaN, "requests_waiting": 0}"#)
            .chat(Chat::Ideal),
    );
    all.push(
        new("nonfinite_waiting")
            .health_json(200, r#"{"ok": true, "model_ids": ["M-1"], "version": "1.2.3", "requests_running": 0, "requests_waiting": -Infinity}"#)
            .chat(Chat::Ideal),
    );
    all.push(
        new("laya_nan_probability")
            .overrides(Overrides {
                kind: Some("laya"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "loaded": ["en"], "device": "cuda:0"}))
            .systemone_json(200, r#"{"answers": {"intent": {"choice": "billing", "probabilities": {"billing": NaN}}}, "model": "M-1"}"#),
    );
    all.push(
        new("laya_neg_inf_probability")
            .overrides(Overrides {
                kind: Some("laya"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "loaded": ["en"], "device": "cuda:0"}))
            .systemone_json(200, r#"{"answers": {"intent": {"choice": "billing", "probabilities": {"billing": -Infinity}}}, "model": "M-1"}"#),
    );
    all.push(
        new("laya_nan_model")
            .overrides(Overrides {
                kind: Some("laya"),
                ..Overrides::default()
            })
            .health(200, json!({"ok": true, "loaded": ["en"], "device": "cuda:0"}))
            .systemone_json(200, r#"{"answers": {"intent": {"choice": "billing", "probabilities": {"billing": 0.87}}}, "model": NaN}"#),
    );
    all.push(
        new("model_ids_nan_list")
            .health_json(200, r#"{"ok": true, "model_ids": [NaN], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}"#)
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("model_ids_nan_scalar")
            .health_json(200, r#"{"ok": true, "model_ids": NaN, "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}"#)
            .chat(fixed(ok_chat())),
    );
    all.push(
        new("chat_ignored_nan")
            .health(200, json!({"ok": true, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}))
            .chat(fixed(Reply::json(
                200,
                r#"{"choices": [{"message": {"content": "42"}}], "usage": {"prompt_tokens": NaN}}"#,
            ))),
    );

    all
}

/// The four malformed registries parity.py writes, name -> exact contents.
pub const BAD_REGISTRIES: [(&str, &str); 4] = [
    ("engines-not-a-table", "engines = 5\n"),
    (
        "engine-entry-is-a-string",
        "engines = { bad = \"hello\" }\n",
    ),
    (
        "engine-without-url",
        "[engines.nourl]\nmodel = \"M-1\"\nhost = \"h\"\n",
    ),
    (
        "engine-without-model",
        "[engines.nomodel]\nurl = \"http://127.0.0.1:1\"\nhost = \"h\"\nkind = \"tensorfold\"\n",
    ),
];

/// One `[engines.*]` entry as parity.py writes it.
pub struct EngineEntry {
    pub name: &'static str,
    pub url: String,
    pub model: &'static str,
    pub host: String,
    pub kind: &'static str,
    pub shared: Option<Scalar>,
    pub health_path: Option<&'static str>,
    pub read: Option<&'static str>,
    pub exporter_url: Option<String>,
    pub expected: Vec<(&'static str, Scalar)>,
}

impl EngineEntry {
    pub fn from_scenario(scenario: &Scenario, url: String) -> Self {
        let overrides = scenario.overrides;
        Self {
            name: scenario.name,
            model: overrides.model.unwrap_or(MODEL),
            host: format!("h-{}", scenario.name),
            kind: overrides.kind.unwrap_or("tensorfold"),
            shared: overrides.shared,
            health_path: overrides.health_path,
            read: overrides.read,
            exporter_url: if overrides.exporter_self {
                Some(url.clone())
            } else {
                None
            },
            expected: overrides.expected.to_vec(),
            url,
        }
    }
}

fn scalar(value: Scalar) -> String {
    match value {
        Scalar::Str(text) => serde_json::to_string(text).expect("quote a TOML string"),
        Scalar::Bool(flag) => if flag { "true" } else { "false" }.to_string(),
        Scalar::Int(number) => number.to_string(),
    }
}

/// The registry text, written the way parity.py's `toml_value` writer does:
/// one table per engine in sorted order, keys sorted, `expected_features` as a
/// subtable after a blank line.
pub fn registry_text(entries: &[EngineEntry]) -> String {
    let mut entries: Vec<&EngineEntry> = entries.iter().collect();
    entries.sort_by_key(|entry| entry.name);
    let mut lines: Vec<String> = Vec::new();
    for entry in entries {
        lines.push(format!("[engines.{}]", entry.name));
        let mut fields: Vec<(&str, String)> = vec![
            (
                "host",
                serde_json::to_string(&entry.host).expect("quote host"),
            ),
            (
                "kind",
                serde_json::to_string(entry.kind).expect("quote kind"),
            ),
            (
                "model",
                serde_json::to_string(entry.model).expect("quote model"),
            ),
            ("url", serde_json::to_string(&entry.url).expect("quote url")),
        ];
        if let Some(health_path) = entry.health_path {
            fields.push((
                "health_path",
                serde_json::to_string(health_path).expect("quote health_path"),
            ));
        }
        if let Some(read) = entry.read {
            fields.push(("read", serde_json::to_string(read).expect("quote read")));
        }
        if let Some(exporter) = entry.exporter_url.as_deref() {
            fields.push((
                "exporter_url",
                serde_json::to_string(exporter).expect("quote exporter_url"),
            ));
        }
        if let Some(shared) = entry.shared {
            fields.push(("shared", scalar(shared)));
        }
        fields.sort_by_key(|(key, _)| *key);
        for (key, value) in fields {
            lines.push(format!("{key} = {value}"));
        }
        if !entry.expected.is_empty() {
            lines.push(String::new());
            lines.push(format!("[engines.{}.expected_features]", entry.name));
            let mut expected = entry.expected.clone();
            expected.sort_by_key(|(key, _)| *key);
            for (key, value) in expected {
                lines.push(format!("{key} = {}", scalar(value)));
            }
        }
        lines.push(String::new());
    }
    lines.join("\n")
}
