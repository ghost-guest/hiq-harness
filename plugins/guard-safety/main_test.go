package main

import "testing"

func TestCheckCommand(t *testing.T) {
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
		got, denied := checkCommand(c.cmd)
		if denied != c.denied {
			t.Fatalf("%q: denied=%v, want %v", c.cmd, denied, c.denied)
		}
		if denied && got.ID != c.rule {
			t.Fatalf("%q: rule=%s, want %s", c.cmd, got.ID, c.rule)
		}
	}
}

func TestExplainShape(t *testing.T) {
	deny := explain("git push --force")
	if deny["decision"] != "deny" || deny["reason"] == "" {
		t.Fatalf("deny must carry a reason: %v", deny)
	}
	allow := explain("ls")
	if allow["decision"] != "allow" {
		t.Fatalf("allow got %v", allow)
	}
}
