# Bors
Home of a (WIP) rewrite of the [`homu`](https://github.com/rust-lang/homu) bors implemntation in Rust.

## Architecture
- An `axum` web server listens on a `/github` endpoint for webhooks related to a GitHub app of the bot.
- The webhooks are converted to `BorsEvent`s and executed.
- The bot stores data in a database and performs queries and commands on attached GitHub repositories
using the GitHub REST API.

## Development
Directory structure:
- `database/migration`
  - `SeaORM` migrations that are the source of truth for database schema
- `database/entity`
  - Automatically generated `SeaORM` DB entities, which are generated from a (Postgre) database.
- `src`
  - Code of the bot

## Database
You must have `sea-orm-cli` installed for the following commands to work.
```console
$ cargo install sea-orm-cli
```

You must also set up a `DATABASE_URL` environment variable. **You can use SQLite for local testing,
but when entities are regenerated, it should be done against a Postgre database!**
```console
$ export DATABASE_URL=sqlite://bors.db?mode=rwc
```

### Updating the DB schema
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
