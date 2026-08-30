## Git commits

- Use the Conventional Commits format for every commit:
  `type(optional-scope): concise description`.
- Use an appropriate type such as `feat`, `fix`, `docs`, `refactor`, `test`,
  `build`, `ci`, or `chore`.
- Keep the subject concise, imperative, lowercase, and without a trailing
  period.
- Keep each commit focused on one logical change.
- Before committing, inspect the staged diff and ensure unrelated changes are
  not included.
- Do not create a commit unless the user explicitly asks for one.

Examples:

- `feat: add production deployment workflow`
- `fix(storage): remove board objects after deletion`
- `docs: document manual release process`
