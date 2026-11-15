//! OpenAI 兼容的模型客户端。
//!
//! 只依赖已经装好的运行时（不引入 reqwest，用 curl 子进程）——
//! 这样能保持零新增依赖，也便于观察原始 HTTP 往返。

use harness_core::agent_loop::{ModelClient, ModelResponse, ToolRequest};
use serde_json::Value;
use std::process::Command;

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

    /// 发一次请求。返回 (响应, 用量)。
    fn post(&self, body: &Value) -> anyhow::Result<(Value, Usage)> {
        let payload = serde_json::to_string(body)?;
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        // 用 curl 避免引入 HTTP 依赖；--max-time 防挂死。
        let out = Command::new("curl")
            .args([
                "-sS",
                "--max-time",
                "300",
                "-H",
                "Content-Type: application/json",
                "-H",
                &format!("Authorization: Bearer {}", self.api_key),
                "-d",
                &payload,
                &url,
            ])
            .output()?;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.status.success() {
            anyhow::bail!("curl 失败: {}", String::from_utf8_lossy(&out.stderr));
        }
        if stdout.trim().is_empty() {
            anyhow::bail!("空响应");
        }
        let v: Value = serde_json::from_str(&stdout).map_err(|e| {
            anyhow::anyhow!(
                "响应不是 JSON: {e}; 原始: {}",
                &stdout[..stdout.len().min(300)]
            )
        })?;

        if let Some(err) = v.get("error") {
            anyhow::bail!("API 错误: {err}");
        }

        let usage = parse_usage(&v);
        Ok((v, usage))
    }
}

fn parse_usage(v: &Value) -> Usage {
    let u = v.get("usage");
    Usage {
        prompt_tokens: u
            .and_then(|x| x.get("prompt_tokens"))
            .and_then(|x| x.as_u64()),
        completion_tokens: u
            .and_then(|x| x.get("completion_tokens"))
            .and_then(|x| x.as_u64()),
        cached_tokens: u
            .and_then(|x| x.get("prompt_tokens_details"))
            .and_then(|x| x.get("cached_tokens"))
            .and_then(|x| x.as_u64()),
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

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": msgs,
        });
        if let Some(effort) = &self.effort {
            body["reasoning"] = serde_json::json!({"effort": effort});
        }

        let (resp, _usage) = self.post(&body)?;
        parse_model_response(&resp)
    }
}

impl OpenAiClient {
    /// 暴露原始 POST，供 runner 直接构造请求（需要拿到 usage）。
    pub fn post_public(&self, body: &Value) -> anyhow::Result<(Value, Usage)> {
        self.post(body)
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
    let body = serde_json::json!({
        "model": client.model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 64,
    });
    let (resp, usage) = client.post(&body)?;
    let text = resp
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    Ok((text, usage))
}
