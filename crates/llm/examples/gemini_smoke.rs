//! Manual smoke test for `GoogleProvider` against the real Gemini API: a
//! plain completion, a tool call, and a streamed completion. Needs
//! `GOOGLE_API_KEY` or `GEMINI_API_KEY`; not run in CI.

use consistent_world_llm::client::{
    ChatMessage, CompletionRequest, LlmProvider, Role, ToolDefinition,
};
use consistent_world_llm::google::GoogleProvider;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api_key = std::env::var("GOOGLE_API_KEY")
        .or_else(|_| std::env::var("GEMINI_API_KEY"))
        .expect("GOOGLE_API_KEY or GEMINI_API_KEY must be set");

    let model = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.0-flash".to_string());

    let provider = GoogleProvider::new(api_key, None);

    // 1) Plain completion
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

    // 2) Tool call
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
            max_tokens: Some(256),
            temperature: Some(0.0),
            stream: false,
        })
        .await?;
    println!(
        "content: {:?}\ntool_calls: {:?}\nlatency_ms: {}",
        resp.content, resp.tool_calls, resp.latency_ms
    );

    // 3) Streaming
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
