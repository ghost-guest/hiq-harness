//! hiq-harness 桌面版。
//!
//! 纯 Rust + GPUI（Zed 的 GPU 渲染框架），无 webview、无 Node。
//! 界面结构参考 orbit（`crates/orbit-pi/src/app`）：左侧导航 + 主区 + 底部状态。
//! 设计令牌移植自 Zed 的 `ui` crate（经 orbit 传递），见 `theme.rs`。
//!
//! 后端连接走 harness host 的 `/run/stream`（SSE）。

mod sse_client;
mod theme;

use gpui::{
    div, prelude::*, px, size, App, Application, Bounds, Context, FocusHandle, Focusable,
    InteractiveElement, KeyDownEvent, SharedString, StatefulInteractiveElement, WindowBounds,
    WindowOptions,
};
use std::sync::{Arc, Mutex};
use theme::{color, Radius, Space, Text};

// ─────────────────────────────────────────────────────────────
// 状态
// ─────────────────────────────────────────────────────────────

/// 会话里的一条消息。
#[derive(Debug, Clone)]
enum Entry {
    /// 用户提交的任务
    User(String),
    /// 正在执行的步骤（工具调用等）
    Step(String),
    /// 最终回复
    Answer(String),
    /// 错误
    Error(String),
}

/// 应用状态。
struct App2 {
    /// 后端地址
    base_url: SharedString,
    /// 输入框内容
    input: String,
    /// 对话记录
    entries: Vec<Entry>,
    /// 是否正在执行
    running: bool,
    /// 底部状态文字
    status: SharedString,
    /// 输入框焦点
    focus: FocusHandle,
}

impl Focusable for App2 {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl App2 {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            base_url: "http://127.0.0.1:7788".into(),
            input: String::new(),
            entries: Vec::new(),
            running: false,
            status: "就绪".into(),
            focus: cx.focus_handle(),
        }
    }

    /// 处理按键。
    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "enter" => {
                self.submit(cx);
                return;
            }
            "backspace" => {
                self.input.pop();
            }
            "space" => self.input.push(' '),
            _ => {
                // 普通字符
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ks.modifiers.control && !ks.modifiers.platform {
                        self.input.push_str(ch);
                    }
                }
            }
        }
        cx.notify();
    }

    /// 提交任务：后台线程跑 SSE，事件回灌到 UI。
    fn submit(&mut self, cx: &mut Context<Self>) {
        let task = self.input.trim().to_string();
        if task.is_empty() || self.running {
            return;
        }
        self.input.clear();
        self.running = true;
        self.status = "执行中…".into();
        self.entries.push(Entry::User(task.clone()));

        // 事件队列：SSE 线程写入，UI 定时取出
        let queue: Arc<Mutex<Vec<sse_client::SseEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let q = queue.clone();
        let base = self.base_url.to_string();

        std::thread::spawn(move || {
            let _ = sse_client::stream_task(&base, &task, |ev| {
                if let Ok(mut g) = q.lock() {
                    g.push(ev);
                }
            });
            // 标记结束
            if let Ok(mut g) = q.lock() {
                g.push(sse_client::SseEvent {
                    event: "__end__".into(),
                    data: String::new(),
                });
            }
        });

        // 每 100ms 拉一次队列（GPUI 的做法：用 timer 驱动重绘）
        let queue2 = queue.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;

                let drained: Vec<sse_client::SseEvent> = {
                    let mut g = queue2.lock().unwrap();
                    std::mem::take(&mut *g)
                };

                let mut ended = false;
                let mut sink: Vec<Entry> = Vec::new();
                for ev in drained {
                    match ev.event.as_str() {
                        "start" => sink.push(Entry::Step("开始了".into())),
                        "step" => {
                            if let Some(n) = ev.data_u64("step") {
                                sink.push(Entry::Step(format!("第 {} 步", n + 1)));
                            }
                        }
                        "tool" => {
                            let name = ev.data_str("name").unwrap_or_default();
                            sink.push(Entry::Step(format!("→ {name}")));
                        }
                        "tool_done" => {
                            let name = ev.data_str("name").unwrap_or_default();
                            let ok = ev.data_bool("ok").unwrap_or(false);
                            sink.push(Entry::Step(format!(
                                "  {} {name}",
                                if ok { "✓" } else { "✗" }
                            )));
                        }
                        "guard" => {
                            if let Some(note) = ev.data_str("note") {
                                sink.push(Entry::Step(format!("⚠ {note}")));
                            }
                        }
                        "text" => {
                            if let Some(t) = ev.data_str("text") {
                                sink.push(Entry::Answer(t));
                            }
                        }
                        "error" => {
                            if let Some(e) = ev.data_str("error") {
                                sink.push(Entry::Error(e));
                            }
                        }
                        "done" => {
                            if let Some(n) = ev.data_u64("steps") {
                                sink.push(Entry::Step(format!("完成（{n} 步）")));
                            }
                        }
                        "__end__" => ended = true,
                        _ => {}
                    }
                }

                let has = !sink.is_empty();
                let _ = this.update(cx, |this, cx| {
                    for e in sink {
                        this.entries.push(e);
                    }
                    if ended {
                        this.running = false;
                        this.status = "就绪".into();
                    }
                    if has || ended {
                        cx.notify();
                    }
                });

                if ended {
                    break;
                }
            }
        })
        .detach();

        cx.notify();
    }
}

// ─────────────────────────────────────────────────────────────
// 渲染
// ─────────────────────────────────────────────────────────────

impl Render for App2 {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(color::bg())
            .text_color(color::text())
            .font_family("SF Mono")
            .track_focus(&self.focus)
            .key_context("Input")
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                this.on_key(ev, cx);
            }))
            // 左侧导航
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(200.0))
                    .h_full()
                    .bg(color::panel())
                    .border_r_1()
                    .border_color(color::border())
                    .child(
                        div()
                            .p(px(16.0))
                            .text_size(Text::Base.px())
                            .child("hiq-harness"),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .flex()
                            .flex_col()
                            .gap(Space::Base04.px())
                            .child(nav_item("对话", true))
                            .child(nav_item("事件", false))
                            .child(nav_item("记忆", false))
                            .child(nav_item("SOP", false)),
                    ),
            )
            // 主区
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .h_full()
                    .child(self.render_header())
                    .child(self.render_transcript())
                    .child(self.render_composer(cx)),
            )
    }
}

impl App2 {
    fn render_header(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(Space::Base20.px())
            .py(Space::Base12.px())
            .border_b_1()
            .border_color(color::border())
            .bg(color::panel())
            .child(
                div()
                    .text_size(Text::Large.px())
                    .child("对话"),
            )
            .child(
                div()
                    .text_size(Text::Small.px())
                    .text_color(color::muted())
                    .child(self.base_url.clone()),
            )
    }

    fn render_transcript(&self) -> impl IntoElement {
        let mut col = div()
            .flex()
            .flex_col()
            .flex_1()
            .overflow_hidden()
            .p(Space::Base20.px())
            .gap(Space::Base08.px());

        if self.entries.is_empty() {
            col = col.child(
                div()
                    .text_color(color::muted())
                    .text_size(Text::Small.px())
                    .child("还没有任务。在下方输入后按回车。"),
            );
        } else {
            for e in &self.entries {
                col = col.child(render_entry(e));
            }
        }
        col
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(Space::Base08.px())
            .px(Space::Base20.px())
            .py(Space::Base12.px())
            .border_t_1()
            .border_color(color::border())
            .bg(color::panel())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_1()
                    .px(Space::Base12.px())
                    .py(Space::Base08.px())
                    .rounded(Radius::Medium.px())
                    .border_1()
                    .border_color(color::border())
                    .bg(color::bg())
                    .text_size(Text::Base.px())
                    .child(if self.input.is_empty() {
                        "描述要完成的任务…".to_string()
                    } else {
                        self.input.clone()
                    }),
            )
            .child(
                div()
                    .id("run-btn")
                    .px(Space::Base16.px())
                    .py(Space::Base08.px())
                    .rounded(Radius::Medium.px())
                    .bg(if running {
                        color::muted()
                    } else {
                        color::accent()
                    })
                    .text_size(Text::Base.px())
                    .child(if running { "运行中" } else { "运行" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.submit(cx);
                    })),
            )
    }
}

fn nav_item(label: &str, active: bool) -> impl IntoElement {
    div()
        .px(Space::Base12.px())
        .py(Space::Base08.px())
        .rounded(Radius::Medium.px())
        .bg(if active {
            color::accent()
        } else {
            color::panel()
        })
        .text_size(Text::Small.px())
        .text_color(if active { color::text() } else { color::muted() })
        .child(label.to_string())
}

fn render_entry(e: &Entry) -> impl IntoElement {
    let (label, text, col) = match e {
        Entry::User(t) => ("你", t.clone(), color::accent()),
        Entry::Step(t) => ("", t.clone(), color::muted()),
        Entry::Answer(t) => ("助手", t.clone(), color::ok()),
        Entry::Error(t) => ("错误", t.clone(), color::err()),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .text_size(Text::XSmall.px())
                .text_color(col)
                .child(label.to_string()),
        )
        .child(
            div()
                .text_size(Text::Small.px())
                .text_color(if matches!(e, Entry::Step(_)) {
                    color::muted()
                } else {
                    color::text()
                })
                .child(text),
        )
}

// ─────────────────────────────────────────────────────────────
// 入口
// ─────────────────────────────────────────────────────────────

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(760.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("hiq-harness".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let app = cx.new(|cx| App2::new(cx));
                // 显式聚焦输入区，否则键盘事件不会送达
                let handle = app.read(cx).focus.clone();
                window.focus(&handle);
                app
            },
        )
        .expect("打开窗口失败");
        cx.activate(true);
    });
}
