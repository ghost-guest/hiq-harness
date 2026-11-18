// A real guard plugin: refuses the four shell commands that most often turn a
// productive agent run into a bad afternoon.
//
// Build: go build -o /tmp/guard-safety .
// Load:  harnessd serve --engine <engine> --plugin /tmp/guard-safety
//
// Design notes
//   - The rules live in checkCommand, a pure function, so they are testable
//     without spawning a plugin host (see main_test.go).
//   - Every rule returns a *reason*, because the kernel writes it into
//     `guard.deny` and the user reads that line when a command is blocked.
//   - `safety.rules` exists so an operator can list what is enforced without
//     reading this file.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strings"

	"hiq/sdk/go/harnessplugin"
)

// Config lets an operator turn a rule off, or protect extra env files,
// without rebuilding the plugin. Path comes from GUARD_SAFETY_CONFIG;
// a missing file means "all rules on", so the zero case is safe.
type Config struct {
	Disabled     []string `json:"disabled"`
	ProtectedEnv []string `json:"protected_env"`
}

func loadConfig() Config {
	path := os.Getenv("GUARD_SAFETY_CONFIG")
	if path == "" {
		return Config{}
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		fmt.Fprintf(os.Stderr, "guard-safety: %s: %v (all rules stay on)\n", path, err)
		return Config{}
	}
	var cfg Config
	if err := json.Unmarshal(raw, &cfg); err != nil {
		fmt.Fprintf(os.Stderr, "guard-safety: %s is not valid JSON: %v (all rules stay on)\n", path, err)
		return Config{}
	}
	return cfg
}

// enabled reports whether a rule still applies. A disabled rule is a
// deliberate operator decision, so it is not an error.
func (c Config) enabled(id string) bool {
	for _, d := range c.Disabled {
		if d == id {
			return false
		}
	}
	return true
}

// Policy is the rule set plus its configuration.
type Policy struct{ cfg Config }

// Rule is one enforced policy, named so a log line can point at it.
type Rule struct {
	ID  string `json:"id"`
	Why string `json:"why"`
}

var rules = []Rule{
	{"force-push", "git push --force rewrites shared history; ask a human first"},
	{"dotenv-write", "editing .env writes live secrets and is never what the task meant"},
	{"root-delete", "deleting the filesystem root ends the machine, not the task"},
	{"fork-bomb", "a fork bomb takes down the host for everyone"},
}

// tokens splits a shell line on whitespace after dropping quotes, which is
// enough to see flags like `-f` without pretending to be a shell parser.
func tokens(cmd string) []string {
	repl := strings.NewReplacer("\"", " ", "'", " ", "\t", " ", "\n", " ")
	return strings.Fields(repl.Replace(cmd))
}

func hasToken(toks []string, want string) bool {
	for _, t := range toks {
		if t == want {
			return true
		}
	}
	return false
}

// writesTo reports whether the command is trying to *write* a path rather than
// read it: redirection, tee, sed -i, cp/mv whose destination is that path.
func writesTo(cmd string, toks []string, path string) bool {
	if strings.Contains(cmd, "> "+path) || strings.Contains(cmd, ">"+path) {
		return true
	}
	for i, t := range toks {
		switch t {
		case "tee":
			if i+1 < len(toks) && toks[i+1] == path {
				return true
			}
		case "-i":
			if i > 0 && (toks[i-1] == "sed" || toks[i-1] == "perl") && strings.Contains(cmd, path) {
				return true
			}
		case "cp", "mv":
			// Destination is the last token.
			if i+1 < len(toks) && toks[len(toks)-1] == path {
				return true
			}
		}
	}
	return false
}

// checkCommand is the whole policy: deny plus a human-readable reason, or a
// zero Rule when the command passes.
func (p Policy) checkCommand(cmd string) (Rule, bool) {
	toks := tokens(cmd)
	lower := strings.ToLower(cmd)

	// force push: `git push` anywhere, with a force flag.
	if p.cfg.enabled("force-push") && hasToken(toks, "push") && (hasToken(toks, "--force") || hasToken(toks, "-f") || hasToken(toks, "--force-with-lease")) {
		for _, t := range toks {
			if t == "git" {
				return rules[0], true
			}
		}
	}
	// dotenv writes (`.env`, `.env.local`, …) plus anything the operator
	// listed in protected_env.
	protected := append([]string{".env"}, p.cfg.ProtectedEnv...)
	for _, t := range toks {
		if p.cfg.enabled("dotenv-write") && matchesProtected(t, protected) {
			base := t[strings.LastIndex(t, "/")+1:]
			if writesTo(cmd, toks, base) || writesTo(cmd, toks, t) {
				return rules[1], true
			}
		}
	}
	if p.cfg.enabled("root-delete") && strings.Contains(cmd, "rm -rf /") && !strings.Contains(cmd, "rm -rf /tmp") && !strings.Contains(cmd, "rm -rf /var") {
		return rules[2], true
	}
	if p.cfg.enabled("fork-bomb") && (strings.Contains(lower, ":(){") || strings.Contains(lower, ":(){ :|:& };:")) {
		return rules[3], true
	}
	return Rule{}, false
}

// matchesProtected reports whether a token names a protected env file,
// with or without a directory in front of it.
func matchesProtected(token string, protected []string) bool {
	base := token[strings.LastIndex(token, "/")+1:]
	for _, p := range protected {
		// `.env` also covers `.env.local`, `.env.production` — the suffix form
		// is how dotenv files are actually named.
		if token == p || base == p || strings.HasPrefix(base, p+".") {
			return true
		}
	}
	return false
}

// explain is the tool body; kept separate so the test can call it directly.
func (p Policy) explain(cmd string) map[string]any {
	if rule, denied := p.checkCommand(cmd); denied {
		return map[string]any{"decision": "deny", "reason": rule.Why, "rule": rule.ID}
	}
	return map[string]any{"decision": "allow"}
}

func main() {
	policy := Policy{cfg: loadConfig()}
	err := harnessplugin.Serve("guard-safety",
		harnessplugin.WithTool(harnessplugin.Tool{
			Name:        "policy.check",
			Description: "Refuse force-pushes, .env writes, root deletes and fork bombs",
			Handler: func(in json.RawMessage) (any, error) {
				var p struct {
					Command string `json:"command"`
				}
				_ = json.Unmarshal(in, &p)
				return policy.explain(p.Command), nil
			},
		}),
		harnessplugin.WithTool(harnessplugin.Tool{
			Name:        "safety.rules",
			Description: "List the enforced rules",
			Handler: func(json.RawMessage) (any, error) {
				active := make([]Rule, 0, len(rules))
				for _, r := range rules {
					if policy.cfg.enabled(r.ID) {
						active = append(active, r)
					}
				}
				return map[string]any{"rules": active, "protected_env": policy.cfg.ProtectedEnv}, nil
			},
		}),
		harnessplugin.WithEvent("guard.deny", func(raw json.RawMessage) {
			// A denial of our own making is worth a line: it explains itself.
			fmt.Fprintf(os.Stderr, "guard-safety watched a denial: %s\n", raw)
		}),
	)
	if err != nil {
		fmt.Fprintln(os.Stderr, "plugin:", err)
		os.Exit(1)
	}
}
