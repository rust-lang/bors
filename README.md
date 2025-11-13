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
| `--client-id`      | `OAUTH_CLIENT_ID`    |             | GitHub OAuth client ID for rollup UI.                     |
| `--client-secret`  | `OAUTH_CLIENT_SECRET`|             | GitHub OAuth client secret for rollup UI.                 |
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

### OAuth app
If you want to create rollups, you will need to create a GitHub OAuth app configured like so:
1. In the [developer settings](https://github.com/settings/developers), go to "OAuth Apps" and create a new application.
2. Set the Authorization callback URL to `<http address of bors>/oauth/callback`.
3. Note the generated Client ID and Client secret, and pass them through the CLI flags or via your environment configuration.

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

## Contributing

We are happy to receive contributions to bors! You can check out our list of [good first issues](https://github.com/rust-lang/bors/issues?q=is%3Aissue%20state%3Aopen%20label%3A%22good%20first%20issue%22).

Note that sometimes the issues can get stale or they might not contain enough information that you would need to implement a feature or fix a bug. If you would like to work on something non-trivial, it would be great to open a topic on our [Zulip channel](https://rust-lang.zulipchat.com/#narrow/channel/496228-t-infra.2Fbors/topic/bors.20down/with/542683375) so that we can discuss it.

## License
Bors is dual-licensed under [MIT](LICENSE-MIT) and [Apache 2.0](LICENSE-APACHE).
