# ZedDeps

A Zed extension that shows dependency update hovers and quick fixes for **Cargo.toml** and **package.json**.

## Features

- **Hover info** — Hover over any dependency version to see the latest stable release available on [crates.io](https://crates.io) or [npm](https://www.npmjs.com).
- **Quick fix** — Click the lightbulb to update a dependency to its latest stable version in one action.
- **Diagnostics** — Outdated dependencies are highlighted with an information diagnostic.

## Installation

Install directly from the Zed extensions panel by installing this repo as a dev extension.

## Supported manifests

| File            | Registry   |
|-----------------|------------|
| `Cargo.toml`    | crates.io  |
| `package.json`  | npm        |

## Development

```bash
cargo test --workspace
cargo build -p zeddeps-lsp
```

## License

MPL-2.0
