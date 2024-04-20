# Bors
Home of a (WIP) rewrite of the [`homu`](https://github.com/rust-lang/homu) bors implementation in Rust.

Commands supported by the bot can be found [here](docs/commands.md).
Design of the bot is described [here](docs/design.md).

If you want to help testing the bot, please ask around on the [`#t-infra`](https://rust-lang.zulipchat.com/#narrow/stream/242791-t-infra)
stream on Rust Zulip.

## Configuration
There are several parameters that can be configured when launching the bot. Parameters without a default value are
required.

| **CLI flag**       | **Environment var.** | **Default** | **Description**                                                  |
|--------------------|----------------------|-------------|------------------------------------------------------------------|
| `--app-id`         | `APP_ID`             |             | GitHub app ID of the bors bot.                                   |
| `--private-key`    | `PRIVATE_KEY`        |             | Private key of the GitHub app.                                   |
| `--webhook-secret` | `WEBHOOK_SECRET`     |             | Key used to authenticate GitHub webhooks.                        |
| `--db`             | `DB`                 |             | Database connection string. PostgreSQL and SQLite are supported. |
| `--cmd-prefix`     | `CMD_PREFIX`         | @bors       | Prefix used to invoke bors commands in PR comments.              |

### Special branches
The bot uses the following two branch names for its operations.
- `automation/bors/try-merge`
  - Used to perform merges of a pull request commit with a parent commit.
  - Should not be configured for any CI workflows!
- `automation/bors/try`
  - This branch should be configured for CI workflows corresponding to try runs.

The two branches are currently needed because we cannot set `try-merge` to parent and merge it with a PR commit
atomically using the GitHub API.

### GitHub app
If you want to attach `bors` to a GitHub app, you should point its webhooks at `<http address of bors>/github`.

### How to add a repository to bors
Here is a guide on how to add a repository so that this bot can be used on it:
1) Add a file named `rust-bors.toml` to the root of the main branch of the repository. The configuration struct that
describes the file can be found in `src/config.rs`. [Here](rust-bors.example.toml) is an example configuration file.
2) Install the GitHub app corresponding to this bot to the corresponding repository. You can use the
`https://github.com/settings/apps/<app-name>/installations` link (to be automated via `team` repo).
3) Configure a CI workflow on push to the `automation/bors/try` branch.
4) Give the bot permissions to push to `automation/bors/try` and `automation/bors/try-merge` (to be automated via `team` repo).

## Development
Directory structure:
- `database/migration`
  - `SeaORM` migrations that are the source of truth for database schema
- `database/entity`
  - Automatically generated `SeaORM` DB entities, which are generated from a (Postgre) database.
- `src/bors`
  - Bors commands and their handlers.
- `src/database`
  - Database ORM layer built on top of SeaORM.
- `src/github`
  - Communication with the GitHub API and definitions of GitHub webhook messages.
- `src/config.rs`
  - Configuration of repositories that want to use bors.
- `src/permissions.rs`
  - Handling of user permissions for performing try and review actions.

### Database
You must have `sea-orm-cli` installed for the following commands to work.
```console
$ cargo install sea-orm-cli
```

You must also set up a `DATABASE_URL` environment variable. **You can use SQLite for local testing,
but when entities are regenerated, it should be done against a Postgre database!**
```console
$ export DATABASE_URL=sqlite://bors.db?mode=rwc
```

#### Updating the DB schema
1) Generate a new migration
    ```console
    $ sea-orm-cli migrate -d database/migration/ generate <name>
    ```
2) Change the migration manually in `database/migration/src/<new-migration>.rs`.
3) Apply migrations to a **Postgre** DB. (You can use Docker for that).
    ```console
    $ sea-orm-cli migrate -d database/migration/ up
    ```
4) Re-generate entities, again against a **Postgre** DB.
    ```console
    $ sea-orm-cli generate entity -o database/entity/src --lib
    ```
