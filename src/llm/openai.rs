use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::ToolDef;

// ─── Single-turn types ───────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    /// "required" forces a tool call (no prose); omitted = model's choice (auto).
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Message {
    Standard { role: String, content: String },
    Assistant { role: String, content: Option<String>, tool_calls: Vec<ToolCallMessage> },
    ToolResult { role: String, tool_call_id: String, content: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCallMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
struct OpenAITool {
    #[serde(rename = "type")]
    kind: String,
    function: OpenAIFunction,
}

#[derive(Serialize)]
struct OpenAIFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

// ─── Response types ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ChatResponse {
    choices: Option<Vec<Choice>>,
    error: Option<OpenAIError>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCallMessage>>,
}

#[derive(Deserialize)]
struct OpenAIError {
    message: String,
}

// ─── Public API ──────────────────────────────────────────────────────────

/// Guarantee the literal token "json" is present in the messages when
/// `structured` json_object mode is on (OpenAI rejects the request otherwise).
fn ensure_json_token(structured: bool, system: &str, user: &str) -> String {
    let missing = structured
        && !system.to_lowercase().contains("json")
        && !user.to_lowercase().contains("json");
    if missing {
        format!("{system}\nRespond in JSON.")
    } else {
        system.to_owned()
    }
}

pub async fn complete(
    http: &Client,
    model: &str,
    api_key: &str,
    system: &str,
    user: &str,
    temperature: f32,
    structured: bool,
) -> Result<String> {
    let url = "https://api.openai.com/v1/chat/completions";

    let system_owned = ensure_json_token(structured, system, user);

    let body = ChatRequest {
        model: model.to_owned(),
        messages: vec![
            Message::Standard { role: "system".to_owned(), content: system_owned },
            Message::Standard { role: "user".to_owned(), content: user.to_owned() },
        ],
        temperature,
        response_format: structured.then(|| ResponseFormat { kind: "json_object".to_owned() }),
        tools: None,
        tool_choice: None,
    };

    let resp = http
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await
        .context("OpenAI HTTP request failed")?;

    let status = resp.status();
    let text = resp.text().await.context("failed to read OpenAI response body")?;

    if !status.is_success() {
        bail!("OpenAI API returned HTTP {status}: {text}");
    }

    let parsed: ChatResponse = serde_json::from_str(&text)
        .context("failed to parse OpenAI response JSON")?;

    if let Some(err) = parsed.error {
        bail!("OpenAI API error: {}", err.message);
    }

    let result_text = parsed.choices
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.message.content)
        .unwrap_or_default();

    Ok(result_text)
}

/// Result of a single turn in the tool-calling loop.
pub enum ToolTurnResult {
    /// Model returned text (done).
    Text(String),
    /// Model requested tool calls.
    ToolCalls(Vec<ToolCallMessage>),
}

/// Send a multi-turn tool-calling request to OpenAI.
#[allow(clippy::too_many_arguments)]
pub async fn complete_with_tools(
    http: &Client,
    model: &str,
    api_key: &str,
    system: &str,
    contents: &[super::ChatMessage],
    tools: &[ToolDef],
    temperature: f32,
    force_tool_use: bool,
) -> Result<ToolTurnResult> {
    let url = "https://api.openai.com/v1/chat/completions";

    let mut messages: Vec<Message> = Vec::with_capacity(contents.len() + 1);
    messages.push(Message::Standard { role: "system".to_owned(), content: system.to_owned() });

    for msg in contents {
        match msg {
            super::ChatMessage::User(text) => {
                messages.push(Message::Standard { role: "user".to_owned(), content: text.clone() });
            }
            super::ChatMessage::ModelToolCalls(calls) => {
                let tool_calls: Vec<ToolCallMessage> = calls.iter().map(|c| ToolCallMessage {
                    id: c.id.clone().unwrap_or_default(),
                    kind: "function".to_owned(),
                    function: ToolCallFunction {
                        name: c.name.clone(),
                        arguments: c.args.to_string(),
                    },
                }).collect();
                messages.push(Message::Assistant {
                    role: "assistant".to_owned(),
                    content: None,
                    tool_calls,
                });
            }
            super::ChatMessage::ToolResults(results) => {
                for r in results {
                    messages.push(Message::ToolResult {
                        role: "tool".to_owned(),
                        tool_call_id: r.id.clone().unwrap_or_default(),
                        content: r.content.clone(),
                    });
                }
            }
        }
    }

    let openai_tools: Vec<OpenAITool> = tools.iter().map(|t| OpenAITool {
        kind: "function".to_owned(),
        function: OpenAIFunction {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
        },
    }).collect();

    let body = ChatRequest {
        model: model.to_owned(),
        messages,
        temperature,
        response_format: None,
        tools: Some(openai_tools),
        // "required" forces a tool call while no chunk is committed; otherwise
        // omit (model may finish with a text summary).
        tool_choice: force_tool_use.then(|| "required".to_owned()),
    };

    let resp = http
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await
        .context("OpenAI tool-calling HTTP request failed")?;

    let status = resp.status();
    let text = resp.text().await.context("failed to read OpenAI response body")?;

    if !status.is_success() {
        bail!("OpenAI API returned HTTP {status}: {text}");
    }

    let parsed: ChatResponse = serde_json::from_str(&text)
        .context("failed to parse OpenAI response JSON")?;

    if let Some(err) = parsed.error {
        bail!("OpenAI API error: {}", err.message);
    }

    let choice = parsed.choices
        .and_then(|c| c.into_iter().next());

    match choice {
        Some(c) => {
            if let Some(tool_calls) = c.message.tool_calls
                && !tool_calls.is_empty()
            {
                return Ok(ToolTurnResult::ToolCalls(tool_calls));
            }
            Ok(ToolTurnResult::Text(c.message.content.unwrap_or_default()))
        }
        None => Ok(ToolTurnResult::Text(String::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::ensure_json_token;

    #[test]
    fn token_present_in_system_is_unchanged() {
        let s = ensure_json_token(true, "Respond with a JSON object.", "rank these");
        assert_eq!(s, "Respond with a JSON object.");
    }

    #[test]
    fn token_present_in_user_leaves_system_unchanged() {
        let s = ensure_json_token(true, "You are a ranker.", "reply as json please");
        assert_eq!(s, "You are a ranker.");
    }

    #[test]
    fn token_absent_appends_directive() {
        let s = ensure_json_token(true, "You are a ranker.", "rank these chunks");
        assert!(s.to_lowercase().contains("json"), "must inject the json token");
    }

    #[test]
    fn not_structured_never_modifies() {
        let s = ensure_json_token(false, "You are a ranker.", "rank these chunks");
        assert_eq!(s, "You are a ranker.");
    }
}
