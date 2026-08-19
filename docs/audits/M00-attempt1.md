# M0 audit — attempt 1

- Reviewed commit: `b4000006d38fb47cdbad34904982d57d1acf04b0`
- Auditor task: `/root/audit_plan`
- Result: `REJECTED`

The independent auditor passed the architecture boundaries and exit-code
taxonomy, but rejected the milestone because schemas did not enforce the prose
contracts, the debug transition model and authorization lifecycle were
incomplete, CMSIS/DFP handling contradicted the no-bundling policy, write-root
containment lacked canonical/symlink rules, and licensing/source-distribution
policy for M1 was unresolved.

Required remediation was:

1. Enforce message variants, hello/error structures, success/error exclusivity,
   and field policy in both schemas.
2. Add the complete debug transition and command-guard table.
3. Define TUI/agent authorization fields and invalidation.
4. Resolve CMSIS/DFP paths without bundling vendor packs.
5. Constrain generated writes to a canonical `.samdebug` root.
6. Use immutable source identifiers and a concrete release trust model.
7. Choose a GPL corresponding-source distribution mechanism.
8. Add the project and Rust dependency license policies.

REJECTED
