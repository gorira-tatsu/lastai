# Privacy and Local Data

`lastai` is designed as a local command-line tool.

## What lastai reads

By default, `lastai` discovers local JSONL history files from:

- Codex: `$CODEX_HOME` or `~/.codex`
- Claude: `$CLAUDE_CONFIG_DIR` or `~/.claude`

These files can contain prompts, assistant responses, tool outputs, file paths,
project names, command output, and other sensitive local context.

## What lastai writes

`lastai` writes its own config and cache using OS-specific project directories.
Run:

```sh
lastai doctor
```

to see the exact paths on your machine.

The search index is a local cache derived from your conversation history. Treat
it as sensitive. Do not publish or upload the index directory.

## Network behavior

Normal search, indexing, and TUI use do not require network access.

Resume actions launch the configured provider command, such as `codex resume`
or `claude --resume`. Those commands may have their own network behavior.

## What is intentionally filtered

`lastai` tries to avoid indexing injected startup context such as Codex
`AGENTS.md` prelude, environment context blocks, skill bodies, and system
messages. This filtering is best-effort. Review search results before sharing
screenshots, logs, or exported output.

## Before publishing issues or logs

Avoid pasting:

- `lastai doctor` output if path names are sensitive
- search results containing private prompts or code
- files from the index cache directory
- provider history files from `~/.codex` or `~/.claude`
