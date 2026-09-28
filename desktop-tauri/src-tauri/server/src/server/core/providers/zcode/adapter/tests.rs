use super::*;
use crate::server::core::protocol::{anthropic, responses};
use crate::server::core::providers::adapter::ReasoningPatch;
use serde_json::json;

fn ingress(protocol: &str, model: &str, effort: Option<&str>, stream: bool) -> Value {
    let mut body = json!({"model": model, "stream": stream,
        "messages": [{"role": "user", "content": "hello"}]});
    match protocol {
        "responses" => {
            body["input"] = json!("hello");
            if let Some(effort) = effort {
                body["reasoning"] = json!({"effort": effort});
            }
            responses::chat_from_responses(&body).unwrap()
        }
        "messages" => {
            body["max_tokens"] = json!(4096);
            if let Some(effort) = effort {
                body["thinking"] = json!({"type": "adaptive"});
                body["output_config"] = json!({"effort": effort});
            }
            anthropic::chat_from_anthropic(&body).unwrap()
        }
        _ => {
            if let Some(effort) = effort {
                body["reasoning_effort"] = json!(effort);
            }
            body
        }
    }
}

fn bind(adapter: &ZcodeAdapter, body: &mut Value, level: &str) {
    let model = body["model"].as_str().unwrap();
    if let ReasoningPatch::Set { field, value } = adapter.reasoning_patch(level, model, body) {
        body[field] = value;
    }
}

fn wire(adapter: &ZcodeAdapter, body: &Value) -> Value {
    let original = body.clone();
    let plan = adapter
        .build_chat_request(
            &json!({"auth": {"accessToken": "test-placeholder"}}),
            body,
            &HeaderMap::new(),
        )
        .unwrap();
    assert!(plan.url.ends_with("/chat/completions"));
    assert_eq!(body, &original, "发送准备不得修改入口请求");
    assert_eq!(plan.body["messages"], body["messages"]);
    assert_eq!(plan.body["stream"], body["stream"]);
    assert_eq!(
        adapter.outbound_reasoning(body),
        plan.body["reasoning_effort"].as_str().map(str::to_string)
    );
    plan.body
}

#[test]
fn three_protocols_both_regions_binding_and_client_precedence() {
    for adapter in [&ZCODE_ADAPTER, &ZCODE_INTL_ADAPTER] {
        for protocol in ["chat", "responses", "messages"] {
            for model in ["glm-5.2", "glm-5.3", "glm-5.3-flash"] {
                for stream in [false, true] {
                    let mut body = ingress(protocol, model, None, stream);
                    bind(adapter, &mut body, "max");
                    assert_eq!(wire(adapter, &body)["reasoning_effort"], "max");
                    for effort in ["minimal", "low", "medium", "high", "xhigh", "max"] {
                        let expected = match (model, effort) {
                            ("glm-5.2", "minimal") => "minimal",
                            ("glm-5.2", "low" | "medium" | "high") => "high",
                            (_, "minimal" | "low") => "low",
                            (_, "medium" | "high") => "high",
                            _ => "max",
                        };
                        let mut body = ingress(protocol, model, Some(effort), stream);
                        bind(adapter, &mut body, "medium");
                        assert_eq!(
                            wire(adapter, &body)["reasoning_effort"],
                            expected,
                            "{protocol} {model} {effort} stream={stream}"
                        );
                        let mut body = ingress(protocol, model, None, stream);
                        bind(adapter, &mut body, effort);
                        assert_eq!(
                            wire(adapter, &body)["reasoning_effort"],
                            expected,
                            "binding {protocol} {model} {effort} stream={stream}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn unsupported_models_keep_passthrough_without_injecting_defaults() {
    for adapter in [&ZCODE_ADAPTER, &ZCODE_INTL_ADAPTER] {
        for model in ["glm-4.7", "glm-5.1", "glm-4.6v", "glm-future"] {
            for protocol in ["chat", "responses", "messages"] {
                for effort in [None, Some("xhigh")] {
                    let mut body = ingress(protocol, model, effort, false);
                    let original = body.clone();
                    bind(adapter, &mut body, "high");
                    assert_eq!(wire(adapter, &body), original);
                }
            }
        }
    }
}

#[test]
fn explicit_invalid_or_disabled_chat_parameters_are_preserved() {
    for adapter in [&ZCODE_ADAPTER, &ZCODE_INTL_ADAPTER] {
        for value in [
            Value::Null,
            json!(false),
            json!(7),
            json!(""),
            json!("custom"),
            json!("none"),
            json!("off"),
        ] {
            let mut body = ingress("chat", "glm-5.3", None, false);
            body["reasoning_effort"] = value;
            let original = body.clone();
            bind(adapter, &mut body, "max");
            assert_eq!(wire(adapter, &body), original);
        }
        let mut body = ingress("chat", "glm-5.2", None, false);
        body["thinking"] = json!({"type": "disabled"});
        let original = body.clone();
        bind(adapter, &mut body, "max");
        assert_eq!(wire(adapter, &body), original);
    }
}

#[test]
fn absent_off_and_unknown_bindings_do_not_inject() {
    for adapter in [&ZCODE_ADAPTER, &ZCODE_INTL_ADAPTER] {
        for protocol in ["chat", "responses", "messages"] {
            for level in ["off", "none", "", "custom"] {
                let mut body = ingress(protocol, "glm-5.3", None, true);
                let original = body.clone();
                bind(adapter, &mut body, level);
                assert_eq!(wire(adapter, &body), original);
            }
        }
    }
}

#[test]
fn messages_budget_and_responses_nested_effort_reach_zcode() {
    let body = anthropic::chat_from_anthropic(&json!({
        "model": "glm-5.3", "messages": [{"role":"user","content":"hello"}],
        "max_tokens": 40000, "thinking": {"type":"enabled", "budget_tokens":32768}
    }))
    .unwrap();
    assert_eq!(wire(&ZCODE_ADAPTER, &body)["reasoning_effort"], "max");
    let body = ingress("responses", "GLM-5.3-FLASH", Some("xhigh"), true);
    assert_eq!(wire(&ZCODE_INTL_ADAPTER, &body)["reasoning_effort"], "max");
}
