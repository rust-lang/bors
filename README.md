# Bors
Home of a (WIP) rewrite of the [`homu`](https://github.com/rust-lang/homu) bors implementation in Rust.

There are a few documents that should help with understanding the bot:
- [Design](docs/design.md) of the bot.
- [Development guide](docs/development.md).

If you want to help testing the bot, please ask around on the [`#t-infra`](https://rust-lang.zulipchat.com/#narrow/stream/242791-t-infra)
stream on Rust Zulip.

The production instance of the bot is deployed at https://bors-prod.rust-lang.net. You can find its help page with the list of supported commands [here](https://bors-prod.rust-lang.net/help).

## Configuration
There are several parameters that can be configured when launching the bot. Parameters without a default value are
required.

| **CLI flag**       | **Environment var.** | **Default** | **Description**                                           |
|--------------------|----------------------|-------------|-----------------------------------------------------------|
| `--app-id`         | `APP_ID`             |             | GitHub app ID of the bors bot.                            |
| `--private-key`    | `PRIVATE_KEY`        |             | Private key of the GitHub app.                            |
| `--webhook-secret` | `WEBHOOK_SECRET`     |             | Key used to authenticate GitHub webhooks.                 |
| `--db`             | `DATABASE_URL`       |             | Database connection string. Only PostgreSQL is supported. |
| `--cmd-prefix`     | `CMD_PREFIX`         | @bors       | Prefix used to invoke bors commands in PR comments.       |

### Special branches
The bot uses the following branch names for its operations.

#### Try builds
- `automation/bors/try-merge`
  - Used to perform merges of a pull request commit with a parent commit.
  - Should not be configured for any CI workflows!
- `automation/bors/try`
  - This branch should be configured for CI workflows corresponding to try runs.

#### Auto builds
- `automation/bors/auto-merge`
  - Used to merge PR with the latest base branch commit.
  - Should not be configured for any CI workflows!
- `automation/bors/auto`
  - This branch should be configured for CI workflows that need to run before merging to the base branch.

The merge and non-merge branches are needed because we cannot set branches to parent and merge them with a PR commit
atomically using the GitHub API.

### GitHub app
If you want to attach `bors` to a GitHub app, you should point its webhooks at `<http address of bors>/github`.

### How to add a repository to bors
Here is a guide on how to add a repository so that this bot can be used on it:
1) Add a file named `rust-bors.toml` to the root of the main branch of the repository. The configuration struct that
describes the file can be found in `src/config.rs`. [Here](rust-bors.example.toml) is an example configuration file.
2) Install the GitHub app corresponding to this bot to the corresponding repository. You can use the
`https://github.com/settings/apps/<app-name>/installations` link.
3) Enable the corresponding permissions and webhook events for the GH app (see [this](docs/development.md#how-to-test-bors-on-live-repositories)).
4) Configure CI workflows on push to:
   - `automation/bors/try` branch (for try builds)
   - `automation/bors/auto` branch (for auto builds)
5) Give the bot permissions to push to `automation/bors/try`, `automation/bors/try-merge`, `automation/bors/auto`, and `automation/bors/auto-merge`.
