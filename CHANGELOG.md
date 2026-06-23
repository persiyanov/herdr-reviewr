# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Project scaffold: edition 2024, pinned toolchain, centralized `[lints]`,
  CI (fmt + clippy `-D warnings` + test + build), `just` tasks, `cargo-deny`
  config, MIT license.
- `docs/herdr-api-notes.md` — verified herdr socket/CLI surface from the wiring
  spike (pane split, agent send, `events.subscribe`).
