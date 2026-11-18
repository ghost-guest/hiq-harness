// 诊断插件：把内核广播的每一种事件都记一行到 $EVENT_TAP_LOG。
//
// 用途不是生产，而是回答一个只有跑起来才知道的问题：S4 说"6 个事件"，
// 一次普通回合里**实际**看得到哪几种？接了线不等于发得出来。
package main

import (
	"encoding/json"
	"fmt"
	"os"

	"hiq/sdk/go/harnessplugin"
)

// 内核侧 `broadcast("…")` 出现过的全部种类（crates/harnessd 里 grep 得到）。
var kinds = []string{
	"turn.start",
	"turn.end",
	"tool.pre",
	"tool.post",
	"guard.deny",
	"compact.pre",
}

func main() {
	path := os.Getenv("EVENT_TAP_LOG")
	if path == "" {
		fmt.Fprintln(os.Stderr, "event-tap: EVENT_TAP_LOG is required")
		os.Exit(1)
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		fmt.Fprintln(os.Stderr, "event-tap:", err)
		os.Exit(1)
	}
	defer f.Close()

	opts := make([]harnessplugin.Option, 0, len(kinds)+1)
	for _, k := range kinds {
		kind := k
		opts = append(opts, harnessplugin.WithEvent(kind, func(raw json.RawMessage) {
			// 一行一个 kind：探针只需要"有没有到"，不需要载荷。
			fmt.Fprintf(f, "%s\n", kind)
			_ = f.Sync()
		}))
	}
	// 同时当一道 policy 插件存在，确保内核真的会走插件这条路。
	opts = append(opts, harnessplugin.WithTool(harnessplugin.Tool{
		Name:        "policy.check",
		Description: "always allow",
		Handler: func(json.RawMessage) (any, error) {
			return harnessplugin.Allow(), nil
		},
	}))
	if err := harnessplugin.Serve("event-tap", opts...); err != nil {
		fmt.Fprintln(os.Stderr, "event-tap:", err)
		os.Exit(1)
	}
}
