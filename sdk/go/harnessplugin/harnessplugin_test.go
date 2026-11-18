package harnessplugin

import (
	"bytes"
	"encoding/json"
	"io"
	"os"
	"strings"
	"testing"
)

// run drives a plugin through one scripted session and returns its stdout lines.
func run(t *testing.T, stdin string, opts ...Option) []map[string]any {
	t.Helper()
	oldIn, oldOut := os.Stdin, os.Stdout
	defer func() { os.Stdin, os.Stdout = oldIn, oldOut }()

	inR, inW, _ := os.Pipe()
	outR, outW, _ := os.Pipe()
	os.Stdin, os.Stdout = inR, outW
	go func() { io.WriteString(inW, stdin); inW.Close() }()

	done := make(chan error, 1)
	go func() {
		err := Serve("test", opts...)
		outW.Close() // close the write end only after Serve is finished
		done <- err
	}()
	if err := <-done; err != nil {
		t.Fatalf("serve: %v", err)
	}

	raw, _ := io.ReadAll(outR)
	var got []map[string]any
	for _, line := range strings.Split(strings.TrimSpace(string(raw)), "\n") {
		if line == "" {
			continue
		}
		var m map[string]any
		if err := json.Unmarshal([]byte(line), &m); err != nil {
			t.Fatalf("bad frame %q: %v", line, err)
		}
		got = append(got, m)
	}
	return got
}

func TestHelloAdvertisesProtocolAndTools(t *testing.T) {
	got := run(t, `{"id":1,"method":"plugin/hello"}`, WithTool(Tool{
		Name:    "echo.run",
		Handler: func(in json.RawMessage) (any, error) { return in, nil },
	}))
	if len(got) != 1 {
		t.Fatalf("want 1 frame, got %d", len(got))
	}
	res := got[0]["result"].(map[string]any)
	if res["protocol"].(float64) != Protocol {
		t.Fatalf("protocol = %v", res["protocol"])
	}
	tools := res["tools"].([]any)
	if len(tools) != 1 || tools[0] != "echo.run" {
		t.Fatalf("tools = %v", tools)
	}
}

func TestToolCallRoundTripsAndErrors(t *testing.T) {
	in := strings.Join([]string{
		`{"id":1,"method":"tools/call","params":{"tool":"add","input":{"a":2,"b":3}}}`,
		`{"id":2,"method":"tools/call","params":{"tool":"nope","input":{}}}`,
	}, "\n")
	got := run(t, in, WithTool(Tool{
		Name: "add",
		Handler: func(raw json.RawMessage) (any, error) {
			var p struct{ A, B int }
			if err := json.Unmarshal(raw, &p); err != nil {
				return nil, err
			}
			return p.A + p.B, nil
		},
	}))
	if len(got) != 2 {
		t.Fatalf("want 2 frames, got %d", len(got))
	}
	if got[0]["result"].(map[string]any)["content"].(float64) != 5 {
		t.Fatalf("content = %v", got[0]["result"])
	}
	if _, ok := got[1]["error"]; !ok {
		t.Fatalf("unknown tool must be an rpc error: %v", got[1])
	}
}

func TestEventsAreDeliveredAndUnknownOnesIgnored(t *testing.T) {
	var seen bytes.Buffer
	in := strings.Join([]string{
		`{"method":"turn.start","params":{"thread":"t1"}}`,
		`{"method":"something.else","params":{}}`,
		`{"system":"note","params":{}}`,
	}, "\n")
	run(t, in, WithEvent("turn.start", func(raw json.RawMessage) { seen.Write(raw) }))
	if !strings.Contains(seen.String(), "t1") {
		t.Fatalf("event not delivered: %q", seen.String())
	}
}
