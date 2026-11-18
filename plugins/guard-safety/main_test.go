package main

import "testing"

func TestCheckCommand(t *testing.T) {
	p := Policy{}
	cases := []struct {
		cmd    string
		denied bool
		rule   string
	}{
		{"git push origin main", false, ""},
		{"git push --force origin main", true, "force-push"},
		{"git push -f origin main", true, "force-push"},
		{"git push --force-with-lease", true, "force-push"},
		{"echo hi > .env", true, "dotenv-write"},
		{"cat .env", false, ""},
		{"printf 'A=1' | tee .env.local", true, "dotenv-write"},
		{"sed -i 's/A/B/' .env", true, "dotenv-write"},
		{"rm -rf /tmp/scratch", false, ""},
		{"rm -rf /", true, "root-delete"},
		{":(){ :|:& };:", true, "fork-bomb"},
		{"cargo test --workspace", false, ""},
	}
	for _, c := range cases {
		got, denied := p.checkCommand(c.cmd)
		if denied != c.denied {
			t.Fatalf("%q: denied=%v, want %v", c.cmd, denied, c.denied)
		}
		if denied && got.ID != c.rule {
			t.Fatalf("%q: rule=%s, want %s", c.cmd, got.ID, c.rule)
		}
	}
}

func TestExplainShape(t *testing.T) {
	p := Policy{}
	deny := p.explain("git push --force")
	if deny["decision"] != "deny" || deny["reason"] == "" {
		t.Fatalf("deny must carry a reason: %v", deny)
	}
	allow := p.explain("ls")
	if allow["decision"] != "allow" {
		t.Fatalf("allow got %v", allow)
	}
}

func TestDisabledRuleStopsApplying(t *testing.T) {
	p := Policy{cfg: Config{Disabled: []string{"force-push"}}}
	if _, denied := p.checkCommand("git push --force origin main"); denied {
		t.Fatal("a disabled rule still denied")
	}
	// 其余规则不受影响。
	if _, denied := p.checkCommand("rm -rf /"); !denied {
		t.Fatal("disabling one rule turned off another")
	}
}

func TestProtectedEnvExtendsTheDotenvRule(t *testing.T) {
	p := Policy{cfg: Config{ProtectedEnv: []string{"secrets.yaml"}}}
	if _, denied := p.checkCommand("echo x > secrets.yaml"); !denied {
		t.Fatal("protected_env was not enforced")
	}
	if _, denied := p.checkCommand("echo x > notes.yaml"); denied {
		t.Fatal("an unprotected file was blocked")
	}
}

func TestLoadConfigKeepsAllRulesOnWhenFileIsMissing(t *testing.T) {
	t.Setenv("GUARD_SAFETY_CONFIG", "/nope/does-not-exist.json")
	cfg := loadConfig()
	for _, r := range rules {
		if !cfg.enabled(r.ID) {
			t.Fatalf("%s was disabled by a missing config", r.ID)
		}
	}
}
