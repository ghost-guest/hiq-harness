// Package main 是示例插件，演示 Rust 宿主如何通过 JSON-RPC over stdio 调用 Go 插件。
//
// 为什么用进程边界而不是 FFI：崩溃隔离、语言无关、协议可测。
// 这是 kiso 和 Codewhale 的做法。
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
)

// Request 是宿主发来的一次调用。
type Request struct {
	ID     int                    `json:"id"`
	Method string                 `json:"method"`
	Params map[string]interface{} `json:"params,omitempty"`
}

// Response 是回给宿主的结果。
type Response struct {
	ID     int         `json:"id"`
	Result interface{} `json:"result,omitempty"`
	Error  string      `json:"error,omitempty"`
}

// Tool 是一个可被模型调用的工具。
type Tool struct {
	Name        string `json:"name"`
	Description string `json:"description"`
	// ReadOnly 决定它能否与其它只读工具并行。
	ReadOnly bool `json:"read_only"`
}

// plugin 实现五类钩子中的两类（tools + approvals）。
// 完整五类（kiso 的做法）还包括 hooks、compaction、systemPrompt。
type plugin struct {
	name  string
	tools []Tool
}

func newPlugin() *plugin {
	return &plugin{
		name: "example",
		tools: []Tool{
			{Name: "echo", Description: "原样返回输入", ReadOnly: true},
			{Name: "now", Description: "返回当前时间戳", ReadOnly: true},
		},
	}
}

func (p *plugin) handle(req Request) Response {
	switch req.Method {
	case "describe":
		return Response{ID: req.ID, Result: map[string]interface{}{
			"name":  p.name,
			"tools": p.tools,
		}}

	case "tool.call":
		name, _ := req.Params["name"].(string)
		input, _ := req.Params["input"].(string)
		switch name {
		case "echo":
			return Response{ID: req.ID, Result: map[string]interface{}{"output": input}}
		case "now":
			return Response{ID: req.ID, Result: map[string]interface{}{
				"output": fmt.Sprintf("%d", nowMillis()),
			}}
		default:
			return Response{ID: req.ID, Error: "unknown tool: " + name}
		}

	case "approval.decide":
		// 策略链单调：deny > allow > ask。
		// 这里演示最保守的做法 —— 无法判定时返回 ask，交给人。
		name, _ := req.Params["tool"].(string)
		switch name {
		case "echo", "now":
			return Response{ID: req.ID, Result: map[string]interface{}{"verdict": "allow"}}
		default:
			return Response{ID: req.ID, Result: map[string]interface{}{
				"verdict": "ask",
				"reason":  "未识别的工具，交给人决定",
			}}
		}

	default:
		return Response{ID: req.ID, Error: "unknown method: " + req.Method}
	}
}

func main() {
	p := newPlugin()
	sc := bufio.NewScanner(os.Stdin)
	sc.Buffer(make([]byte, 0, 1024*1024), 8*1024*1024)
	w := bufio.NewWriter(os.Stdout)
	defer w.Flush()

	for sc.Scan() {
		line := sc.Bytes()
		if len(line) == 0 {
			continue
		}
		var req Request
		if err := json.Unmarshal(line, &req); err != nil {
			// 坏输入必须报错，不能静默丢弃。
			out, _ := json.Marshal(Response{Error: "bad request: " + err.Error()})
			w.Write(out)
			w.WriteByte('\n')
			w.Flush()
			continue
		}
		resp := p.handle(req)
		out, err := json.Marshal(resp)
		if err != nil {
			continue
		}
		w.Write(out)
		w.WriteByte('\n')
		w.Flush()
	}
}

func nowMillis() int64 {
	return timeNow().UnixMilli()
}
