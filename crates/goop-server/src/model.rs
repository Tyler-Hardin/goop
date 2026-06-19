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
use rig::client::CompletionClient;
use rig::providers::{anthropic, deepseek, groq, ollama, openai, openrouter, zai};
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};

use crate::config::{self, Config, Provider};
use crate::memory::SessionMemory;
use crate::session_state::SessionState;

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
    /// Z.ai uses an OpenAI-compatible streaming protocol, so the response
    /// type is the same as OpenAI's.
    Zai(openai::completion::streaming::StreamingCompletionResponse),
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
    /// Z.ai / GLM — OpenAI-compatible API.  Only GLM-5.2 is used.
    Zai(Agent<openai::completion::GenericCompletionModel<zai::ZAiExt>>),
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
            AnyAgent::Zai(a) => AnyStream::Zai(a.stream_prompt(prompt).await),
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
    /// Z.ai uses the same OpenAI-compatible streaming protocol.
    Zai(rig::agent::StreamingResult<openai::completion::streaming::StreamingCompletionResponse>),
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
            AnyStream::Zai(inner) => map_stream_poll(inner, cx, AnyStreamingResponse::Zai),
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

/// Build an agent for the configured provider, attaching tools from
/// the supplied [`SessionState`] and MCP proxy tools.
pub fn build_agent(
    config: &Config,
    preamble: &str,
    memory: SessionMemory,
    state: Arc<SessionState>,
    mcp_tools: Vec<Box<dyn rig::tool::ToolDyn>>,
) -> anyhow::Result<Arc<AnyAgent>> {
    let provider = config.provider();
    let model_name = config.model_name();

    let tools = crate::tools::build_tools(config, &state);
    let mut all_tools = tools;
    all_tools.extend(mcp_tools);

    // Wrap memory in Option so each match arm can take ownership
    // without requiring Clone on the (non-Clone) SessionMemory.
    let mut memory = Some(memory);

    /// One arm body — constructs a provider-specific client, then
    /// calls finish_agent with the tools and memory taken from `memory`.
    macro_rules! arm {
        ($variant:ident, $new_client:expr) => {{
            let client = $new_client;
            AnyAgent::$variant(finish_agent(
                client.agent(model_name).preamble(preamble),
                config,
                memory
                    .take()
                    .expect("bug: memory already consumed in match"),
                all_tools,
            ))
        }};
    }

    let any_agent = match provider {
        Provider::DeepSeek => arm!(
            DeepSeek,
            deepseek::Client::new(&config::api_key_for(provider).map_err(anyhow::Error::new)?)?
        ),
        Provider::OpenAI => arm!(
            OpenAI,
            openai::CompletionsClient::new(
                &config::api_key_for(provider).map_err(anyhow::Error::new)?
            )?
        ),
        Provider::OpenRouter => arm!(
            OpenRouter,
            openrouter::Client::new(&config::api_key_for(provider).map_err(anyhow::Error::new)?)?
        ),
        Provider::Groq => arm!(
            Groq,
            groq::Client::new(&config::api_key_for(provider).map_err(anyhow::Error::new)?)?
        ),
        Provider::Ollama => {
            let ollama_api_key = std::env::var("OLLAMA_API_KEY").ok();
            arm!(
                Ollama,
                ollama::Client::builder()
                    .api_key(ollama::OllamaApiKey::from(
                        ollama_api_key.unwrap_or_default().as_str(),
                    ))
                    .base_url(&config.ollama_base_url)
                    .build()?
            )
        }
        Provider::Anthropic => arm!(
            Anthropic,
            anthropic::Client::new(&config::api_key_for(provider).map_err(anyhow::Error::new)?)?
        ),
        Provider::Zai => arm!(
            Zai,
            zai::Client::new(&config::api_key_for(provider).map_err(anyhow::Error::new)?)?
        ),
    };

    tracing::info!("● provider · {}  model · {}", provider.label(), model_name);

    Ok(Arc::new(any_agent))
}

/// Common builder finalisation shared by all providers.
fn finish_agent<M: rig::completion::CompletionModel>(
    builder: rig::agent::AgentBuilder<M>,
    config: &Config,
    memory: SessionMemory,
    tools: Vec<Box<dyn rig::tool::ToolDyn>>,
) -> Agent<M> {
    builder
        .tools(tools)
        .max_tokens(config.max_tokens)
        .default_max_turns(config.default_max_turns)
        .conversation_id("default")
        .memory(memory)
        .build()
}
