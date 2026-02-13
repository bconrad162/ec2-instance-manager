# AGENTS.md

## Engineering Rules

- Every new feature, behavior change, or bug fix must include unit tests in the same change.
- If a change cannot be unit tested directly, add the closest deterministic test coverage possible and document the gap in the PR/summary.
- Do not merge code changes without running the test suite locally.

## Product Focus

- Prioritize keeping terminal sessions embedded in the app and making them feel as close to native terminals as possible (colors, prompts, input behavior, and shell initialization).
