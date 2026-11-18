// A minimal reference plugin: one tool, one event, no state.
//
// Build: go build -o /tmp/example-echo .
// Load:  harnessd serve --engine <engine> --plugin /tmp/example-echo
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strings"

	"hiq/sdk/go/harnessplugin"
)

func main() {
	err := harnessplugin.Serve("example-echo",
		harnessplugin.WithTool(harnessplugin.Tool{
			Name:        "echo.run",
			Description: "Echo the input back",
			Handler: func(in json.RawMessage) (any, error) {
				var v any
				if err := json.Unmarshal(in, &v); err != nil {
					return nil, err
				}
				return v, nil
			},
		}),
		harnessplugin.WithTool(harnessplugin.Tool{
			Name:        "policy.check",
			Description: "Refuse destructive shell commands",
			Handler: func(in json.RawMessage) (any, error) {
				var p struct{ Command string `json:"command"` }
				_ = json.Unmarshal(in, &p)
				if strings.Contains(p.Command, "rm -rf /") {
					return harnessplugin.Deny("refuses to delete the filesystem root"), nil
				}
				return harnessplugin.Allow(), nil
			},
		}),
		harnessplugin.WithEvent("turn.start", func(raw json.RawMessage) {
			fmt.Fprintf(os.Stderr, "turn started: %s\n", raw)
		}),
	)
	if err != nil {
		fmt.Fprintln(os.Stderr, "plugin:", err)
		os.Exit(1)
	}
}
