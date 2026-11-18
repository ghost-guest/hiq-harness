//! OpenAI 兼容的模型客户端。
//!
//! 实际传输由 [`crate::llm_transport::Transport`] 完成（基于 `genai`，
//! 进程内真流式）。这个类型是 runner 面向的薄封装：
//! 它把 complete conversation 发出去，拿回最终文本、工具调用与 usage。
//!
//! 取代了原先 `spawn curl` + `.output()` 的实现 —— 那条路径必须等进程
//! 退出才能拿到任何字节，既拿不到 TTFT、也无法 token 级流式，
//! 而且请求体走 argv，长上下文会撞 ARG_MAX。

use crate::llm_transport::{StreamChunk, Transport};
use harness_core::agent_loop::{ModelClient, ModelResponse, ToolRequest};
use serde_json::Value;

pub struct OpenAiClient {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// 推理强度：low / high / max
    pub effort: Option<String>,
}

/// 模型响应里的 token 用量，用于校准本地估算。
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
}

/// 一次请求的结果：最终文本或工具调用。
pub struct Completion {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolRequest>,
    pub usage: Usage,
    /// 结束原因（Stop / Length / ToolCalls …）。
    pub stop_reason: Option<String>,
}

impl OpenAiClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            effort: None,
        }
    }

    fn transport(&self) -> anyhow::Result<Transport> {
        Transport::new(
            &self.base_url,
            &self.api_key,
            &self.model,
            self.effort.clone(),
        )
    }

    /// 发一次带 tools 的流式请求。
    ///
    /// `tools` 是 OpenAI 格式的工具定义数组；`max_tokens` 是本轮上限。
    /// `on_delta` 在流式过程中被逐段调用（正文增量）。
    pub fn complete_with_tools(
        &self,
        messages: Vec<Value>,
        tools: Value,
        max_tokens: u32,
        mut on_delta: impl FnMut(&str),
    ) -> anyhow::Result<Completion> {
        let transport = self
            .transport()?
            .with_extra_body(serde_json::json!({ "max_tokens": max_tokens }));
        self.run_stream(transport, messages, tools, &mut on_delta)
    }

    fn run_stream(
        &self,
        transport: Transport,
        messages: Vec<Value>,
        tools: Value,
        on_delta: &mut impl FnMut(&str),
    ) -> anyhow::Result<Completion> {
        let end = transport.stream_chat_with_tools(messages, tools, |chunk| {
            if let StreamChunk::Text(t) = chunk {
                on_delta(&t);
            }
        })?;

        let tool_calls = end
            .tool_calls
            .iter()
            .map(|tc| ToolRequest {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.arguments.clone(),
            })
            .collect();

        Ok(Completion {
            text: end.text.filter(|t| !t.trim().is_empty()),
            tool_calls,
            usage: end
                .usage
                .map(|u| Usage {
                    prompt_tokens: Some(u.prompt_tokens),
                    completion_tokens: Some(u.completion_tokens),
                    cached_tokens: u.cached_tokens,
                })
                .unwrap_or_default(),
            stop_reason: end.stop_reason,
        })
    }

    /// 发一次请求，等它跑完，返回最终文本 / 工具调用 / 用量。
    ///
    /// `on_delta` 会在流式过程中被逐段调用（正文增量），调用方可据此
    /// 做增量渲染。不需要就传 `|_| {}`。
    pub fn complete(
        &self,
        messages: Vec<Value>,
        mut on_delta: impl FnMut(&str),
    ) -> anyhow::Result<Completion> {
        let transport = self.transport()?;
        self.run_stream(transport, messages, Value::Null, &mut on_delta)
    }
}

impl ModelClient for OpenAiClient {
    async fn call(&self, messages: &[String]) -> anyhow::Result<ModelResponse> {
        // 把纯文本消息列表转成 chat 格式。
        // 第一条当 system，其余按 user/assistant 交替。
        let mut msgs: Vec<Value> = Vec::new();
        for (i, m) in messages.iter().enumerate() {
            let role = if i == 0 {
                "system"
            } else if i % 2 == 0 {
                "user"
            } else {
                "assistant"
            };
            msgs.push(serde_json::json!({"role": role, "content": m}));
        }

        let done = self.complete(msgs, |_| {})?;
        match done.text {
            Some(t) => Ok(ModelResponse::Text(t)),
            None if !done.tool_calls.is_empty() => Ok(ModelResponse::ToolCalls(done.tool_calls)),
            None => Ok(ModelResponse::Text(String::new())),
        }
    }
}

/// 解析模型响应，抽出文本或工具调用。
pub fn parse_model_response(resp: &Value) -> anyhow::Result<ModelResponse> {
    let choice = resp
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| anyhow::anyhow!("响应缺少 choices"))?;

    let finish = choice.get("finish_reason").and_then(|f| f.as_str());
    let msg = choice
        .get("message")
        .ok_or_else(|| anyhow::anyhow!("缺少 message"))?;

    // 工具调用优先
    if let Some(calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        if !calls.is_empty() {
            let mut reqs = Vec::new();
            for c in calls {
                let id = c
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("call")
                    .to_string();
                let f = c
                    .get("function")
                    .ok_or_else(|| anyhow::anyhow!("tool_call 缺 function"))?;
                let name = f
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let args_str = f.get("arguments").and_then(|x| x.as_str()).unwrap_or("{}");
                let input = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                reqs.push(ToolRequest { id, name, input });
            }
            return Ok(ModelResponse::ToolCalls(reqs));
        }
    }

    let text = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    // 截断
    if finish == Some("length") {
        return Ok(ModelResponse::Length(text));
    }
    Ok(ModelResponse::Text(text))
}

/// 直接问一句话，用于连通性测试。
pub fn ping(client: &OpenAiClient, prompt: &str) -> anyhow::Result<(String, Usage)> {
    let msgs = vec![serde_json::json!({"role": "user", "content": prompt})];
    let transport = Transport::new(
        &client.base_url,
        &client.api_key,
        &client.model,
        None,
    )?
    .with_extra_body(serde_json::json!({ "max_tokens": 64 }));
    let end = transport.stream_chat(msgs, |_| {})?;
    let usage = end
        .usage
        .map(|u| Usage {
            prompt_tokens: Some(u.prompt_tokens),
            completion_tokens: Some(u.completion_tokens),
            cached_tokens: u.cached_tokens,
        })
        .unwrap_or_default();
    Ok((end.text.unwrap_or_default(), usage))
}
