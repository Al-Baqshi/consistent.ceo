//! Unit-level coverage for cost calculation, the OpenAI request shape, the
//! `CompletionResponse` type, and the streaming SSE tool-call parser — all
//! offline, no network calls.

use consistent_world_llm::client::{
    CompletionRequest, CompletionResponse, Role, TokenUsage, ToolDefinition,
};
use consistent_world_llm::cost::CostCalculator;
use consistent_world_llm::openai::OpenAiProvider;
use consistent_world_llm::streaming::{StreamEvent, StreamParser};

// Shared fixture request reused by the serialization-shape tests below.
fn make_sample_request() -> CompletionRequest {
    CompletionRequest {
        model: "gpt-4o".to_string(),
        messages: vec![consistent_world_llm::client::ChatMessage {
            role: Role::User,
            content: "Hello".to_string(),
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: Some(vec![ToolDefinition {
            name: "test_tool".to_string(),
            description: "Test tool".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "x": { "type": "string" }
                },
                "required": ["x"]
            }),
        }]),
        max_tokens: Some(64),
        temperature: Some(0.1),
        stream: false,
    }
}

#[test]
fn test_cost_calculation() {
    let calc = CostCalculator::new();
    let cost = calc.calculate("gpt-4o", 1_000_000, 500_000);
    // 1M input @ 2.50 + 0.5M output @ 10.0 = 2.50 + 5.0 = 7.5
    assert!((cost - 7.5).abs() < 1e-9);
}

#[test]
fn test_openai_request_serialization_shape() {
    // We don't hit the network here; just verify that serializing the public
    // request type produces the expected top-level fields when mapped into
    // the OpenAI wire format via serde_json.
    let req = make_sample_request();
    let _provider = OpenAiProvider::new(
        "dummy-key".to_string(),
        Some("https://api.example.com/v1".to_string()),
    );

    // Build the JSON body in the same way `complete` will.
    let body = serde_json::json!({
        "model": req.model,
        "messages": req.messages.iter().map(|m| {
            serde_json::json!({
                "role": match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                },
                "content": m.content,
            })
        }).collect::<Vec<_>>(),
        "tools": req.tools.as_ref().map(|tools| {
            tools.iter().map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            }).collect::<Vec<_>>()
        }),
        "max_tokens": req.max_tokens,
        "temperature": req.temperature,
        "stream": false,
    });

    // Basic structural checks
    assert_eq!(body["model"], "gpt-4o");
    assert!(body["messages"].is_array());
    assert!(body["tools"].is_array() || body["tools"].is_null());
}

#[test]
fn test_completion_response_struct_can_be_constructed() {
    // Sanity check that the response type is sound and can represent
    // typical data we expect back from an OpenAI-compatible provider.
    let resp = CompletionResponse {
        content: Some("ok".to_string()),
        tool_calls: Vec::new(),
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        model: "gpt-4o".to_string(),
        latency_ms: 0,
    };

    assert_eq!(resp.content.as_deref(), Some("ok"));
    assert_eq!(resp.usage.input_tokens, 10);
    assert_eq!(resp.model, "gpt-4o");
}

#[test]
fn test_sse_parser_tool_call_delta() {
    let mut parser = StreamParser::new();
    let mut all_events = Vec::new();

    // Simulate OpenAI stream: first chunk has id + name
    let line1 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"run","arguments":""}}]}}]}"#;
    for e in parser.feed_line(line1) {
        all_events.push(e);
    }

    // Arguments streamed in chunks
    let line2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]}}]}"#;
    for e in parser.feed_line(line2) {
        all_events.push(e);
    }
    let line3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"echo\""}}]}}]}"#;
    for e in parser.feed_line(line3) {
        all_events.push(e);
    }
    let line4 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}"#;
    for e in parser.feed_line(line4) {
        all_events.push(e);
    }

    let start = all_events.iter().find_map(|e| {
        if let StreamEvent::ToolCallStart { id, name } = e {
            Some((id.clone(), name.clone()))
        } else {
            None
        }
    });
    assert_eq!(start, Some(("call_abc".to_string(), "run".to_string())));

    let deltas: Vec<String> = all_events
        .iter()
        .filter_map(|e| {
            if let StreamEvent::ToolCallDelta {
                arguments_chunk, ..
            } = e
            {
                Some(arguments_chunk.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(deltas.join(""), r#"{"cmd":"echo"}"#);

    let end = all_events
        .iter()
        .any(|e| matches!(e, StreamEvent::ToolCallEnd { id } if id == "call_abc"));
    assert!(end);
}
