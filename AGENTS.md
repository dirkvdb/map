Maintain readable, modular, and maintainable Rust code. Use **Clippy** and **Knots** to enforce measurable code quality and complexity constraints.

### Complexity thresholds

| Metric | Maximum |
|---|---:|
| Cognitive complexity | 15 |
| Cyclomatic complexity (McCabe) | 10 |
| Function length | 80 lines |
| Nesting depth | 4 |
| Function arguments | 7 |

### Coding guidelines

- Prefer small, cohesive functions with clear responsibilities.
- Use early returns, pattern matching, and idiomatic Rust to reduce nesting.
- Extract reusable functionality when it improves readability.
- Avoid unnecessary abstractions and excessive fragmentation.
- Do not split functions artificially just to satisfy complexity metrics.
- Do not suppress lint warnings or exclude code from analysis merely to pass checks.
- Preserve existing behavior when refactoring.

### Required validation

After modifying Rust code, run:

**1. Formatting**

```fish
cargo fmt --all -- --check
```

**2. Clippy**

```fish
cargo clippy --workspace --all-targets --all-features -- \
    -D warnings \
    -D clippy::cognitive_complexity \
    -D clippy::too_many_lines \
    -D clippy::type_complexity
```

Respect the thresholds configured in `clippy.toml`.

**3. Knots**

```fish
devenv tasks run code:complexity
```

Use the project's configured Knots thresholds and baseline. Do not override thresholds to bypass violations.

**4. Tests**

```fish
cargo test --workspace --all-features
```

### Complexity regression policy

- New functions must satisfy the configured complexity thresholds.
- Modified functions must not introduce new threshold violations.
- Existing violations may remain when unrelated to the current task.
- When modifying an already complex function, prefer reducing its complexity.
- Do not introduce additional complexity debt.
- Update the Knots baseline only when explicitly justified and approved.
- Complexity measurements must not take precedence over correctness, safety, or meaningful abstraction boundaries.

### Agent workflow

1. Inspect the relevant code and existing complexity before making changes.
2. Implement the requested functionality using idiomatic Rust.
3. Run formatting, Clippy, Knots, and relevant tests.
4. Refactor newly introduced complexity violations.
5. Repeat validation until all applicable checks pass.
6. Report any pre-existing or unresolved violations.

### Completion criteria

A task is complete only when:

- The requested functionality works correctly.
- Formatting and Clippy checks pass.
- Knots reports no new complexity violations.
- Relevant tests pass.
- No complexity checks were bypassed or disabled.

In the final response, summarize the changes, validation results, and any remaining complexity concerns.
