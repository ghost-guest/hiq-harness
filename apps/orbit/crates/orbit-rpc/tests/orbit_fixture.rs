//! 把**真跑**留下的一整段输出喂回 orbit 自己的解析器。
//!
//! 这不是人眼看界面，但它比「协议测试通过」更接近界面：只要这里解析不出
//! 文本增量和工具执行事件，界面就一定是空的。人眼那一步仍然要人来点。

use orbit_rpc::types::Event;
use std::path::PathBuf;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../bench/fixtures/orbit-session.ndjson")
}

#[test]
fn a_real_session_parses_into_the_events_the_ui_renders() {
    let path = fixture();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()));

    let mut text_deltas = 0;
    let mut assistant_chars = 0;
    let mut tool_starts = 0;
    let mut tool_ends = 0;
    let mut agent_end = 0;
    let mut responses = 0;
    let mut unknown = 0;

    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match Event::parse_line(line) {
            Event::MessageUpdate { assistant, .. } => {
                if let Some(orbit_rpc::types::AssistantMessageEvent::TextDelta { delta, .. }) = assistant {
                    text_deltas += 1;
                    assistant_chars += delta.chars().count();
                }
            }
            Event::ToolExecutionStart { .. } => tool_starts += 1,
            Event::ToolExecutionEnd { .. } => tool_ends += 1,
            Event::AgentEnd { .. } => agent_end += 1,
            Event::Response { .. } => responses += 1,
            Event::Unknown { .. } => unknown += 1,
            _ => {}
        }
    }

    assert_eq!(responses, 1, "the prompt must get exactly one reply envelope");
    assert!(text_deltas >= 1 && assistant_chars > 0, "UI would show no assistant text");
    assert_eq!(tool_starts, tool_ends, "every started tool call must also finish");
    assert!(tool_ends >= 1, "UI would show no tool calls at all");
    assert_eq!(agent_end, 1, "UI would never see the turn end");
    // 未知事件允许存在（引擎会加新种类），但不能淹没认真件。
    assert!(unknown <= 20, "too many unparsed events: {unknown}");
}
