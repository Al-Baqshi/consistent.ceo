//! Manual smoke test for `AnthropicProvider` against the real Anthropic API:
//! a plain completion, a tool call, and a streamed completion. Needs
//! `ANTHROPIC_API_KEY`; not run in CI.

use consistent_world_llm::anthropic::AnthropicProvider;
use consistent_world_llm::client::{
    ChatMessage, CompletionRequest, LlmProvider, Role, ToolDefinition,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set");

    let model = std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());

    let provider = AnthropicProvider::new(api_key, None);

    println!("--- Test 1: plain completion ---");
    let resp = provider
        .complete(CompletionRequest {
            model: model.clone(),
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: "You are terse. Reply in at most 8 words.".into(),
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: Role::User,
                    content: "What is the capital of France?".into(),
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            tools: None,
            max_tokens: Some(64),
            temperature: Some(0.0),
            stream: false,
        })
        .await?;
    println!(
        "content: {:?}\ntokens in/out: {}/{}\nlatency_ms: {}",
        resp.content, resp.usage.input_tokens, resp.usage.output_tokens, resp.latency_ms
    );

    println!("\n--- Test 2: tool call ---");
    let tool = ToolDefinition {
        name: "get_weather".into(),
        description: "Get the current weather for a given city.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"}
            },
            "required": ["city"]
        }),
    };
    let resp = provider
        .complete(CompletionRequest {
            model: model.clone(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "What's the weather in Tokyo? Call the tool.".into(),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Some(vec![tool]),
            max_tokens: Some(512),
            temperature: Some(0.0),
            stream: false,
        })
        .await?;
    println!(
        "content: {:?}\ntool_calls: {:?}\nlatency_ms: {}",
        resp.content, resp.tool_calls, resp.latency_ms
    );

    println!("\n--- Test 3: streaming ---");
    use futures::StreamExt;
    let mut stream = provider
        .stream_complete(CompletionRequest {
            model: model.clone(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "Count from 1 to 5 with a word for each number.".into(),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            max_tokens: Some(128),
            temperature: Some(0.0),
            stream: true,
        })
        .await?;

    while let Some(event) = stream.next().await {
        print!("{:?} ", event);
    }
    println!("\n\nAll three smoke tests passed.");
    Ok(())
}
