// Package harnessplugin is the Go side of harnessd's plugin seam.
//
// A plugin is one process, one stdio pipe, one protocol version. It may expose
// tools and observe six events. It cannot write the kernel's log, cannot widen
// policy, and its crash does not take the kernel down — those are enforced by
// harnessd, and this package's job is to make the compliant path the easy one.
package harnessplugin

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"sync"
)

// Protocol is the only version this SDK speaks. harnessd refuses to load a
// plugin that answers with anything else, on purpose: a silently downgraded
// plugin is a plugin whose behaviour nobody can predict.
const Protocol = 1

// Events a plugin may subscribe to. Kept to six so plugins never end up
// encoding kernel policy.
var Events = []string{"tool.pre", "tool.post", "turn.start", "turn.end", "compact.pre", "guard.deny"}

// Tool is one callable capability.
type Tool struct {
	Name        string
	Description string
	Handler     func(input json.RawMessage) (any, error)
}

type request struct {
	ID     *uint64         `json:"id,omitempty"`
	Method string          `json:"method"`
	Params json.RawMessage `json:"params"`
}

type response struct {
	ID     uint64      `json:"id"`
	Result any         `json:"result,omitempty"`
	Error  *rpcError   `json:"error,omitempty"`
}

type rpcError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

type plugin struct {
	name    string
	tools   []Tool
	onEvent map[string]func(json.RawMessage)
	mu      sync.Mutex
	out     *bufio.Writer
}

// On registers an event handler. Unknown names are ignored rather than fatal:
// subscribing to an event a newer kernel does not emit should not break a load.
func (p *plugin) On(event string, fn func(json.RawMessage)) {
	for _, e := range Events {
		if e == event {
			p.onEvent[event] = fn
			return
		}
	}
}

// Serve runs the plugin loop until stdin closes. It returns nil on a clean EOF.
func Serve(name string, opts ...Option) error {
	p := &plugin{name: name, onEvent: map[string]func(json.RawMessage){}, out: bufio.NewWriter(os.Stdout)}
	for _, o := range opts {
		o(p)
	}

	in := bufio.NewScanner(os.Stdin)
	in.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)
	for in.Scan() {
		line := in.Bytes()
		if len(line) == 0 {
			continue
		}
		var req request
		if err := json.Unmarshal(line, &req); err != nil {
			continue // malformed frame from the kernel: ignore, never die
		}
		if req.Method == "" {
			continue
		}
		if req.ID == nil {
			if fn, ok := p.onEvent[req.Method]; ok {
				fn(req.Params)
			}
			continue
		}
		p.dispatch(*req.ID, req.Method, req.Params)
	}
	return in.Err()
}

// Option configures a plugin before it serves.
type Option func(*plugin)

// WithTool registers a callable tool.
func WithTool(t Tool) Option {
	return func(p *plugin) { p.tools = append(p.tools, t) }
}

// WithEvent subscribes to one of the six events.
func WithEvent(event string, fn func(json.RawMessage)) Option {
	return func(p *plugin) { p.On(event, fn) }
}

func (p *plugin) dispatch(id uint64, method string, params json.RawMessage) {
	switch method {
	case "plugin/hello":
		names := make([]string, 0, len(p.tools))
		for _, t := range p.tools {
			names = append(names, t.Name)
		}
		p.reply(response{
			ID: id,
			Result: map[string]any{
				"name":     p.name,
				"protocol": Protocol,
				"tools":    names,
				"caps":     []string{}, // never ask for policy.bypass
			},
		})
	case "tools/call":
		var call struct {
			Tool  string          `json:"tool"`
			Input json.RawMessage `json:"input"`
		}
		if err := json.Unmarshal(params, &call); err != nil {
			p.fail(id, -32602, "bad params: "+err.Error())
			return
		}
		for _, t := range p.tools {
			if t.Name != call.Tool {
				continue
			}
			out, err := t.Handler(call.Input)
			if err != nil {
				p.fail(id, -32000, err.Error())
				return
			}
			p.reply(response{ID: id, Result: map[string]any{"content": out}})
			return
		}
		p.fail(id, -32601, "unknown tool "+call.Tool)
	default:
		p.fail(id, -32601, "unknown method "+method)
	}
}

func (p *plugin) reply(r response) {
	p.mu.Lock()
	defer p.mu.Unlock()
	b, err := json.Marshal(r)
	if err != nil {
		return
	}
	p.out.Write(b)
	p.out.WriteByte('\n')
	p.out.Flush()
}

func (p *plugin) fail(id uint64, code int, msg string) {
	p.reply(response{ID: id, Error: &rpcError{Code: code, Message: msg}})
}

// Uncertain marks a handler result as an effect whose completion could not be
// observed. harnessd turns this into a blocking receipt instead of retrying.
func Uncertain(detail string) any {
	return map[string]any{"status": "uncertain", "detail": detail}
}

// Deny is the shape a policy tool returns when it wants a command refused.
func Deny(reason string) any {
	return map[string]any{"decision": "deny", "reason": reason}
}

// Allow is the counterpart of Deny.
func Allow() any { return map[string]any{"decision": "allow"} }

// Errorf exists so example plugins do not import fmt for one line each.
func Errorf(format string, args ...any) error { return fmt.Errorf(format, args...) }
