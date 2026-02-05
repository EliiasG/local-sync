# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

local-sync is a Rust tool that syncs git-tracked files between a local workspace and a NAS folder, respecting .gitignore.

## Build Commands

- **Build**: `cargo build`
- **Run**: `cargo run`
- **Test**: `cargo test`
- **Single test**: `cargo test <test_name>`
- **Check (fast compile check)**: `cargo check`
- **Format**: `cargo fmt`
- **Lint**: `cargo clippy`

## Usage

1. Run `local-sync init <nas-path>` to initialize
2. Run `local-sync push` to copy local files to NAS
3. Run `local-sync pull` to copy NAS files to local
4. Run `local-sync status` to see sync status
5. Run `local-sync add <file|dir>` to add gitignored files/directories to sync
6. Run `local-sync remove <file|dir>` to remove from additional sync list

## Architecture

Single-file implementation in `src/main.rs`:
- Uses `git ls-files --cached --others --exclude-standard` as baseline for files to sync
- Additional gitignored files/directories can be added via `add` command (stored as `+path` lines in `.local-sync`)
- Stores a `.local-sync-manifest` JSON file in the NAS folder for conflict detection and deletion tracking
- Uses SHA256 content hashes to detect changes
