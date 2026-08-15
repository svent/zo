# zo — Zettabyte Oracle

A fast, minimal CLI for using OpenRouter models from your terminal. zo handles one-off questions, pipelines, multi-turn chat, file edits, workspace tools, shell commands, web search, and image generation without turning every request into an unrestricted agent session.

## Install

```bash
cargo install zo-cli
export OPENROUTER_API_KEY='sk-or-v1-...'
```

Pre-built binaries and full setup instructions are available in the [documentation](https://zo.svent.dev/docs/installation).

## Features

- **Progressive output** — Stream Markdown with syntax-highlighted code in a terminal and plain text in pipelines.
- **Flexible model selection** — Use aliases such as `/sol` and `/sonnet`, fuzzy matches, or full OpenRouter model IDs.
- **Reasoning control** — Set effort per request, custom model, or globally.
- **File references and scoped output** — Read with `@file`, write with `!file`, or read and update with `@!file`; globs are supported.
- **Workspace file tools** — Let the model inspect a project with `--files read` or edit it with `--files write`.
- **Shell tools and policies** — Enable live command execution with `--shell` and control approvals through policy files.
- **Web search** — Add OpenRouter server-side search with `--web`.
- **Interactive chat** — Keep context across turns with multiline input and file-path completion.
- **Unix pipelines** — Combine prompts with piped command output or redirect zo's plain-text response.
- **Image generation** — Generate one image directly to a workspace path with `--image`.
- **Custom models** — Define aliases with system prompts and model-specific reasoning effort.
- **Explicit safety controls** — Review diffs, opt into hidden paths, inspect tool calls, and choose interactive or automated approval behavior.

## Examples

```bash
# Ask a question; quotes are optional for simple prompts
zo How do I unpack a tar file into a directory

# Select a model and reasoning effort
zo --reasoning-effort medium /sonnet 'Explain async Rust'

# Include files or grant a specific output path
zo '@src/*.rs Review this module'
zo 'Document this project in !ARCHITECTURE.md'
zo 'Add error handling to @!src/main.rs'

# Let the model inspect or edit the current workspace
zo --files read 'Explain how configuration is loaded'
zo --files write 'Refactor the parser and update its tests'

# Enable shell commands or current web results
zo --shell 'Run the test suite and summarize any failures'
zo --web 'Summarize the latest Rust release announcement'

# Compose with other commands
git diff | zo 'Review these changes for bugs'

# Continue interactively
zo --chat --files read 'Help me understand this project'

# Generate an image
zo --image assets/icon.png 'Minimal blue terminal icon'
```

## Permissions

zo starts without workspace or shell tools. File markers grant access only to the named paths; `--files` grants access inside the current workspace; and `--shell` is separately opt-in and policy-controlled. Existing-file changes show a diff and ask for approval unless `--accept-writes` is set. Hidden tool and output paths remain blocked unless `--hidden` is supplied.

See [zo.svent.dev](https://zo.svent.dev) for the quick start, complete permission model, configuration, and examples.

## License

MIT License — see [LICENSE](LICENSE).
