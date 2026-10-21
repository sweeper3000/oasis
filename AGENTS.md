# Repository Guidelines

## Project Structure & Module Organization
Oasis is a self-hosted file server. `backend/src/` contains the Rocket application: `api/` defines endpoints, `entity/` holds data models, `service/` implements supporting services, and `util/` provides shared helpers. SQL migrations and test fixtures live in `backend/assets/`.

`frontend/src/` contains Svelte pages, components, modals, players, and TypeScript utilities. Translation files are in `frontend/src/assets/i18n/`; static assets and web workers are in `frontend/public/`. Treat `frontend/public/vendor/` as third-party code. `docker/` contains packaging resources; `doc/` contains documentation images.

## Build, Test, and Development Commands
Run frontend commands from `frontend/` and Cargo commands from `backend/`.

- `npm ci`: install frontend dependencies from the lockfile.
- `npm run dev`: watch and rebuild the frontend; starts a static development server.
- `npm run build`: generate production bundles in `frontend/public/build/`.
- `npm run check`: run Svelte and TypeScript diagnostics.
- `cargo run`: start Rocket, serving the frontend and API on port 8000 by default. Build the frontend first and browse `http://localhost:8000` for integrated behavior.
- `cargo test`: run backend unit tests.
- `node build.js` (repository root): build and package both applications into `release/oasis/`; this reinstalls frontend dependencies and replaces `release/`.

## Coding Style & Naming Conventions
Use four-space indentation and standard rustfmt formatting for Rust; use `snake_case` functions/modules and `PascalCase` types. Frontend source generally uses two-space indentation, semicolons, `PascalCase.svelte` components, and camelCase TypeScript identifiers. Match surrounding conventions when editing. No dedicated ESLint or Prettier configuration is present. Keep English and Chinese translation keys aligned.

## Testing Guidelines
Backend tests use Rust's built-in framework, colocated in `#[cfg(test)]` modules with names such as `test_get_file_encoding`. Run tests from `backend/` so fixture paths resolve; some filesystem tests are Linux-only. No coverage threshold or frontend test runner is configured. For changes, run relevant backend tests and frontend checks/builds; manually verify affected authentication, file operations, or preview flows.

## Commit & Pull Request Guidelines
History uses short, plain summaries such as `fix page refresh issue` and version-only release commits. Keep commits focused and summaries specific. For pull requests, describe the behavior change, link relevant issues, report validation commands/results, and include screenshots for visible UI changes.

## Security & Configuration
Copy `backend/assets/oasis.conf.sample` to `oasis.conf` beside the executable to configure binding or TLS. Keep runtime databases, user files, credentials, and private TLS keys out of commits.
