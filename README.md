# Symphony

Symphony turns project work into isolated, autonomous implementation runs, allowing teams to manage
work instead of supervising coding agents.

[![Symphony demo video preview](.github/media/symphony-demo-poster.jpg)](.github/media/symphony-demo.mp4)

_In this [demo video](.github/media/symphony-demo.mp4), Symphony monitors a Linear board for work and spawns agents to handle the tasks. The agents complete the tasks and provide proof of work: CI status, PR review feedback, complexity analysis, and walkthrough videos. When accepted, the agents land the PR safely. Engineers do not need to supervise Codex; they can manage the work at a higher level._

> [!WARNING]
> Symphony is a low-key engineering preview for testing in trusted environments.

## Running Symphony

### Requirements

Symphony works best in codebases that have adopted
[harness engineering](https://openai.com/index/harness-engineering/). Symphony is the next step --
moving from managing coding agents to managing work that needs to get done.

### Option 1. Make your own

Tell your favorite coding agent to build Symphony in a programming language of your choice:

> Implement Symphony according to the following spec:
> https://github.com/openai/symphony/blob/main/SPEC.md

### Option 2. Use the Rust reference implementation for this fork

Check out [rust-todoist/README.md](rust-todoist/README.md) for instructions on how to set up your
environment and run the Rust Todoist implementation. The Elixir runtime remains in this fork as a
legacy reference, but Rust is the primary implementation path here. You can also ask your favorite
coding agent to help with the setup:

> Set up Symphony for my repository based on
> https://github.com/openai/symphony/blob/main/rust-todoist/README.md

For Rust Todoist planning and implementation docs, see:

- [rust-todoist/docs/linear-to-todoist-prd.md](rust-todoist/docs/linear-to-todoist-prd.md)
- [rust-todoist/docs/linear-to-todoist-spec.md](rust-todoist/docs/linear-to-todoist-spec.md)
- [rust-todoist/docs/todoist-rust-runtime.md](rust-todoist/docs/todoist-rust-runtime.md)

## Script Layout

- Root [`scripts/`](scripts/README.md) is for repo-wide tooling and shared smoke-harness assets.
- [`rust-todoist/scripts/`](rust-todoist/scripts) is for Rust Todoist runtime-local helper scripts.
- [`scripts/smoke_repo_baseline/`](scripts/smoke_repo_baseline) is a fixture that models the root of the external smoke repo, which is why it includes its own `AGENTS.md`.

---

## License

This project is licensed under the [Apache License 2.0](LICENSE).
