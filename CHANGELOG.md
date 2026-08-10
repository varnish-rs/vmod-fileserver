# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.0.12]

### Changed

- `mime.types` files with duplicate lines for the same extension no longer fail VCL init; the last matching definition now wins (matches nginx/Apache behavior).

### Added

- Test coverage for URLs containing multiple `?` when stripping the query string before filesystem lookup.
- Test coverage for duplicate-extension `mime.types` entries (`tests/test08.vtc`, `tests/dup1.types`).

## [0.0.11] and earlier

Not tracked in this file — see `git log` and the version-matching table in [README.md](README.md).
