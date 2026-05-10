# Zalezhnosti

A Zed extension that shows dependency update hovers and quick fixes for **Cargo.toml**, **package.json**, **pyproject.toml**, and **requirements.txt**.

## Features

- **Registry and repo links** - Hovers link to the package page on the registry and, when available, the source repository.
- **Hover info** - Hover over any dependency version to see the latest stable release available on [crates.io](https://crates.io), [npm](https://www.npmjs.com), or [PyPI](https://pypi.org).
- **Quick fix** - Click the lightbulb to update a dependency to its latest stable version in one action.
- **Diagnostics** - Outdated dependencies are highlighted with an information diagnostic.

## Installation

Install directly from the Zed extensions panel by installing this repo as a dev extension.

## Supported manifests

| File | Registry |
|------|----------|
| `Cargo.toml` | crates.io |
| `package.json` | npm |
| `pyproject.toml` | PyPI |
| `requirements.txt` | PyPI |

Rust workspace dependencies declared in `[workspace.dependencies]` are supported, including `workspace = true` references from member crates.
Python dependencies are supported in standardized `pyproject.toml` dependency arrays and in `requirements.txt` files, including referenced requirement and constraint files.

## Development

```bash
cargo test --workspace
cargo build -p zalezhnosti-lsp
```

## License

MPL-2.0
