# Contributing Guidelines

Thank you for your interest in contributing to this project. Whether it is a bug report, a new feature, a correction, or additional documentation, we value feedback and contributions from our community.

Please read this document before you submit an issue or a pull request, so that we have the information we need to respond.

## Reporting Bugs and Feature Requests

Use the project's issue tracker to report bugs or suggest features. Before you file an issue, check open and recently closed issues to avoid duplicates. Include:

- A reproducible example or a series of steps.
- The version of the code you used.
- Any modifications you made.
- Anything unusual about your environment or your deployment.

## Contributing via Pull Requests

Before you send a pull request:

1. Work against the latest `main` branch.
2. Check open and recently merged pull requests for related work.
3. Open an issue before you begin significant work.
4. Keep the change focused.
5. Verify the affected workflow.
6. Use clear commit messages.
7. Respond to review and automated-check feedback.

## Project checks

- Write a failing test before you change behavior, then make it pass.
- Every durable directory has an `AGENTS.md` file. When a change alters what a directory holds or how it works, update the nearest `AGENTS.md` in the same change.
- When you change user-visible or backend behavior, add a line under `## [Unreleased]` in `CHANGELOG.md` in the same change. Name the behavior, command or setting in plain short sentences. Leave out ticket ids, person names and dates.
- `python3 scripts/dox_lint.py` must print `DOX PASS`.
- You run the infra tests from `infra` with `../scripts/node.sh pnpm test`. [Developer verification](README.md#developer-verification) states which credentials the script needs.
- Do not commit credentials, keys, persistent passwords, or customer data. Before you push, scan your changes for secrets with gitleaks and the configuration file `.gitleaks.toml` at the repository root.

## Code of Conduct

This project has adopted the [Amazon Open Source Code of Conduct](https://aws.github.io/code-of-conduct).
For more information, see the [Code of Conduct FAQ](https://aws.github.io/code-of-conduct-faq)
or contact opensource-codeofconduct@amazon.com with any additional questions or comments.

## Security Issue Notifications

Report potential security issues through the [AWS vulnerability reporting page](https://aws.amazon.com/security/vulnerability-reporting/).
Do not create a public issue for a suspected vulnerability. See [SECURITY.md](SECURITY.md).

## Licensing

See [LICENSE](LICENSE) for the project license. Contributions are accepted under that license.
