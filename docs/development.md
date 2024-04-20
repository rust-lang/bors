# Development guide
This document should help you make sense of the codebase and provide
guidance on working with it.

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
