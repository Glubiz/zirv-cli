# Zirv

**Make AI development work across agents, models, and long sessions.**

Zirv is a CLI that brings your coding agents, project context, and development workflows into one terminal. It runs alongside installed harnesses such as Claude Code and Codex, coordinates work between them, and supervises sessions so progress can survive context loss or a handoff. Its goal is to make multi-agent work reliable and repeatable while you stay in control.

[![Release](https://img.shields.io/github/v/release/Glubiz/zirv-cli)](https://github.com/Glubiz/zirv-cli/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://github.com/Glubiz/zirv-cli/blob/main/LICENSE)

[Get started](#get-started) · [Explore capabilities](#what-zirv-does) · [Full CLI reference](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md)

> **Native harness: coming soon.** The current release uses the existing harness through `zirv chat`. `zirv native` only shows a notice; native execution cannot be enabled yet. Native design notes in the full reference describe work in progress.

## What Zirv does

| Capability | Why it matters |
| --- | --- |
| **Coordinate agents** | Work across supported harnesses and models, delegate tasks to workers, and keep sessions visible in one terminal dashboard. |
| **Keep useful context** | Share project instructions and compact memory between harnesses, with handoffs when a session needs to restart or change harness. |
| **Supervise long sessions** | Monitor context health and usage, then advise, compact, or restart with a handoff when work begins to degrade. |
| **Run repeatable work** | Use development workflows, verification, and project scripts from the same CLI. |
| **Keep operator control** | Repository context cannot grant itself permissions or override operator-owned safety settings. |

Zirv also runs YAML, JSON, and TOML scripts from `.zirv/commands/`, with parameters, shortcuts, and platform-specific steps.

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

## Go deeper

- [Setup and harness migration](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#ai-setup-and-harness-migration)
- [Scripts, parameters, and shortcuts](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#usage)
- [Workflows, agents, and skills](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#development-workflows)
- [Context, memory, supervision, and safety](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#context-management-zirv-ctx)
- [Supported harnesses and models](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#supported-harnesses-and-models)
- [Upgrading](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md#upgrading)

The [full CLI reference](https://github.com/Glubiz/zirv-cli/blob/main/REFERENCE.md) preserves detailed commands, configuration examples, and current development notes. On the website, these links open the reference on GitHub.

## Contributing and license

Contributions are welcome. [Open an issue](https://github.com/Glubiz/zirv-cli/issues) for major changes or submit a pull request. Zirv is licensed under [MIT](https://github.com/Glubiz/zirv-cli/blob/main/LICENSE); see the [disclaimer](https://github.com/Glubiz/zirv-cli/blob/main/DISCLAIMER.md) for supervision and third-party harness notes.
