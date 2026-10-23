# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.9](https://github.com/bitomule/bazelqueue/compare/v0.1.8...v0.1.9) - 2026-10-23

### Fixed

- remove job receipts once the coordinator has recorded them, and prune receipts of forgotten jobs ([#17](https://github.com/bitomule/bazelqueue/pull/17))

## [0.1.8](https://github.com/bitomule/bazelqueue/compare/v0.1.7...v0.1.8) - 2026-10-23

### Fixed

- share estimates across runs that only change test filters, and size new runs from their repository ([#15](https://github.com/bitomule/bazelqueue/pull/15))

## [0.1.7](https://github.com/bitomule/bazelqueue/compare/v0.1.6...v0.1.7) - 2026-10-23

### Fixed

- size repeated builds and tests from their last runs so small ones share the machine ([#13](https://github.com/bitomule/bazelqueue/pull/13))

## [0.1.6](https://github.com/bitomule/bazelqueue/compare/v0.1.5...v0.1.6) - 2026-10-22

### Added

- tell agents how a queued build ended in one call ([#12](https://github.com/bitomule/bazelqueue/pull/12))

### Fixed

- let setup upgrade the coordinator while jobs are queued or running ([#9](https://github.com/bitomule/bazelqueue/pull/9))

### Internal

- el Harness de e2e espera a guardianes y coordinadores antes de borrar su carpeta ([#11](https://github.com/bitomule/bazelqueue/pull/11))

## [0.1.5](https://github.com/bitomule/bazelqueue/compare/v0.1.4...v0.1.5) - 2026-10-20

### Fixed

- stop hooks, read-only commands and clean from reserving the whole machine ([#7](https://github.com/bitomule/bazelqueue/pull/7))

## [0.1.4](https://github.com/bitomule/bazelqueue/compare/v0.1.3...v0.1.4) - 2026-10-13

### Fixed

- hand the coordinator over to the installed release during setup ([#5](https://github.com/bitomule/bazelqueue/pull/5))

## [0.1.3](https://github.com/bitomule/bazelqueue/compare/v0.1.2...v0.1.3) - 2026-10-13

### Fixed

- *(packaging)* expect CHANGELOG.md in the release archive ([#3](https://github.com/bitomule/bazelqueue/pull/3))

## [0.1.2](https://github.com/bitomule/bazelqueue/compare/v0.1.1...v0.1.2) - 2026-10-13

### Added

- keep building with a reduced budget under memory-pressure warning ([#2](https://github.com/bitomule/bazelqueue/pull/2))

### Internal

- replace broken Mermaid rendering with static queue diagram
- remove personal queue migration from README
- remove Makefile-specific README framing
- redesign README with visual queue guide
- record verified Homebrew publication and live activation
