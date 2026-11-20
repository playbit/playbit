# kvdb

## 1) Project overview

A "local-first" key-value store, written in Rust for the Playbit runtime.

## 2) Engineering Principles (Normative)

These principles are mandatory. They are implementation constraints, not suggestions.

[Playbit Tiger Style](../../docs/tiger-style.md) is normative for this tier A crate.

### 2.1 KISS (Keep It Simple, Stupid)

Required:
- Prefer straightforward control flow over meta-programming.
- Prefer explicit comptime branches and typed structs over hidden dynamic behavior.
- Keep error paths obvious and localized.

### 2.2 YAGNI (You Aren't Gonna Need It)

Required:
- Do not add config features, command line arguments or other features without a concrete caller/user.
- Do not introduce speculative abstractions.
- Keep unsupported paths explicit (panic or return clear error) rather than silent no-ops.

### 2.3 DRY (Don't Repeat Yourself) + Rule of Three ("Three strikes and you refactor")

Required:
- Duplicate small local logic when it preserves clarity.
- Extract shared helpers only after repeated, stable patterns (rule-of-three).
- When extracting, preserve module boundaries and avoid hidden coupling.

### 2.4 Fail fast + Explicit errors

Required:
- Prefer explicit errors for unsupported or unsafe states.
- Never silently broaden permissions or capabilities.

## 3) Agent Workflow (Required)

1. **Read before write** — inspect existing implementation before editing.
2. **Define scope boundary** — one concern per change; avoid mixed feature+refactor+infra patches.
3. **Implement minimal patch** — apply KISS/YAGNI/DRY rule-of-three explicitly.
4. **Test** — Write tests for new features or changes
5. **Incremental** — Take an incremental approach: keep the program working at each step.
6. **Version control**:
    - Before committing changes, run tests that are affected by changes
    - Serialize git index writes: never run `git add`, `git commit`, `git rm`, `git mv`, or similar index-mutating commands in parallel.

## 4) Building and testing

- Always run Cargo tests with `--release`, including targeted tests and
  `--no-run` builds. Keep debug assertions and overflow checks enabled as
  instructed in the [root agent guide](../../AGENTS.md).
- Test: `cargo test --release --locked`
- Sync protocol details in `src/sync.rs`
- Storage contract in `src/storage.rs`
- Keep the codebase small, direct, and easy to audit.
