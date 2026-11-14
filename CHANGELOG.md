# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.15](https://github.com/bitomule/bazelqueue/compare/v0.1.14...v0.1.15) - 2026-11-13

### Fixed

- an unreadable shell during a fast-lane check no longer fails the build ([#48](https://github.com/bitomule/bazelqueue/pull/48))

### Internal

- run the validation suites and archive checks through Bazel

## [0.1.14](https://github.com/bitomule/bazelqueue/compare/v0.1.13...v0.1.14) - 2026-11-09

### Changed

- the queue no longer knows about output disks ([#45](https://github.com/bitomule/bazelqueue/pull/45))

### Fixed

- the resource estimate no longer inflates while the Mac swaps ([#46](https://github.com/bitomule/bazelqueue/pull/46))
- a slow external volume no longer sends builds to a nearly full internal disk ([#44](https://github.com/bitomule/bazelqueue/pull/44))
- keep worktree Bazel setup out of tracked files ([#42](https://github.com/bitomule/bazelqueue/pull/42))
- exclude worktree Bazel output from package discovery ([#41](https://github.com/bitomule/bazelqueue/pull/41))
- a shell that exits during legacy discovery no longer makes status and wait fail ([#39](https://github.com/bitomule/bazelqueue/pull/39))

### Internal

- explain jobs limits and queue reservations ([#43](https://github.com/bitomule/bazelqueue/pull/43))

## [0.1.13](https://github.com/bitomule/bazelqueue/compare/v0.1.12...v0.1.13) - 2026-11-07

### Added

- pick the output base disk before each build and judge free space where builds write ([#37](https://github.com/bitomule/bazelqueue/pull/37))
- show each queued job's position, reason and entry time in status --json ([#33](https://github.com/bitomule/bazelqueue/pull/33))
- point every worktree at the shared Bazel caches and cap the disk cache ([#35](https://github.com/bitomule/bazelqueue/pull/35))
- let a build carry a priority so urgent ones go first in the queue ([#32](https://github.com/bitomule/bazelqueue/pull/32))
- give each worktree its own Bazel output base ([#30](https://github.com/bitomule/bazelqueue/pull/30))

### Fixed

- keep the queue moving when swap holds the disk under the safety margin ([#38](https://github.com/bitomule/bazelqueue/pull/38))

### Internal

- document migrating existing checkouts and stopping the server before deleting a worktree ([#36](https://github.com/bitomule/bazelqueue/pull/36))
- pin per-worktree lanes and disk measurement, document the cost ([#34](https://github.com/bitomule/bazelqueue/pull/34))

## [0.1.12](https://github.com/bitomule/bazelqueue/compare/v0.1.11...v0.1.12) - 2026-11-07

### Added

- cut stalled Bazel jobs as a time failure and free their slot ([#27](https://github.com/bitomule/bazelqueue/pull/27))

### Fixed

- keep the queue moving under load and let small jobs pass ([#29](https://github.com/bitomule/bazelqueue/pull/29))
- admit builds on what the machine really has free ([#26](https://github.com/bitomule/bazelqueue/pull/26))

## [0.1.11](https://github.com/bitomule/bazelqueue/compare/v0.1.10...v0.1.11) - 2026-11-05

### Added

- start up-to-date builds and tests at once through a fast lane ([#23](https://github.com/bitomule/bazelqueue/pull/23))

### Internal

- run the native Bazel contract without GitHub or the registry ([#25](https://github.com/bitomule/bazelqueue/pull/25))

## [0.1.10](https://github.com/bitomule/bazelqueue/compare/v0.1.9...v0.1.10) - 2026-10-30

### Changed

- share local build capacity without a warning pressure penalty ([#20](https://github.com/bitomule/bazelqueue/pull/20))

### Fixed

- recover orphaned quarantine reservations without stalling the queue ([#22](https://github.com/bitomule/bazelqueue/pull/22))

## [0.1.9](https://github.com/bitomule/bazelqueue/compare/v0.1.8...v0.1.9) - 2026-10-27

### Fixed

- let Bazel check the cache beyond the reserved CPUs and size local tests by their reservation ([#19](https://github.com/bitomule/bazelqueue/pull/19))
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
