---
name: kiss
description: Apply pragmatic Keep It Simple, Stupid (KISS) principles when implementing, refactoring, reviewing, or validating code in any Knots-supported language. Enforce cognitive and cyclomatic complexity, nesting, and function size with Knots; for Rust projects also run rustfmt, Clippy, and tests. Use across repositories without requiring devenv tasks, custom scripts, or clippy.toml.
---

# KISS: maintainable code with measurable complexity

Make the smallest coherent change that solves the task. Prefer clear control flow, focused responsibilities, and existing project conventions over unnecessary layers of abstraction.

## Policy

- Preserve behavior unless the task requests a change; test relevant behavior.
- Prefer descriptive names, early exits, and straightforward decomposition. Avoid speculative frameworks, needless indirection, and tiny helper functions written merely to lower scores.
- Inspect existing code and complexity first. Avoid introducing new threshold violations or worsening existing complexity debt.
- Never silently suppress warnings, lower limits, exclude relevant files, or update a complexity baseline to pass checks.
- Prioritize correctness and maintainability if a numeric target conflicts with good design; disclose and justify exceptions.
- Do not add custom devenv tasks, devenv scripts, `clippy.toml`, or analysis wrapper scripts. Do not edit tooling configuration unless explicitly requested.

## Portable default thresholds

| Metric | Maximum | Analyzer |
| --- | ---: | --- |
| Cognitive complexity | 15 | Knots |
| McCabe cyclomatic complexity | 10 | Knots |
| Nesting depth | 4 | Knots |
| Function SLOC | 80 | Knots |

Apply repository-defined Knots limits instead if explicitly configured. These are per-function heuristics, not project-wide scores. Only scan source languages supported by the installed Knots version.

## Workflow for all supported languages

1. Identify changed source paths, the relevant language(s), existing project checks, and whether `knots` is on PATH.
2. Make the minimum maintainable change and update relevant tests.
3. Run Knots directly with no project-specific wrapper or devenv task. From the repository root, pick source directories that actually exist and contain the changed code; for a conventional `src/` layout:

   ```sh
   knots -r src/ --cognitive-threshold 15 --mccabe-threshold 10 --nesting-threshold 4 --sloc-threshold 80
   ```

   For other layouts, replace `src/` with actual source paths or individual files. Avoid generated code, vendored dependencies, and build outputs by scoping the inputs appropriately. If a repository has a Knots configuration, respect its existing thresholds and exclusions instead of overriding them.
4. If `.knots-baseline.json` exists and the project uses it, pass `--baseline .knots-baseline.json` in the direct Knots invocation. Never run `--write-baseline` without explicit approval. If no baseline exists, compare violations against unchanged code or the pre-edit state; do not assume every reported violation is new. Knots `--changed` / `--since` may help when supported.
5. Run the project's established formatter, tests, and other quality checks appropriate to the language. Do not invent new setup or build tasks.
6. Refactor newly introduced violations and rerun affected checks. Report passes, failures, existing debt, or unavailable tools precisely.

If Knots is unavailable or does not support the project's language, say so explicitly; apply KISS design review and existing language tooling without pretending a complexity check ran. See [references/setup.md](references/setup.md) for optional installation guidance.

## Additional Rust checks

For Rust codebases only, run these directly (no `clippy.toml` required):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings -D clippy::too_many_lines -D clippy::type_complexity -D clippy::too_many_arguments
cargo test --workspace --all-features
```

Use the repository's documented feature selections if `--all-features` is inappropriate, and respect explicit project lint configuration. Keep Rust lint checks separate from Knots. Do **not** enable `clippy::cognitive_complexity` (previously called `cyclomatic_complexity`): Clippy documents it as unsuitable for measuring complexity. Knots owns both cognitive and McCabe complexity. Clippy's default settings apply without creating `clippy.toml`; the Knots SLOC limit is the authoritative function-size threshold.

## Completion report

State which Knots inputs and thresholds were used; the Clippy, formatting, and test results for Rust; any new violations; and any existing violations that remain. Never claim a tool passed unless it was executed.
