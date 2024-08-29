# Bors design
This document briefly describes how the bors bot works.

`bors` is a binary that launches an `axum` web server, which listens on a `/github` endpoint for webhooks related to a
GitHub app attached to the bot. Once a webhook arrives, and it passes a filter of known webhooks, it is converted to
a `BorsEvent` and passed to a command service that executes it.

The command service stores persistent data (like information about ongoing try builds) inside a database and performs
queries and commands on attached GitHub repositories using the GitHub REST API.

The main goal of the bot is to make sure that commits pushed to the main branch of a repository pass all required CI
checks. Bors takes care of preparing (merge) commits for testing on CI and of actually merging the commits into the
main branch if all CI tests pass.

## Repository management
On startup, and when a GitHub app installation is changed, bors will try to load information about all repositories
attached to the GitHub app. Each such repository **must** contain a `rust-bors.toml` file in its main branch, otherwise
the bot will ignore it. The bot will load the repository configuration and start listening for webhooks coming from
each loaded repository.

```text
..................
|    GH repo 1   |    read by
| rust-bors.toml | ---------------
..................               |
                                 -----> [Bors]
..................               |
|    GH repo 2   |    read by    |
| rust-bors.toml | ---------------
..................
```

## Sending commands
The bot can be controlled by commands embedded within pull request comments on GitHub. The supported command list
can be found [here](commands.md). Each command is delivered as a webhook to the bot, which parses it,
executes it and usually posts the result/response back onto the corresponding PR as a comment.

### User permissions
To perform privileged commands (e.g. starting a try build), users must have the proper permissions set. Permissions are
loaded by the bot from the [team API](https://github.com/rust-lang/team), more specifically from
`https://team-api.infra.rust-lang.org/v1/permissions/bors.<repo-name>.[try/review].json`.

There are two separate permissions, `try` (for managing try builds), and `review` (for approving PRs).

## Periodic refresh
Periodically (every few minutes), the bot will perform a refresh action, which will do the following for every attached
repository:
- Check the currently running CI workflow. If some of them are running for too long
(based on the `timeout` configured for the repository), it will cancel them.
- Reload user permissions from the Team API.
- Reload `rust-bors.toml` config for the repository from its main branch.

## Concurrency
The bot is currently listening for GitHub webhooks concurrently, however it handles all commands serially, to avoid
race conditions. This limitation is expected to be lifted in the future.

## Try builds
A try build means that you execute a specific CI job on a PR (without merging the PR), to test if the job passes C
tests. Here is a sequence diagram that describes what happens when a try build is scheduled (generated using
https://textart.io/sequence).

```text
+-----+                            +-------+                              +-----+ +-----+ +---------+
| PR  |                            | bors  |                              | GHA | | DB  | | teamAPI |
+-----+                            +-------+                              +-----+ +-----+ +---------+
   |                                   |                                     |       |         |
   | @bors try (via webhook)           |                                     |       |         |
   |---------------------------------->|                                     |       |         |
   |                                   |                                     |       |         |
   |                                   | check user permissions              |       |         |
   |                                   |------------------------------------------------------>|
   |                                   |                                     |       |         |
   |                                   | force push parent to `try-merge`    |       |         |
   |                                   |------------------------------------>|       |         |
   |                                   |                                     |       |         |
   |                                   | merge PR branch with `try-merge`    |       |         |
   |                                   |------------------------------------>|       |         |
   |                                   |                                     |       |         |
   |                                   | force push `try-merge` to `try`     |       |         |
   |                                   |------------------------------------>|       |         |
   |                                   |                                     |       |         |
   |                                   | store try build                     |       |         |
   |                                   |-------------------------------------------->|         |
   |                                   |                                     |       |         |
   |                           comment |                                     |       |         |
   |<----------------------------------|                                     |       |         |
   |                                   |                                     |       |         |
   |                                   |                   workflow finished |       |         |
   |                                   |<------------------------------------|       |         |
   |                                   |                                     |       |         |
   |                                   | update try build                    |       |         |
   |                                   |-------------------------------------------->|         |
   |                                   |                                     |       |         |
   |                                   |            (last) workflow finished |       |         |
   |                                   |<------------------------------------|       |         |
   |                                   |                                     |       |         |
   |                                   | update try build                    |       |         |
   |                                   |-------------------------------------------->|         |
   |                                   |                                     |       |         |
   |     comment with try build result |                                     |       |         |
   |<----------------------------------|                                     |       |         |
   |                                   |                                     |       |         |
```

Bors first decides which parent commit to use (usually the latest version of the main branch, but it can be overridden),
then it merges it with the latest PR commit in `automation/bors/try-merge`, and then force pushes this merged commit
to `automation/bors/try`, where the CI tests should run. It also stores information about the try build in the DB, so
that it can handle timed out builds or let the user cancel the build.

We need two branches, since it is not possible to atomically force set a branch to the parent commit and merge
it with the PR commit using the GitHub API. Without atomicity, CI would run twice unnecessarily (once after setting
the branch to parent, and then again after merging the PR commit).

Note that `automation/bors/try-merge` should not have any CI workflows configured! These should be configured for the `automation/bors/try` branch instead.

## Recognizing that CI has succeeded/failed
With [homu](https://github.com/rust-lang/homu) (the old bors implementation), GitHub actions CI running repositories had
to use a "fake" job that marked the whole CI workflow as succeeded or failed, to signal to bors if it should consider
the workflow to be OK or not.

The new bors uses a different approach. It asks GitHub which [check suites](https://docs.github.com/en/rest/checks/suites)
are attached to a given commit, and then it waits until all of these check suites complete (or until a timeout is reached).
Thanks to this approach, there is no need to introduce fake CI jobs.
