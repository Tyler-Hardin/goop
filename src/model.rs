//! Provider abstraction — wraps rig's type-level providers behind enums so
//! goop can select a provider at runtime from configuration.
//!
//! The key insight: stream types differ only in their `R` (streaming-response)
//! parameter, and the variants we match on in `session.rs` don't use `R`.
//! So we wrap the stream and map `R` into an opaque enum.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;
use rig::agent::Agent;
use rig::client::{CompletionClient, ProviderClient};
use rig::providers::{anthropic, deepseek, groq, ollama, openai, openrouter};
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};

use crate::config::{self, Config, Provider};
use crate::memory::FileConversationMemory;

// ── opaque streaming-response enum ───────────────────────────────

/// Wraps each provider's streaming completion response so we can embed
/// the `R` in `StreamedAssistantContent::Final(R)` without the session
/// needing to know the concrete type.
///
/// Variants are never destructured — they just sit inside
/// `StreamedAssistantContent::Final` which we ignore.
#[allow(dead_code)]
pub(crate) enum AnyStreamingResponse {
    DeepSeek(deepseek::StreamingCompletionResponse),
    OpenAI(openai::completion::streaming::StreamingCompletionResponse),
    OpenRouter(openrouter::streaming::StreamingCompletionResponse),
    Groq(groq::StreamingCompletionResponse),
    Ollama(ollama::StreamingCompletionResponse),
    Anthropic(anthropic::streaming::StreamingCompletionResponse),
}

// ── agent enum ───────────────────────────────────────────────────

/// Owns a rig agent for any supported provider.
pub(crate) enum AnyAgent {
    DeepSeek(Agent<deepseek::CompletionModel>),
    OpenAI(Agent<openai::completion::CompletionModel>),
    OpenRouter(Agent<openrouter::completion::CompletionModel>),
    Groq(Agent<groq::CompletionModel>),
    Ollama(Agent<ollama::CompletionModel>),
    Anthropic(Agent<anthropic::completion::CompletionModel>),
}

impl AnyAgent {
    /// Start streaming a prompt, returning a unified stream type.
    pub(crate) async fn stream_prompt(&self, prompt: &str) -> AnyStream {
        match self {
            AnyAgent::DeepSeek(a) => AnyStream::DeepSeek(a.stream_prompt(prompt).await),
            AnyAgent::OpenAI(a) => AnyStream::OpenAI(a.stream_prompt(prompt).await),
            AnyAgent::OpenRouter(a) => AnyStream::OpenRouter(a.stream_prompt(prompt).await),
            AnyAgent::Groq(a) => AnyStream::Groq(a.stream_prompt(prompt).await),
            AnyAgent::Ollama(a) => AnyStream::Ollama(a.stream_prompt(prompt).await),
            AnyAgent::Anthropic(a) => AnyStream::Anthropic(a.stream_prompt(prompt).await),
        }
    }
}

// ── stream enum ──────────────────────────────────────────────────

/// Wraps each provider's `StreamingResult` behind a single `Stream`
/// impl so the session can consume a unified type.
pub(crate) enum AnyStream {
    DeepSeek(rig::agent::StreamingResult<deepseek::StreamingCompletionResponse>),
    OpenAI(rig::agent::StreamingResult<openai::completion::streaming::StreamingCompletionResponse>),
    OpenRouter(rig::agent::StreamingResult<openrouter::streaming::StreamingCompletionResponse>),
    Groq(rig::agent::StreamingResult<groq::StreamingCompletionResponse>),
    Ollama(rig::agent::StreamingResult<ollama::StreamingCompletionResponse>),
    Anthropic(rig::agent::StreamingResult<anthropic::streaming::StreamingCompletionResponse>),
}

impl Stream for AnyStream {
    type Item =
        Result<rig::agent::MultiTurnStreamItem<AnyStreamingResponse>, rig::agent::StreamingError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this {
            AnyStream::DeepSeek(inner) => {
                map_stream_poll(inner, cx, AnyStreamingResponse::DeepSeek)
            }
            AnyStream::OpenAI(inner) => map_stream_poll(inner, cx, AnyStreamingResponse::OpenAI),
            AnyStream::OpenRouter(inner) => {
                map_stream_poll(inner, cx, AnyStreamingResponse::OpenRouter)
            }
            AnyStream::Groq(inner) => map_stream_poll(inner, cx, AnyStreamingResponse::Groq),
            AnyStream::Ollama(inner) => map_stream_poll(inner, cx, AnyStreamingResponse::Ollama),
            AnyStream::Anthropic(inner) => {
                map_stream_poll(inner, cx, AnyStreamingResponse::Anthropic)
            }
        }
    }
}

/// Poll a concrete `StreamingResult<R>`, converting items to our
/// opaque `AnyStreamingResponse` type.
fn map_stream_poll<R>(
    inner: &mut rig::agent::StreamingResult<R>,
    cx: &mut Context<'_>,
    wrap: fn(R) -> AnyStreamingResponse,
) -> Poll<
    Option<
        Result<rig::agent::MultiTurnStreamItem<AnyStreamingResponse>, rig::agent::StreamingError>,
    >,
> {
    match Pin::new(inner).as_mut().poll_next(cx) {
        Poll::Ready(Some(Ok(item))) => Poll::Ready(Some(Ok(map_item(item, wrap)))),
        Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
        Poll::Ready(None) => Poll::Ready(None),
        Poll::Pending => Poll::Pending,
    }
}

/// Map `MultiTurnStreamItem<R>` → `MultiTurnStreamItem<AnyStreamingResponse>`.
fn map_item<R>(
    item: rig::agent::MultiTurnStreamItem<R>,
    wrap: fn(R) -> AnyStreamingResponse,
) -> rig::agent::MultiTurnStreamItem<AnyStreamingResponse> {
    match item {
        rig::agent::MultiTurnStreamItem::StreamAssistantItem(content) => {
            rig::agent::MultiTurnStreamItem::StreamAssistantItem(map_assistant(content, wrap))
        }
        rig::agent::MultiTurnStreamItem::StreamUserItem(content) => {
            rig::agent::MultiTurnStreamItem::StreamUserItem(content)
        }
        rig::agent::MultiTurnStreamItem::CompletionCall(call) => {
            rig::agent::MultiTurnStreamItem::CompletionCall(call)
        }
        rig::agent::MultiTurnStreamItem::FinalResponse(resp) => {
            rig::agent::MultiTurnStreamItem::FinalResponse(resp)
        }
        #[allow(unreachable_patterns)]
        _ => unreachable!("new MultiTurnStreamItem variant added in rig — update AnyStream"),
    }
}

/// Map `StreamedAssistantContent<R>` → `StreamedAssistantContent<AnyStreamingResponse>`.
fn map_assistant<R>(
    content: StreamedAssistantContent<R>,
    wrap: fn(R) -> AnyStreamingResponse,
) -> StreamedAssistantContent<AnyStreamingResponse> {
    match content {
        StreamedAssistantContent::Text(text) => StreamedAssistantContent::Text(text),
        StreamedAssistantContent::ToolCall {
            tool_call,
            internal_call_id,
        } => StreamedAssistantContent::ToolCall {
            tool_call,
            internal_call_id,
        },
        StreamedAssistantContent::ToolCallDelta {
            id,
            internal_call_id,
            content,
        } => StreamedAssistantContent::ToolCallDelta {
            id,
            internal_call_id,
            content,
        },
        StreamedAssistantContent::Reasoning(reasoning) => {
            StreamedAssistantContent::Reasoning(reasoning)
        }
        StreamedAssistantContent::ReasoningDelta { reasoning, id } => {
            StreamedAssistantContent::ReasoningDelta { reasoning, id }
        }
        StreamedAssistantContent::Final(r) => StreamedAssistantContent::Final(wrap(r)),
    }
}

// ── agent construction ───────────────────────────────────────────

/// Wire up all goop tools on an agent builder.
///
/// Centralised so the tool list isn't duplicated across six match arms.
macro_rules! with_goop_tools {
    ($builder:expr, $config:expr, $memory:expr) => {
        $builder
            .tool(crate::tools::Read)
            .tool(crate::tools::ReadHtml)
            .tool(crate::tools::Replace)
            .tool(crate::tools::Write)
            .tool(crate::tools::Shell)
            .tool(crate::tools::Cd)
            .tool(crate::tools::Ssh)
            .tool(crate::tools::Disconnect)
            .tool(crate::tools::WebFetch)
            .tool(crate::tools::Screenshot)
            .tool(crate::tools::CursorPosition)
            .tool(crate::tools::MouseMove)
            .tool(crate::tools::MouseClick)
            .tool(crate::tools::KeyType)
            .tool(crate::tools::KeyPress)
            .tool(crate::tools::WindowList)
            .tool(crate::tools::WindowFocus)
            .tool(crate::tools::WindowGetActive)
            .tool(crate::tools::OpenUrl)
            .max_tokens($config.max_tokens)
            .default_max_turns($config.default_max_turns)
            .conversation_id("default")
            .memory($memory)
            .build()
    };
}

/// Build an agent for the configured provider.
///
/// Reads the appropriate `*_API_KEY` env var and constructs the rig
/// agent with all goop tools and the supplied conversation memory.
pub fn build_agent(
    config: &Config,
    preamble: &str,
    memory: FileConversationMemory,
) -> anyhow::Result<Arc<AnyAgent>> {
    let api_key = config::api_key_for(config.provider)?;
    let model_name = config
        .model
        .as_deref()
        .unwrap_or(config.provider.default_model());

    // Only one arm executes, but Rust sees a potential move in each.
    // Use Option::take so the memory is moved exactly once.
    let mut memory = Some(memory);

    let any_agent = match config.provider {
        Provider::DeepSeek => {
            let client = deepseek::Client::new(&api_key)?;
            let builder = client.agent(model_name).preamble(preamble);
            AnyAgent::DeepSeek(with_goop_tools!(builder, config, memory.take().unwrap()))
        }
        Provider::OpenAI => {
            let client = openai::CompletionsClient::new(&api_key)?;
            let builder = client.agent(model_name).preamble(preamble);
            AnyAgent::OpenAI(with_goop_tools!(builder, config, memory.take().unwrap()))
        }
        Provider::OpenRouter => {
            let client = openrouter::Client::new(&api_key)?;
            let builder = client.agent(model_name).preamble(preamble);
            AnyAgent::OpenRouter(with_goop_tools!(builder, config, memory.take().unwrap()))
        }
        Provider::Groq => {
            let client = groq::Client::new(&api_key)?;
            let builder = client.agent(model_name).preamble(preamble);
            AnyAgent::Groq(with_goop_tools!(builder, config, memory.take().unwrap()))
        }
        Provider::Ollama => {
            // Ollama reads OLLAMA_API_BASE_URL (defaults to localhost:11434) and
            // optional OLLAMA_API_KEY from the environment.
            let client = ollama::Client::from_env()?;
            let builder = client.agent(model_name).preamble(preamble);
            AnyAgent::Ollama(with_goop_tools!(builder, config, memory.take().unwrap()))
        }
        Provider::Anthropic => {
            let client = anthropic::Client::new(&api_key)?;
            let builder = client.agent(model_name).preamble(preamble);
            AnyAgent::Anthropic(with_goop_tools!(builder, config, memory.take().unwrap()))
        }
    };

    tracing::info!(
        "● provider · {}  model · {}",
        config.provider.label(),
        model_name
    );

    Ok(Arc::new(any_agent))
}
