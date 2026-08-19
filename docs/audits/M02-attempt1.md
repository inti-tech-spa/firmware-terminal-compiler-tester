# M02 audit — attempt 1

Verdict: **REJECTED**

- Audited commit: `6829fa3f666d4d35afeb3aa1d3fd903879fb07e8`
- Audit task identity: `/root/audit_plan`
- Environment: macOS 26.5.2 (25F84), Apple Silicon arm64
- Rust: rustc/cargo 1.97.1
- cargo-deny: 0.20.2
- cargo-audit: 0.22.2

## Evidence

- 33 routine tests passed; both ignored real-bundle installer tests passed.
- Formatting and strict Clippy passed.
- Cargo-deny licenses, bans, sources, and advisories passed with warnings only.
- Cargo-audit scanned 121 dependencies against 1,217 advisories with no findings.
- The manifest parsed and its supplied Draft 2020-12 AJV validation passed.
- OpenOCD binary SHA-256: `60e9601b76f6afb8e8dc00f7eb6b36b2901730cf6ae0a047b14fbe9be3fab011`.
- Corresponding-source SHA-256: `c1d4bf9546e0ad2249b5785c707edf0d67b972186d624fd024ba27751cca88b2`.
- Bundle version, target scripts, declared notices, SPDX SBOM, dynamic linkage, and quarantine state passed inspection.

## Findings

1. The OpenOCD release assets are unpublished, their pinned URLs return 404,
   and the production manifest correctly remains fail-closed.
2. Setup does not consume cancellation during download, hashing, extraction,
   validation, or installation.
3. System-tool configuration is validated syntactically but is not used by
   production doctor or tool resolution.
4. Materialized hard links do not count against the expanded-byte limit.
5. Installed-tree validation trusts a marker, presence, modes, and executable
   versions rather than hashes for every installed file.
6. Doctor can classify a missing OpenOCD executable as a target connection
   failure when exactly one probe is visible.

## Required remediation

- Publish and download-verify both exact OpenOCD assets, enable the manifest,
  and exercise clean online plus cached offline production setup.
- Thread cancellation through the complete setup path and prove SIGINT exit
  130 with prompt cleanup.
- Make the explicit system-tool override operational and validate its tools.
- Charge hard-link copies to the extraction quota and expand hostile-link tests.
- Record and verify installed-tree file hashes, including libraries, scripts,
  and notices.
- Distinguish missing/unverified OpenOCD from target and VTref failures.

No later milestone may begin until remediation is independently approved.
