# AGENT.md

Guidelines for Claude Code and other AI agents working on this project.

## Commits

- **No AI attribution lines**: Do not add "Co-Authored-By: Claude [Model]" or similar lines to git commit messages. Omit them entirely.
- **Regular commit practice**: Follow standard git practices for the project. Keep commit messages clear and focused on the changes made.

## General

- Follow the project's existing code style and conventions.
- Refer to the main README and PLAN for project architecture and direction.

## Terminology

- **V2G** is the high level communication (ISO 15118, over V2GTP). **BPT** (bidirectional power transfer) or **V2X** is delivering power from the vehicle (discharge). Do not use V2G for discharging.
