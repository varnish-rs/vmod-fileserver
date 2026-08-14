# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Added

- `root()` gained a `follow_links` option (`BOOL`, default `false`). By
  default, a request is resolved one path segment at a time and never
  follows a symlink (whether it points inside or outside `path`); set
  `follow_links = true` to follow symlinks unconditionally and restore the
  previous behavior.
- `API.md`, generated from source doc comments at build time, documents the
  full VCL API (`root()`, `backend()`).

### Changed

- By default, a request that hits a symlink anywhere in its path now fails
  instead of the symlink being followed.
- `root()` now opens `path` at VCL-load time (unless `follow_links = true`)
  and fails loading if it doesn't exist, instead of only failing per-request.

## [0.1.0] - 2026-08-10

### Changed

- Query strings are now stripped before resolving a file on disk, so cache-busting URLs like `/app.js?v=123` correctly resolve to `/app.js` instead of failing to find a file named `app.js?v=123`.
- `mime.types` files with duplicate lines for the same extension no longer fail VCL init; the last matching definition now wins (matches nginx/Apache behavior).
- HTTP method validation now runs before any filesystem access, so an unsupported method always returns 405 regardless of whether the requested file exists.

### Fixed

- Missing files now return 404, and unreadable files return 403, instead of a generic backend-fetch error.

### Added

- Test coverage for query-string stripping, including URLs containing multiple `?` (`tests/test07.vtc`).
- Test coverage for duplicate-extension `mime.types` entries (`tests/test08.vtc`, `tests/dup1.types`).
- Test coverage for missing/unreadable files and method validation ahead of filesystem access (`tests/test09.vtc`).

## [0.0.11] - 2026-06-12

### Fixed

- `If-Modified-Since` comparisons now use `>=` instead of `>`, so a request whose `If-Modified-Since` exactly matches the file's last-modified time correctly gets a 304.

### Changed

- Target Varnish 9.0.
- Renamed the internal VCC object type from `root` to `file_backend`.

## [0.0.10] - 2025-09-18

### Changed

- Internal-only: lint fixes and CI cleanups. Target Varnish 8.0. No functional changes.

## [0.0.9] - 2025-06-02

### Changed

- Internal-only: clippy lint fixes, formatting, and justfile tidy-up. Target Varnish 7.7.1. No functional changes.

## [0.0.8] - 2025-03-30

### Changed

- Adapted to varnish-rs API renames (`Serve` trait replaced by `VclBackend`). Target Varnish 7.7. No functional/behavior change.

## [0.0.7] - 2024-11-11

### Changed

- Migrated to varnish-rs's proc-macro-based vmod API (`#[varnish::vmod]`), replacing the old `build.rs` code generation. Target Varnish 7.6.

### Added

- `default.vcl` usage example.

## [0.0.6-1] - 2024-04-01

### Fixed

- Test suite adjusted for compatibility with older `curl` versions used in CI.

## [0.0.6] - 2024-04-01

### Added

- Test coverage ensuring the backend correctly serves a response without consuming the request body (`tests/test06.vtc`).

### Changed

- Target Varnish 7.5.

## [0.0.5] - 2023-09-23

### Changed

- Target Varnish 7.4. No functional changes.

## [0.0.3] and [0.0.4] - 2023-03-19

### Changed

- Simplified error handling in the backend implementation.
- Target Varnish 7.3.

## [0.0.1] and [0.0.2] - 2023-02-12

Initial implementation: a Varnish backend vmod serving files directly from disk, with `etag`/`last-modified` support and content-type detection via a `mime.types` database. Target Varnish 7.2.
