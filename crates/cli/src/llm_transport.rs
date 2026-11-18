//! LLM 传输层：把一次对话变成「流式事件 + 结束汇总」。
//!
//! 基于 **aimux**（MIT，crates.io 0.6.1）—— dim（arcships/dim）生产环境使用
//! 的模型传输层。选它而不是继续手写 curl / 自己包 SDK 的原因：
//!
//! - 一个抽象同时覆盖 OpenAI Chat Completions、OpenAI Responses、
//!   Anthropic Messages、Gemini GenerateContent。换模型或换家供应商不用
//!   重写传输层。
//! - 已发布到 crates.io，不需要 fork dim 的 67 万行，也不会被上游锁定。
//! - 事件是强类型的：`finish_reason` 是枚举，usage 有
//!   `input_token_details.cache_read_tokens` 这样的具名字段，不必自己
//!   从 JSON 里抠字符串。
//!
//! **对外契约不变**：`StreamChunk` / `StreamEnd` / `Usage` / `ToolCallFull`
//! 的字段语义与之前一致，调用方（`llm.rs`、`runner.rs`）无需改动。
//!
//! 同步 API、内部起 current-thread runtime：调用方本来就是同步的，
//! 不把 async 传染出去。

use std::time::Duration;

use aimux_core::generate::{stream_text, GenerateTextOptions};
use aimux_core::message::{ModelMessage, ModelPrompt};
use aimux_core::options::Tool;
use aimux_core::prelude::TextStreamPart;
use aimux_core::provider::Provider as _;
use aimux_core::tool::FunctionTool;
use aimux_providers::openai::{create_openai, OpenAIProviderSettings};
use futures::StreamExt;
use serde_json::Value;

/// 流式过程中产生的事件。
///
/// 与 `RunEvent` 分开：这是传输层（模型吐什么），`RunEvent` 是任务层
/// （agent 在做什么），两者不是一回事。
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 正文增量（已是完整 UTF-8 字符串）。
    Text(String),
    /// 推理过程增量（部分模型有，非 OpenAI 标准字段）。
    Reasoning(String),
    /// 工具调用名/ID 已知（参数仍在累积）。
    ///
    /// aimux 在流结束时才给出解析好的完整参数，所以这里只报「要调哪个
    /// 工具」；完整参数在 `StreamEnd::tool_calls` 里。
    ToolCallStart { id: String, name: String },
    /// 正常结束（流自然收尾）。
    End(StreamEnd),
    /// 结束但流不完整 —— 上游可能截断。
    ///
    /// 必须与 `End` 区分，否则「被截断的响应」看起来和「正常结束」一样。
    ClosedWithoutDone,
}

/// 一轮流式结束时的汇总。
#[derive(Debug, Clone, Default)]
pub struct StreamEnd {
    /// 累积的正文。
    pub text: Option<String>,
    /// token 用量。
    pub usage: Option<Usage>,
    /// 结束原因（stop / length / tool_calls …）。
    pub stop_reason: Option<String>,
    /// 聚合后的工具调用（参数已解析成 JSON）。
    pub tool_calls: Vec<ToolCallFull>,
}

/// token 用量。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// 命中前缀缓存的 prompt token 数。
    ///
    /// `None` 表示「服务端没说」，不是 0 —— 首次请求必然没命中，
    /// 服务端会给 0。界面要区分这两者，否则每次新会话都会误报「未上报」。
    pub cached_tokens: Option<u64>,
}

impl Usage {
    /// 缓存命中率（0.0~1.0）。没有 prompt_tokens 时返回 `None`。
    pub fn hit_rate(&self) -> Option<f64> {
        if self.prompt_tokens == 0 {
            return None;
        }
        Some(self.cached_tokens.unwrap_or(0) as f64 / self.prompt_tokens as f64)
    }
}

/// 聚合完成的工具调用（参数已解析成 JSON）。
#[derive(Debug, Clone)]
pub struct ToolCallFull {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// 进程内共享的模型客户端。
///
/// **全进程只要一个**：HTTP 连接池挂在内部 client 上，每次 `new` 一个新
/// client 就等于丢掉连接池、每请求重做 TLS 握手（curl 子进程方案的根本
/// 缺陷之一）。
#[derive(Clone)]
pub struct Transport {
    model: std::sync::Arc<dyn aimux_core::language_model::LanguageModel>,
    model_id: String,
    /// 额外请求字段（如非标准的 `reasoning.effort`）。
    extra_body: Option<Value>,
    effort: Option<String>,
}

impl Transport {
    /// 建一个传输层。
    ///
    /// `base_url` 指向 OpenAI 兼容端点（如 `http://127.0.0.1:3425/v1`）。
    /// 尾斜杠由 aimux 自己处理，这里不用管。
    pub fn new(
        base_url: &str,
        api_key: &str,
        model: &str,
        effort: Option<String>,
    ) -> anyhow::Result<Self> {
        let provider = create_openai(OpenAIProviderSettings {
            base_url: Some(base_url.trim().to_string()),
            api_key: Some(api_key.to_string()),
            ..Default::default()
        })
        .map_err(|e| anyhow::anyhow!("建 provider 失败: {e}"))?;

        let model_handle = provider
            .language_model(model)
            .map_err(|e| anyhow::anyhow!("取模型 {model} 失败: {e}"))?;

        Ok(Self {
            model: model_handle,
            model_id: model.to_string(),
            extra_body: None,
            effort,
        })
    }

    /// 设置额外请求字段（深合并进请求）。
    pub fn with_extra_body(mut self, extra: Value) -> Self {
        self.extra_body = Some(extra);
        self
    }

    /// 设置推理档位（`reasoning.effort`）。
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort;
        self
    }

    pub fn model(&self) -> &str {
        &self.model_id
    }

    /// 发起一次流式请求，把增量逐条交给 `on_chunk`。
    ///
    /// `messages` 是完整的 chat 消息数组（含 system 与历史）。**只追加、
    /// 不改写** —— 服务端的 KV 缓存按完整前缀单元匹配，任何重排都会让
    /// 前面整段缓存作废。
    pub fn stream_chat(
        &self,
        messages: Vec<Value>,
        on_chunk: impl FnMut(StreamChunk),
    ) -> anyhow::Result<StreamEnd> {
        self.stream_chat_inner(messages, Value::Null, on_chunk)
    }

    /// 带工具定义的流式请求。
    ///
    /// `tools` 是 OpenAI 格式的数组（`[{"type":"function","function":{...}}]`）。
    /// 传 `Value::Null` 表示不带工具 —— **不要**传空数组：空 `tools: []`
    /// 和「没有 tools 字段」在部分服务端行为不同，也会改变请求字节。
    pub fn stream_chat_with_tools(
        &self,
        messages: Vec<Value>,
        tools: Value,
        on_chunk: impl FnMut(StreamChunk),
    ) -> anyhow::Result<StreamEnd> {
        self.stream_chat_inner(messages, tools, on_chunk)
    }

    fn stream_chat_inner(
        &self,
        messages: Vec<Value>,
        tools: Value,
        mut on_chunk: impl FnMut(StreamChunk),
    ) -> anyhow::Result<StreamEnd> {
        let prompt = build_prompt(messages)?;
        let mut options = self.build_options(tools)?;

        // 同步 API 内部起一个 current-thread runtime：
        // 调用方（runner）本来就是同步的，不把 async 传染出去。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        rt.block_on(async {
            let mut res = stream_text(self.model.as_ref(), prompt, options)
                .await
                .map_err(|e| anyhow::anyhow!("请求失败: {e}"))?;

            let mut text = String::new();
            let mut thinking = String::new();
            let mut end_out = StreamEnd::default();
            let mut saw_finish = false;
            // 已报过的工具调用 id，避免同一调用重复发 start
            let mut announced: Vec<String> = Vec::new();

            while let Some(item) = res.stream.next().await {
                let part = item.map_err(|e| anyhow::anyhow!("读流失败: {e}"))?;
                match part {
                    TextStreamPart::TextDelta { text: d, .. } => {
                        if !d.is_empty() {
                            text.push_str(&d);
                            on_chunk(StreamChunk::Text(d));
                        }
                    }
                    TextStreamPart::ReasoningDelta { text: delta, .. } => {
                        if !delta.is_empty() {
                            thinking.push_str(&delta);
                            on_chunk(StreamChunk::Reasoning(delta));
                        }
                    }
                    TextStreamPart::ToolCall(tc) => {
                        if !announced.contains(&tc.tool_call_id) {
                            announced.push(tc.tool_call_id.clone());
                            on_chunk(StreamChunk::ToolCallStart {
                                id: tc.tool_call_id.clone(),
                                name: tc.tool_name.clone(),
                            });
                        }
                        end_out.tool_calls.push(ToolCallFull {
                            id: tc.tool_call_id,
                            name: tc.tool_name,
                            arguments: tc.input,
                        });
                    }
                    TextStreamPart::Finish {
                        finish_reason,
                        total_usage,
                        ..
                    } => {
                        saw_finish = true;
                        // aimux 的 finish_reason 是枚举：上游给了未知值时
                        // 也不会让整条流报错（这正是弃用 async-openai 的
                        // 原因之一）。
                        end_out.stop_reason = Some(format!("{finish_reason:?}"));
                        end_out.usage = Some(Usage {
                            prompt_tokens: u64::from(total_usage.input_tokens.unwrap_or(0)),
                            completion_tokens: u64::from(total_usage.output_tokens.unwrap_or(0)),
                            cached_tokens: total_usage
                                .input_token_details
                                .cache_read_tokens
                                .filter(|n| *n > 0)
                                .map(u64::from),
                        });
                    }
                    TextStreamPart::Error { error, .. } => {
                        return Err(anyhow::anyhow!("上游报错: {error}"));
                    }
                    _ => {}
                }
            }

            end_out.text = if text.trim().is_empty() {
                None
            } else {
                Some(text)
            };

            // 判别「正常收尾」还是「被截断」：没收到 Finish 就说明流不完整，
            // 必须让调用方知道，否则截断看起来和完成一样。
            if saw_finish {
                on_chunk(StreamChunk::End(end_out.clone()));
            } else {
                on_chunk(StreamChunk::ClosedWithoutDone);
            }

            Ok(end_out)
        })
    }

    /// 组装请求参数：工具、推理档位、超时。
    fn build_options(&self, tools: Value) -> anyhow::Result<GenerateTextOptions> {
        let mut opts = GenerateTextOptions::default();

        // 工具：OpenAI 格式（`[{"type":"function","function":{...}}]`）
        // → aimux 的 `FunctionTool`。
        if let Some(arr) = tools.as_array() {
            let mut out = Vec::with_capacity(arr.len());
            for t in arr {
                out.push(map_tool(t)?);
            }
            if !out.is_empty() {
                opts.tools = Some(out);
            }
        }

        // 本轮输出上限。`max_tokens` 是 OpenAI 的参数名，aimux 用
        // `max_output_tokens`，语义一致。
        if let Some(n) = self
            .extra_body
            .as_ref()
            .and_then(|v| v.get("max_tokens"))
            .and_then(Value::as_u64)
        {
            opts.max_output_tokens = Some(u32::try_from(n).unwrap_or(u32::MAX));
        }

        // 推理档位：aimux 有类型化的 `reasoning`，但那是它自己的档位枚举
        // （Minimal/Low/Medium/High…），与中继约定的 `reasoning.effort`
        // 字符串不是同一套。这里走 provider_options 原样透传，
        // 保证请求字节与之前完全一致（前缀缓存依赖这一点）。
        if let Some(e) = &self.effort {
            let mut map = aimux_core::shared::SharedProviderOptions::default();
            let mut ns = serde_json::Map::new();
            ns.insert("reasoning".into(), serde_json::json!({ "effort": e }));
            map.insert("openai".to_string(), ns);
            opts.provider_options = Some(map);
        }

        // 超时：默认无超时。首包超时防「连上了但不出数据」，
        // 空闲超时防「流到一半卡死」。两者都不是总时长，不会腰斩长回答。
        opts.timeout = Some(aimux_core::options::TimeoutConfiguration {
            first_chunk_ms: Some(STREAM_HEADER_TIMEOUT.as_millis() as u64),
            chunk_ms: Some(STREAM_READ_TIMEOUT.as_millis() as u64),
            ..Default::default()
        });

        Ok(opts)
    }
}

/// OpenAI 格式的工具定义 → aimux `Tool`。
fn map_tool(t: &Value) -> anyhow::Result<Tool> {
    let f = t.get("function").unwrap_or(t);
    let name = f
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("工具定义缺 name: {t}"))?;
    let schema = f
        .get("parameters")
        .cloned()
        .filter(|s| !s.is_null())
        .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
    Ok(Tool::Function(FunctionTool {
        description: f.get("description").and_then(Value::as_str).map(str::to_owned),
        ..FunctionTool::new(name, schema)
    }))
}

/// 把 `Value` 消息数组转成 aimux 的 `ModelPrompt`。
///
/// 会话文件里存的是 OpenAI 格式（字符串 `content`、`tool_calls`、
/// `tool_call_id`），这里逐条映射。**保持顺序不变**：服务端按完整前缀
/// 匹配缓存单元，重排会让前面整段缓存作废。
fn build_prompt(messages: Vec<Value>) -> anyhow::Result<ModelPrompt> {
    let mut out: Vec<ModelMessage> = Vec::with_capacity(messages.len());

    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");

        match role {
            "system" | "user" => {
                let Some(text) = content_as_text(m.get("content")) else {
                    continue;
                };
                out.push(if role == "system" {
                    ModelMessage::system(text)
                } else {
                    ModelMessage::user(text)
                });
            }
            "assistant" => {
                let text = content_as_text(m.get("content")).unwrap_or_default();
                let calls = m
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .filter(|c| !c.is_empty());

                match calls {
                    // 带工具调用的 assistant：正文与调用要一起回灌。
                    //
                    // aimux 要求 tool 结果消息能找到「前序 tool call」，
                    // 只发正文会把调用记录丢掉，后续 tool 消息就成了孤儿，
                    // 校验直接失败（实测：tool result ... has no tool name
                    // or preceding tool call）。
                    Some(calls) => {
                        use aimux_core::content::AssistantContentPart;
                        let mut parts: Vec<AssistantContentPart> = Vec::new();
                        if !text.is_empty() {
                            parts.push(AssistantContentPart::Text {
                                text,
                                provider_options: None,
                            });
                        }
                        for c in calls {
                            let f = c.get("function").unwrap_or(c);
                            let Some(name) = f.get("name").and_then(Value::as_str) else {
                                continue;
                            };
                            // 参数在会话文件里是字符串（OpenAI 约定），
                            // 这里解析回 JSON；解析不了就传空对象，
                            // 不让一条坏记录毁掉整个请求。
                            let input = f
                                .get("arguments")
                                .and_then(Value::as_str)
                                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                                .or_else(|| f.get("arguments").cloned())
                                .unwrap_or_else(|| serde_json::json!({}));
                            parts.push(AssistantContentPart::ToolCall {
                                tool_call_id: c
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                tool_name: name.to_string(),
                                input,
                                provider_executed: None,
                                provider_options: None,
                            });
                        }
                        if parts.is_empty() {
                            continue;
                        }
                        out.push(ModelMessage::assistant_parts(parts));
                    }
                    None => {
                        if !text.is_empty() {
                            out.push(ModelMessage::assistant(text));
                        }
                    }
                }
            }
            "tool" => {
                // 工具结果：`{role:"tool", tool_call_id, content}`。
                // aimux 用 ToolContent 承载，需要 id 与输出正文。
                let id = m
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let text = content_as_text(m.get("content")).unwrap_or_default();
                out.push(tool_message(id, text));
            }
            // 未知角色跳过而不是报错：上游多给字段不该让整轮失败。
            _ => {}
        }
    }

    if out.is_empty() {
        anyhow::bail!("消息列表为空，无法请求模型");
    }
    Ok(ModelPrompt::Messages(out))
}

/// 构造一条工具结果消息。
///
/// `ToolContent` 是 `Vec<ToolContentPart>` 的别名，每个 part 用
/// `ToolResultPart` 承载调用 id 与输出正文。
fn tool_message(id: &str, text: String) -> ModelMessage {
    use aimux_core::content::{ToolResultOutput, ToolResultPart};
    use aimux_core::message::ToolContent;

    ModelMessage::tool(ToolContent::from(vec![
        aimux_core::content::ToolContentPart::ToolResult(ToolResultPart {
            tool_call_id: id.to_string(),
            tool_name: None,
            output: ToolResultOutput::Text {
                value: text,
                provider_options: None,
            },
            provider_options: None,
        }),
    ]))
}

/// 取出 `content` 的文本。
///
/// 兼容两种形态：纯字符串（OpenAI 标准）与 parts 数组
/// （`[{"type":"text","text":"…"}]`，部分客户端会这么发）。
fn content_as_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut buf = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    buf.push_str(t);
                }
            }
            Some(buf)
        }
        _ => None,
    }
}

/// 保持与旧实现一致的超时常量（供调用方引用）。
pub const STREAM_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
pub const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(180);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_hit_rate() {
        let u = Usage {
            prompt_tokens: 1000,
            completion_tokens: 10,
            cached_tokens: Some(900),
        };
        assert!((u.hit_rate().unwrap() - 0.9).abs() < 1e-9);
        let none = Usage::default();
        assert!(none.hit_rate().is_none());
    }

    #[test]
    fn content_text_accepts_string_and_parts() {
        assert_eq!(
            content_as_text(Some(&serde_json::json!("你好"))).unwrap(),
            "你好"
        );
        assert_eq!(
            content_as_text(Some(&serde_json::json!([
                {"type":"text","text":"a"},
                {"type":"text","text":"b"}
            ])))
            .unwrap(),
            "ab"
        );
    }

    #[test]
    fn build_prompt_keeps_order_and_roles() {
        let msgs = vec![
            serde_json::json!({"role":"system","content":"S"}),
            serde_json::json!({"role":"user","content":"U1"}),
            serde_json::json!({"role":"assistant","content":"A1"}),
            serde_json::json!({"role":"user","content":"U2"}),
        ];
        let p = build_prompt(msgs).unwrap();
        match p {
            ModelPrompt::Messages(v) => {
                assert_eq!(v.len(), 4);
                assert_eq!(v[0].role(), "system");
                assert_eq!(v[1].role(), "user");
                assert_eq!(v[2].role(), "assistant");
                assert_eq!(v[3].role(), "user");
            }
            _ => panic!("应为 Messages"),
        }
    }

    #[test]
    fn build_prompt_keeps_tool_calls() {
        // 回归：assistant 的 tool_calls 曾经被整段丢弃，只剩正文。
        // 这样后续 tool 结果消息就成了孤儿，aimux 校验直接失败：
        //   "tool result <id> has no tool name or preceding tool call"
        let msgs = vec![
            serde_json::json!({"role":"user","content":"列目录"}),
            serde_json::json!({
                "role":"assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name":"list_dir","arguments":"{\"path\":\".\"}"}
                }]
            }),
            serde_json::json!({"role":"tool","tool_call_id":"call_1","content":"a.txt"}),
        ];
        let p = build_prompt(msgs).unwrap();
        match p {
            ModelPrompt::Messages(v) => {
                assert_eq!(v.len(), 3);
                // 第二条必须是 assistant，且带着可回放的工具调用
                assert_eq!(v[1].role(), "assistant");
                let dbg = format!("{:?}", v[1]);
                assert!(
                    dbg.contains("call_1") && dbg.contains("list_dir"),
                    "assistant 消息必须带上 tool_calls，实际：{dbg}"
                );
                assert_eq!(v[2].role(), "tool");
            }
            _ => panic!("应为 Messages"),
        }
    }

    #[test]
    fn build_prompt_tool_message_carries_id_and_text() {
        let msgs = vec![serde_json::json!({
            "role":"tool","tool_call_id":"call_9","content":"结果正文"
        })];
        match build_prompt(msgs).unwrap() {
            ModelPrompt::Messages(v) => {
                let dbg = format!("{:?}", v[0]);
                assert!(dbg.contains("call_9"), "工具结果必须带 call id：{dbg}");
                assert!(dbg.contains("结果正文"), "工具结果必须带正文：{dbg}");
            }
            _ => panic!("应为 Messages"),
        }
    }

    #[test]
    fn build_prompt_rejects_empty() {
        assert!(build_prompt(vec![]).is_err());
        // 只有无法识别的角色 → 也是空
        assert!(build_prompt(vec![serde_json::json!({"role":"x","content":"y"})]).is_err());
    }

    #[test]
    fn map_tool_reads_openai_shape() {
        let t = serde_json::json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "列出目录",
                "parameters": {"type":"object","properties":{"path":{"type":"string"}}}
            }
        });
        let mapped = map_tool(&t).unwrap();
        match mapped {
            Tool::Function(f) => {
                assert_eq!(f.name, "list_dir");
                assert_eq!(f.description.as_deref(), Some("列出目录"));
            }
            _ => panic!("应为函数工具"),
        }
    }

    #[test]
    fn map_tool_rejects_missing_name() {
        let t = serde_json::json!({"type":"function","function":{}});
        assert!(map_tool(&t).is_err());
    }
}
