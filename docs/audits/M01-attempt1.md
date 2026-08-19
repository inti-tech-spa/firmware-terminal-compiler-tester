# M01 Audit — Attempt 1

Verdict: **REJECTED**

- Audited commit: `b95a08b58a0a54536c0552f769c6f300ad093e28`
- Audit task: `/root/audit_plan`
- Environment: macOS 26.5.2 arm64; Rust/Cargo 1.97.1; cargo-deny 0.20.2; cargo-audit 0.22.2

## Evidence

- `cargo test --workspace --locked` passed all 15 tests in an isolated target directory.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` passed.
- `cargo fmt --all --check` passed.
- `cargo-deny check licenses bans sources` passed with non-blocking unused-license and duplicate-version warnings.
- `cargo-audit audit --deny warnings --no-fetch` scanned 72 dependencies against 1,217 advisories with no findings.
- The worktree remained clean during the audit.

## Blocking findings

1. JSON parse-error detection did not recognize `--output=json`; help and version could also emit human output in machine mode.
2. `ChildSupervisor` removed ownership before cleanup and abandoned escalation on the first cleanup error.
3. Cancellation was checked only before dispatch and was not integrated with a running child or tested with SIGINT.
4. Public result/error fields allowed contradictory `ok`, schema-version, category, and exit-code values.

## Required remediation

- Robustly detect every supported JSON option form and return protocol envelopes for JSON help/version/error paths.
- Retain cleanup ownership until confirmed exit, perform best-effort escalation after errors, and test each failure point.
- Wire cancellation into a representative blocking child operation; test SIGINT, exit 130, and reaping.
- Make protocol invariants unrepresentable and test serialized output.

No M2 work may begin until a remediated M1 commit is independently approved.
