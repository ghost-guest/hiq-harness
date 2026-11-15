/**
 * /api/chat —— assistant-ui 的后端适配层。
 *
 * 不做业务逻辑：把 UI 的消息转成 harness 的 /run 请求，
 * 用 ai-sdk 官方的 `createUIMessageStream` 生成合规的 UI 消息流。
 *
 * 这样协议格式由 ai-sdk 保证，我们只写转发。
 */

import { createUIMessageStream, createUIMessageStreamResponse, type UIMessage } from "ai";

export const maxDuration = 300;

/** harness host 的地址。 */
const HARNESS = process.env.HARNESS_URL ?? "http://127.0.0.1:7788";

/** 从 UIMessage 里抽出最后一条用户文本。 */
function lastUserText(messages: UIMessage[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (m.role !== "user") continue;
    const parts = (m as { parts?: { type: string; text?: string }[] }).parts ?? [];
    const text = parts
      .filter((p) => p.type === "text" && typeof p.text === "string")
      .map((p) => p.text as string)
      .join("\n")
      .trim();
    if (text) return text;
    const content = (m as { content?: string }).content;
    if (typeof content === "string" && content.trim()) return content.trim();
  }
  return "";
}

/** 用 ai-sdk 的官方 API 构造流式响应。 */
function streamText(text: string): Response {
  const stream = createUIMessageStream({
    execute: async ({ writer }) => {
      const id = "t0";
      writer.write({ type: "text-start", id });
      writer.write({ type: "text-delta", id, delta: text });
      writer.write({ type: "text-end", id });
    },
  });
  return createUIMessageStreamResponse({ stream });
}

export async function POST(req: Request) {
  let messages: UIMessage[] = [];
  try {
    const body = await req.json();
    messages = body.messages ?? [];
  } catch {
    return new Response("请求体不是合法 JSON", { status: 400 });
  }

  const task = lastUserText(messages);
  if (!task) {
    return streamText("（没有收到任务描述）");
  }

  try {
    const res = await fetch(`${HARNESS}/run`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ task }),
    });

    const data = (await res.json().catch(() => ({}))) as Record<string, unknown>;

    if (res.status === 409) {
      const pending = (data.pending as string[] | undefined) ?? [];
      return streamText(
        `存在未决副作用，需先裁决：\n${pending.map((p) => `- ${p}`).join("\n")}\n\n` +
          `在终端执行：harness resolve <id> --verdict rerun|abandon`,
      );
    }

    if (!res.ok) {
      const err = (data.error as string | undefined) ?? `HTTP ${res.status}`;
      return streamText(`任务执行失败：${err}`);
    }

    const output = (data.output as string | undefined) ?? "(无输出)";
    const steps = (data.steps as number | undefined) ?? 0;
    const cache = data.cache as { steady_hit_rate?: number } | undefined;
    const effort = data.effort as
      | { requested?: string; effective?: string }
      | undefined;

    const meta: string[] = [];
    if (steps) meta.push(`${steps} 步`);
    if (cache?.steady_hit_rate !== undefined) {
      meta.push(`缓存命中 ${(cache.steady_hit_rate * 100).toFixed(0)}%`);
    }
    if (effort?.requested && effort.requested !== effort.effective) {
      meta.push(`档位 ${effort.requested}→${effort.effective}`);
    }

    const suffix = meta.length ? `\n\n---\n_${meta.join(" · ")}_` : "";
    return streamText(output + suffix);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    return streamText(
      `无法连接到 harness 后端（${HARNESS}）：${msg}\n\n` +
        `请先在终端运行：harness serve --port 7788`,
    );
  }
}
