use anyhow::Result;
use axum::{
    extract::State,
    response::{sse::Event, Sse},
    Json,
};
use futures_util::{Stream, StreamExt};
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::{
    sampler::Sampler, FinishReason, GenerateRequest, OptionArray, RequestKind, ThreadRequest,
    Token, TokenCounter,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    #[serde(alias = "system")]
    System,
    #[default]
    #[serde(alias = "user")]
    User,
    #[serde(alias = "assistant")]
    Assistant,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::System => write!(f, "System"),
            Role::User => write!(f, "User"),
            Role::Assistant => write!(f, "Assistant"),
        }
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct ChatRecord {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ChatRequest {
    pub messages: OptionArray<ChatRecord>,
    pub max_tokens: usize,
    pub stop: OptionArray<String>,
    pub temperature: f32,
    pub top_p: f32,
    pub presence_penalty: f32,
    pub frequency_penalty: f32,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            messages: OptionArray::default(),
            max_tokens: 256,
            stop: OptionArray::Item("\n\n".into()),
            temperature: 1.0,
            top_p: 1.0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
        }
    }
}

impl From<ChatRequest> for GenerateRequest {
    fn from(value: ChatRequest) -> Self {
        let ChatRequest {
            messages,
            max_tokens,
            stop,
            temperature,
            top_p,
            presence_penalty,
            frequency_penalty,
        } = value;

        let prompt = Vec::from(messages)
            .into_iter()
            .map(|ChatRecord { role, content }| {
                let role = role.to_string();
                let content = content.trim();
                format!("{role}: {content}")
            })
            .join("\n\n");

        let assistant = Role::Assistant.to_string();
        let prompt = prompt + &format!("\n\n{assistant}:");

        let max_tokens = max_tokens.min(crate::MAX_TOKENS);
        let stop = stop.into();

        Self {
            prompt,
            max_tokens,
            stop,
            sampler: Sampler {
                top_p,
                temperature,
                presence_penalty,
                frequency_penalty,
            },
            occurrences: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatChoice {
    pub message: ChatRecord,
    pub index: usize,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatResponse {
    pub object: String,
    pub choices: Vec<ChatChoice>,
    #[serde(rename = "usage")]
    pub counter: TokenCounter,
}

pub async fn chat_completions(
    State(sender): State<flume::Sender<ThreadRequest>>,
    Json(request): Json<ChatRequest>,
) -> Json<ChatResponse> {
    let (token_sender, token_receiver) = flume::unbounded();

    let _ = sender.send(ThreadRequest {
        request: RequestKind::Chat(request),
        token_sender,
    });

    let mut counter = TokenCounter::default();
    let mut finish_reason = FinishReason::Null;
    let mut text = String::new();
    let mut stream = token_receiver.into_stream();

    while let Some(token) = stream.next().await {
        match token {
            Token::PromptTokenCount(prompt_tokens) => counter.prompt_tokens = prompt_tokens,
            Token::Token(token) => {
                text += &token;
                counter.completion_tokens += 1;
            }
            Token::Stop => {
                finish_reason = FinishReason::Stop;
                break;
            }
            Token::CutOff | Token::EndOfText => {
                finish_reason = FinishReason::Length;
                break;
            }
        }
    }

    counter.total_tokens = counter.prompt_tokens + counter.completion_tokens;

    Json(ChatResponse {
        object: "chat.completion".into(),
        choices: vec![ChatChoice {
            message: ChatRecord {
                role: Role::Assistant,
                content: text,
            },
            index: 0,
            finish_reason,
        }],
        counter,
    })
}

#[derive(Default, Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkChatRecord {
    #[default]
    #[serde(rename = "")]
    None,
    Role(Role),
    Content(String),
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ChunkChatChoice {
    pub delta: ChunkChatRecord,
    pub index: usize,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkChatResponse {
    pub object: String,
    pub choices: Vec<ChunkChatChoice>,
}

pub async fn chunk_chat_completions(
    State(sender): State<flume::Sender<ThreadRequest>>,
    Json(request): Json<ChatRequest>,
) -> Sse<impl Stream<Item = Result<Event>>> {
    let (token_sender, token_receiver) = flume::unbounded();

    let _ = sender.send(ThreadRequest {
        request: RequestKind::Chat(request),
        token_sender,
    });

    let stream = token_receiver.into_stream().map(|token| {
        let choice = match token {
            Token::PromptTokenCount(_) => ChunkChatChoice {
                delta: ChunkChatRecord::Role(Role::Assistant),
                index: 0,
                finish_reason: FinishReason::Null,
            },
            Token::Token(token) => ChunkChatChoice {
                delta: ChunkChatRecord::Content(token),
                index: 0,
                finish_reason: FinishReason::Null,
            },
            Token::CutOff => ChunkChatChoice {
                finish_reason: FinishReason::Length,
                ..Default::default()
            },
            Token::Stop => ChunkChatChoice {
                finish_reason: FinishReason::Stop,
                ..Default::default()
            },
            Token::EndOfText => return Ok(Event::default().data("[DONE]")),
        };

        Event::default()
            .json_data(ChunkChatResponse {
                object: "chat.completion.chunk".into(),
                choices: vec![choice],
            })
            .map_err(|err| err.into())
    });

    Sse::new(stream)
}
