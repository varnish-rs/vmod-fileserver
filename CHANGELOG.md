# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.1.0] - 2026-08-10

### Changed

- `mime.types` files with duplicate lines for the same extension no longer fail VCL init; the last matching definition now wins (matches nginx/Apache behavior).
- HTTP method validation now runs before any filesystem access, so an unsupported method always returns 405 regardless of whether the requested file exists.

### Fixed

- Missing files now return 404, and unreadable files return 403, instead of a generic backend-fetch error.

### Added

- Test coverage for URLs containing multiple `?` when stripping the query string before filesystem lookup.
- Test coverage for duplicate-extension `mime.types` entries (`tests/test08.vtc`, `tests/dup1.types`).
- Test coverage for missing/unreadable files and method validation ahead of filesystem access (`tests/test09.vtc`).

## [0.0.11] and earlier

Not tracked in this file — see `git log` and the version-matching table in [README.md](README.md).
