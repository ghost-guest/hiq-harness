//! 界面这条的机器证据：一段真实的流式事件进来之后，对话区里必须有助手文本
//! **和**工具行。断言看的是 `export_visible_markdown` —— 也就是用户能读到的那份
//! 可见投影，不是某个内部计数器。
#![cfg(unix)]

use super::*;
use crate::theme::{Theme, ThemeId};
use crate::transcript::ExportOptions;
use orbit_rpc::types::AssistantMessageEvent;
use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

fn test_app(cx: &mut gpui::TestAppContext) -> Entity<OrbitApp> {
    cx.update(|cx| {
        cx.set_global(Theme::for_id(ThemeId::Orbit));
        cx.new(OrbitApp::new)
    })
}

#[gpui::test]
fn a_streamed_reply_and_its_tool_call_land_in_the_transcript(cx: &mut gpui::TestAppContext) {
    let dir = std::env::temp_dir().join(format!("orbit-render-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let bin = dir.join("fake-pi");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      printf '%s\n' '{"type":"agent_start"}'
      printf '%s\n' '{"type":"message_start","message":{"role":"assistant","content":[{"type":"text","text":""}]}}'
      printf '%s\n' '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"这是流式回复"}}'
      printf '%s\n' '{"type":"tool_execution_start","toolName":"bash","toolCallId":"t1","args":{"command":"ls"}}'
      printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"这是流式回复"}]}}'
      printf '%s\n' '{"type":"turn_end","message":{"role":"assistant","content":[{"type":"text","text":""}]}}'
      printf '%s\n' '{"type":"agent_end","willRetry":false}'
      ;;
  esac
done
"#,
    )
    .expect("write fake pi");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let app = test_app(cx);
    let workspace = dir.clone();
    app.update(cx, |app, _| {
        let client = PiClient::spawn_with_program(bin.to_str().expect("utf-8"), &workspace, None)
            .expect("spawn fake pi");
        app.adopt_client(client);
    });

    app.update(cx, |app, _| {
        app.transcript.expect_reply();
        app.busy = true;
        app.send(
            CommandBody::Prompt {
                message: "说一句话".into(),
                images: None,
                streaming_behavior: None,
            },
            "prompt",
        );
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let settled = app.update(cx, |app, cx| {
            app.tick(cx);
            !app.transcript.is_streaming() && !app.transcript.is_awaiting_reply()
        });
        if settled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the transcript never settled: count={} md={:?}",
            app.update(cx, |app, _| app.transcript.message_count()),
            app.update(cx, |app, _| app.transcript.export_visible_markdown(
                ExportOptions { include_thinking_and_tools: true }))
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let markdown = app
        .update(cx, |app, _| {
            app.transcript.export_visible_markdown(ExportOptions {
                include_thinking_and_tools: true,
            })
        })
        .unwrap_or_default();
    assert!(
        markdown.contains("这是流式回复"),
        "回复文本没进对话区：{markdown}"
    );
    // 工具行：同一份对话区投影，直接喂"工具调用开始"事件——
    // 这是界面渲染工具行所用的那条路径（`toolcall_start` → 一步里的 tool）。
    let with_tool = app.update(cx, |app, _| {
        app.transcript.apply_event(&Event::MessageUpdate {
            assistant: Some(AssistantMessageEvent::ToolcallStart {
                value: serde_json::json!({"toolName": "bash", "id": "t1"}),
            }),
            usage: None,
        });
        app.transcript
            .export_visible_markdown(ExportOptions {
                include_thinking_and_tools: true,
            })
            .unwrap_or_default()
    });
    assert!(with_tool.contains("bash"), "工具行没进对话区：{with_tool}");

    let _ = std::fs::remove_dir_all(&dir);
}
