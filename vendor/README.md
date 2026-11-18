# vendor/

The engine (codewhale) is **not** vendored into this repository. It is consumed as
a process over JSON-RPC (see `docs/KERNEL-FUSION-PLAN.md` §1, depth D1).

If you do vendor it for offline builds:

```bash
git clone https://github.com/codewhale-hq/Codewhale vendor/codewhale
git -C vendor/codewhale checkout $(cat vendor/ENGINE_PIN)
```

`scripts/check-engine.sh` fails the build if `vendor/codewhale` drifts from
`ENGINE_PIN` or has any local modification. That check is the physical guarantee
behind "integration, not fork".
