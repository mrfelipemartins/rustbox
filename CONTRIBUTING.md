# Contributing to Rustbox

Bug reports, focused pull requests, tests, and documentation improvements are welcome.

## Getting started

Use Rust 1.88 or newer. Fork the repository, create a branch, and follow the [development and testing guide](docs/testing.md). A disposable MySQL database is needed for integration tests; protocol and unit tests can run without one.

Before submitting a pull request, run:

```sh
npm ci --prefix admin
npm test --prefix admin
npm run build --prefix admin
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python3 -m unittest discover -s tests -p 'test_*.py'
```

Run database integration tests for persistence changes and relevant fixture checks for protocol changes. Describe checks you could not run. Capacity claims need measured workloads and results, not just passing unit tests.

## Proposing changes

For substantial behavior or architecture changes, open an issue first to discuss scope. Keep pull requests focused, explain the user-facing impact, and add regression coverage where appropriate. Update the [API specification](openapi.json) and [documentation](docs/README.md) when behavior changes.

Never include credentials, real mail content, private server addresses, or generated test secrets. Report vulnerabilities using [SECURITY.md](SECURITY.md), not public issue reports.

## Working together

Be respectful, explain disagreements constructively, and avoid harassment or sharing personal information. Maintainers may edit or remove abusive content and restrict participation.

Contributions are provided under the project's [MIT license](LICENSE).
