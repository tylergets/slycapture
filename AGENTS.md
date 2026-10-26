# Agent Instructions for `slycapture`

## Environment
- Use `nix develop` to enter the project dev shell.
- `cargo` is available from within `nix develop`.

## Required Workflow
- When you create a new file, always stage it immediately with `git add <path>`.
- Whenever you change the configuration format (`src/config.rs`), also update `nix/home-manager.nix` (if options change) and `README.md` to keep docs and module behavior aligned.
- Run `nix build` after changes that can affect the actual program behavior or build output.

## Validation
- Treat `nix build` as the required project-level verification step before finishing work when changes can affect the actual program. For docs-only changes, `nix build` is not required.

## Deployment
- When asked to deploy or push, update the version in `Cargo.toml` and `flake.nix`.
- Run `cargo update` (inside `nix develop`) to update `Cargo.lock` and include it in the release commit.
- Run `nix flake update` to update `flake.lock` and include it in the release commit.
- Create a commit message and push to the main branch.
