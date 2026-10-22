# Commit Convention

This project follows [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).
Trunk-based: commit directly on `main`, one intention per commit, no feature branches.

## Format

```
<type>(<scope>): <subject>

[optional body]

[optional footer]
```

- **type**: one of the types below
- **scope**: optional, e.g. `config`, `metadata`, `staging`, `health`, `ci`, `docker`
- **subject**: imperative, lowercase, no trailing period, ≤72 characters

## Types

| Type       | Use for                                                 |
|------------|---------------------------------------------------------|
| `feat`     | A new feature                                           |
| `fix`      | A bug fix                                               |
| `chore`    | Maintenance tasks (tooling, release bumps)              |
| `docs`     | Documentation only changes                              |
| `style`    | Code style (formatting, no logic change)                |
| `refactor` | Code change that neither fixes a bug nor adds a feature |
| `perf`     | Performance improvements                                |
| `test`     | Adding or fixing tests                                  |
| `build`    | Build system, Dockerfile                                |
| `ci`       | CI/CD configuration changes                             |
| `revert`   | Reverts a previous commit                               |

## Examples

```
feat(staging): journal prepared entries and commit them atomically
fix(metadata): reject mvhd creation_time of zero
test: assert output is untouched before commit
ci: build and smoke-test the container image on arm64
```

## Breaking changes

Append `!` after the type/scope and add a `BREAKING CHANGE:` footer.

## Language

Commit messages and code comments in English; documentation in French.
