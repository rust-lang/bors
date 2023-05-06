# Bors
Home of a (WIP) rewrite of the [`homu`](https://github.com/rust-lang/homu) bors implemntation in Rust.

## Development

## Database
You must have `sea-orm-cli` installed for the following commands to work.
```console
$ cargo install sea-orm-cli
```

You must also set up a `DATABASE_URL` environment variable.
```console
$ export DATABASE_URL=sqlite://bors.db?mode=rwc
```

- Generate a new migration
  ```console
  $ sea-orm-cli migrate -d database/migration/ generate <name>
  ```
- Apply migrations
  ```console
  $ sea-orm-cli migrate -d database/migration/ up
  ```
- Re-generate entities (run this after generating and manually modifying a new migration and then applying it)
  ```console
  $ sea-orm-cli generate entity -o database/entity/src --lib
  ```
