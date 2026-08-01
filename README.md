# zo - Zettabyte Oracle

A fast, minimal CLI for interacting with LLMs via OpenRouter.

## Features

- **Fuzzy model selection** - `/sonnet`, `/gpt4`, `/haiku`
- **File references** - `@file.txt` to include, `!file.txt` to write, `@!file.txt` to read+write
- **Multimodal inputs** - `@file.pdf`, `@file.png`, `@file.jpg`, `@file.gif`, and `@file.webp` are sent as native attachments
- **Interactive chat** - Multi-turn conversations with `--chat`
- **STDIN piping** - `cat code.rs | zo "review this"`
- **Streaming output** - Syntax-highlighted markdown in real-time
- **Custom models** - Define aliases with system prompts

## Quick Example

```bash
# Ask a question
zo "What is Rust?"

# Use a specific model
zo /sonnet "Explain async/await"

# Include files
zo "@main.rs Review this code"

# Include a PDF or image
zo "@document.pdf Summarize this"
zo "@diagram.png Describe this image"

#$ Pipeline
git diff | zo 'Summarize these changes'

# give read & write access to script.py
# (will show diff of changes and ask for approval)
zo 'Add type hints to @!script.py'

# give read access to src/*.rs and write-only access to ARCHITECTURE.md
zo '@src/*.rs document the architecture in !ARCHITECTURE.md'

# Chat mode (supports multiline with Alt-Enter, Ctrl-O, or Ctrl-J)
zo --chat /sonnet 'Explain async/await'
> Show me a practical example
> How does it compare to threads?
> exit
```

## Documentation

For installation, configuration, and detailed usage guides, visit the docs:

**[zo.svent.dev](https://zo.svent.dev)**

## License

MIT License - see [LICENSE](LICENSE) for details.
