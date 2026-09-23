# Zirv

**Put your AI agents to work together.**

Claude Code, Codex, and your other coding agents can do more as a team. Zirv brings them into one terminal, gives them shared project context, and keeps work moving through delegation, supervised handoffs, and repeatable workflows.

**Up to 41% faster · Up to 51% cheaper**

Measured on larger headless Sonnet tasks against Claude Code with Superpowers; correctness was about the same across the full benchmark.

[![Release](https://img.shields.io/github/v/release/Glubiz/zirv-cli)](https://github.com/Glubiz/zirv-cli/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://github.com/Glubiz/zirv-cli/blob/main/LICENSE)

[Get started](#get-started) · [Why Zirv](#why-zirv) · [Full CLI reference](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md)

## Why Zirv

- **One team across models.** Delegate work to supported harnesses and keep every session visible in one terminal.
- **Less repeated context.** Share project instructions and compact memory without paying to send the same context twice.
- **Long sessions that keep moving.** Watch for context drift, then compact or hand off before progress gets lost.
- **A path from prompt to shipped work.** Run development workflows, verification, and project scripts from the same CLI.
- **Your tools, your rules.** Repository content cannot grant itself permissions or override operator settings.

## Get started

Install Zirv for your platform:

### macOS

```sh
brew tap glubiz/homebrew-tap
brew install zirv
```

### Windows

```powershell
choco install zirv
```

### Linux

```sh
curl -sSfL https://raw.githubusercontent.com/Glubiz/zirv-cli/main/install.sh | sh
```

Other architectures and installation methods are in the [installation reference](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#installation).

Install and sign in to at least one supported agent harness, then run the guided setup in your project:

```sh
cd my-project
zirv setup
zirv
```

In an interactive terminal, bare `zirv` starts a session when the current directory contains a local `.zirv/`. Use `zirv chat` to start explicitly; use `zirv tour` for a guided introduction. Run `zirv init` if you only want to create a project script directory.

> **Native harness: coming soon.** `zirv native` currently only shows a notice. Use `zirv chat` for the existing harness.

## Go deeper

- [Setup and harness migration](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#ai-setup-and-harness-migration)
- [Scripts, parameters, and shortcuts](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#usage)
- [Workflows, agents, and skills](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#development-workflows)
- [Context, memory, supervision, and safety](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#context-management-zirv-ctx)
- [Supported harnesses and models](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#supported-harnesses-and-models)
- [Upgrading](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#upgrading)

The [full CLI reference](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md) preserves detailed commands, configuration examples, and current development notes.

## Contributing and license

Contributions are welcome. [Open an issue](https://github.com/Glubiz/zirv-cli/issues) for major changes or submit a pull request. Zirv is licensed under [MIT](https://github.com/Glubiz/zirv-cli/blob/main/LICENSE); see the [disclaimer](https://github.com/Glubiz/zirv-cli/blob/main/DISCLAIMER.md) for supervision and third-party harness notes.
