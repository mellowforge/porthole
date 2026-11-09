# Contributing to Porthole

Porthole is a Tauri 2 desktop app: a Rust core in `src-tauri` and a React/TypeScript UI in `src`.

## Prerequisites

- macOS with the Xcode Command Line Tools (`xcode-select --install`)
- Node 22 or newer
- Rust (stable toolchain)
- Docker (only for the integration tests and the manual check)

## Install and run

```sh
npm install
npm run tauri dev
```

To work on the UI alone, without the Rust side, start the Vite dev server and open it with mock data:

```sh
npm run dev
```

Then open <http://localhost:1420/?mock> in a browser. The mock only exists on the dev server; tests and production builds never use it.

## Tests

All commands run from the repository root.

UI tests and type checks:

```sh
npm test
npx tsc --noEmit
npx tsc -p tsconfig.node.json --noEmit
```

Rust unit tests and integration tests (the integration tests need the test databases running):

```sh
docker compose -f docker-compose.test.yml up -d --wait   # PG 11 on :54311, PG 17 on :54317, MySQL 8.4 on :33084, MariaDB 11.4 on :33114,
                                                        # MongoDB 6.0 on :27060, 8.0 on :27080, 8.0 replica set on :27082
(cd src-tauri && cargo test)
(cd src-tauri && cargo clippy --all-targets -- -D warnings)
docker compose -f docker-compose.test.yml down    # when finished
```

`cargo test` runs the unit tests and the integration tests in `src-tauri/tests`. The integration tests serialize themselves.

`tests/pg_integration.rs` runs against both PostgreSQL servers. To target other servers, set `TEST_PG_URLS` to a comma-separated list of URLs:

```sh
(cd src-tauri && TEST_PG_URLS="postgres://user:pass@host:5432/db" cargo test --test pg_integration)
```

Default: `postgres://postgres:test@localhost:54311/app_test,postgres://postgres:test@localhost:54317/app_test`.

`tests/mysql_integration.rs` runs against both MySQL and MariaDB the same way; it recreates the databases `fx` and `fx_other` and the user `ts_app` on each. To target other servers, set `TEST_MYSQL_URLS`:

```sh
(cd src-tauri && TEST_MYSQL_URLS="mysql://root:pass@host:3306/app_test" cargo test --test mysql_integration)
```

`tests/mongo_integration.rs` runs against MongoDB 6.0 and 8.0 (standalone, with auth) and an 8.0 single-node replica set (no auth; `--wait` holds until its healthcheck has initiated it). It recreates the databases `fx` and `fx_other` on each, and on the servers with auth the read-only user `ro` (role `read` on `fx`). To target other servers, set `TEST_MONGO_URLS`; a URL without a user is treated as a server without auth:

```sh
(cd src-tauri && TEST_MONGO_URLS="mongodb://root:pass@host:27017/?authSource=admin" cargo test --test mongo_integration)
```

Default: `mongodb://root:test@localhost:27060/?authSource=admin,mongodb://root:test@localhost:27080/?authSource=admin,mongodb://localhost:27082/`.

One test touches the real macOS Keychain and is ignored by default. Run it on a Mac with:

```sh
(cd src-tauri && cargo test -- --ignored)
```

## Build a local app

```sh
npm run tauri build
```

The results are in `src-tauri/target/release/bundle/`:

- `macos/Porthole.app`
- `dmg/Porthole_<version>_<arch>.dmg`, where `<arch>` is your Mac's architecture (`aarch64` or `x64`)

This build is not signed with a Developer ID, so macOS will warn when you open it unless you sign it with your own Developer ID. To build a signed, notarized release, see [Releasing](#releasing-maintainers).

## Manual check

The window, menus, dialogs and Keychain are checked by hand. See [docs/manual-check.md](docs/manual-check.md).

## Where data lives

- Connection profiles, saved queries, history and open tabs: `~/Library/Application Support/io.github.mellowforge.porthole/app.db` (SQLite)
- Passwords: macOS Keychain, service `io.github.mellowforge.porthole`, account `connection:{id}`. Passwords are never written to `app.db`.

## Releasing (maintainers)

`scripts/release.sh` builds the signed, notarized universal app:

```sh
scripts/release.sh
```

It writes these to `release/<version>/` (git-ignored), with the version taken from `src-tauri/tauri.conf.json`:

- `Porthole.app`, signed and stapled
- `Porthole_<version>_universal.dmg`, signed and stapled
- `SHA256SUMS.txt`

`scripts/release.sh --skip-notarize` does everything except notarizing and stapling, for trying out a build. Its DMG is named `Porthole_<version>_universal_UNNOTARIZED.dmg` and it writes no `SHA256SUMS.txt`. Don't publish its output.

The files are assembled in a temporary directory and moved to `release/<version>/` as the last step, so a failed run leaves an earlier build there untouched.

One-time setup:

1. Install both Rust targets of the universal build:

   ```sh
   rustup target add aarch64-apple-darwin x86_64-apple-darwin
   ```

2. Install the Developer ID Application certificate named at the top of the script in your login keychain.
3. Store the notary credentials in a keychain profile. The command prompts for an app-specific password:

   ```sh
   xcrun notarytool store-credentials porthole-notary --apple-id <apple-id> --team-id <team-id>
   ```

The script reads credentials only from the keychain. Never commit a certificate, key, password or `.env` file.

Before publishing the repository or a release, run the audit:

```sh
scripts/audit-public.sh
```

It fails if a tracked file holds credentials, key files or personal home folders. It also applies any extra patterns from the optional, git-ignored file named in the script.

Before publishing the repository, also check its history:

```sh
scripts/audit-public.sh --history --single-commit
```

`--history` also checks the author and committer emails, messages and tree paths of every commit reachable from `HEAD`. `--single-commit` fails if there is more than one such commit. `--single-commit` needs `--history`.
