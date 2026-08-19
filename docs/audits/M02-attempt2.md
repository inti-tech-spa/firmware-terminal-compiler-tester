# M02 audit — attempt 2

Verdict: **REJECTED**

- Audited commit: `373134030d01821f4b64b51bc3cf5d5abec47488`
- Audit task identity: `/root/audit_plan`
- Environment: macOS 26.5.2 (25F84), Apple Silicon arm64
- Rust: rustc/cargo 1.97.1
- cargo-deny: 0.20.2
- cargo-audit: 0.22.2

## Remediation verification

- **PASS:** Cooperative setup cancellation covers downloading, hashing,
  decompression, extraction, copying, executable validation, and inventory;
  SIGINT returns 130 and removes the partial download.
- **PASS:** Explicit system-tool doctor loads `samdebug.toml`, validates all six
  absolute tool paths, and reports the reproducibility loss.
- **PASS:** Hard-link materialization consumes the extraction quota; safe,
  external, missing, and chained cases are tested.
- **PASS:** Schema-v2 install markers inventory every installed file by size and
  SHA-256; tampering is detected and repaired from verified cache.
- **PASS:** A visible probe without verified managed OpenOCD reports
  `openocd_unavailable`.
- **PASS:** Formatting, strict Clippy, 38 routine tests, both real-bundle tests,
  cargo-deny, and cargo-audit.

The local OpenOCD binary, OpenOCD source, and Arm archive hashes match the
embedded manifest.

## Remaining blocker

Both pinned OpenOCD release URLs return HTTP 404, the production manifest
remains `installable: false`, and production setup returns exit 3 with
`TOOL_MANIFEST_DISABLED`. Consequently, clean online production setup and
verified cached offline reuse cannot yet be accepted.

## Required remediation

Publish the exact binary and source archives at the pinned URLs, download both
back and verify their hashes, enable and commit the production manifest, then
independently run fresh online setup followed by offline cache reuse.

No later milestone may begin until this final M02 gate is independently
approved.
