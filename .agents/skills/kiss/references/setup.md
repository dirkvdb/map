# Optional installation and sharing

This skill does not install analysis tools and does not require a devenv task, shell script, or `clippy.toml`.

Install Knots using a supported method, such as `uv tool install knots` or `cargo install knots`, then ensure `knots` is available on PATH. For reproducible environments, pin a tested Knots version using the project's normal package-management mechanism, optionally adding its package to `devenv.nix` through the ordinary `packages` list. Do not create `devenv` scripts or tasks.

For Rust, install the `rustfmt` and `clippy` rustup components or include them in the project's existing Rust toolchain. The skill runs Cargo commands directly without a Clippy config file; Rust's default thresholds and any existing repository lint policy apply.

For contributors sharing a repository, commit the skill under `.agents/skills/kiss/`, or add that directory as a Git submodule and document `git clone --recurse-submodules`. The skill is instructions, not a CI enforcement mechanism: CI can independently run the same direct `knots` and (for Rust) Cargo commands.

Check `knots --help` for supported options and `knots --supported-languages` if available in your installed version. The current Knots project describes support for C, C++, Rust, Python, JavaScript, TypeScript, Ada, Go, Java, C#, Kotlin, Swift, PHP, Fortran, Scala and Lua; check the installed release because language support changes.
