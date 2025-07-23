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
- Reload the mergeability status of open PRs from GitHub.
- Sync the status of PRs between the DB and GitHub.
- Run the merge queue.

## Concurrency
The bot is currently listening for GitHub webhooks concurrently, however it handles all commands serially, to avoid
race conditions. This limitation might be lifted in the future.

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

## Auto builds
The merge queue is an automated system that processes approved pull requests and merges them into the base branch after
ensuring they pass all CI checks. PRs are approved using the `@bors r+` command and then "queued" automatically.

Here is a sequence diagram that describes what happens when a PR is approved and enters the merge queue:

```text
+-----+                            +-------+                              +-----+ +-----+ +---------+
| PR  |                            | bors  |                              | GHA | | DB  | | teamAPI |
+-----+                            +-------+                              +-----+ +-----+ +---------+
   |                                   |                                     |       |         |
   | @bors r+ (via webhook)            |                                     |       |         |
   |---------------------------------->|                                     |       |         |
   |                                   |                                     |       |         |
   |                                   | check user permissions              |       |         |
   |                                   |------------------------------------------------------>|
   |                                   |                                     |       |         |
   |                                   | store approval in DB                |       |         |
   |                                   |-------------------------------------------->|         |
   |                                   |                                     |       |         |
   | comment: "Commit abc123 approved" |                                     |       |         |
   |<----------------------------------|                                     |       |         |
   |                                   |                                     |       |         |
   |                                   | (every 30s: process merge queue)    |       |         |
   |                                   |                                     |       |         |
   |                                   | merge PR with base to `auto-merge`  |       |         |
   |                                   |------------------------------------>|       |         |
   |                                   |                                     |       |         |
   |                                   | push merge commit to `auto`         |       |         |
   |                                   |------------------------------------>|       |         |
   |                                   |                                     |       |         |
   |                                   | create check run on PR head         |       |         |
   |                                   |------------------------------------>|       |         |
   |                                   |                                     |       |         |
   |                                   | store auto build                    |       |         |
   |                                   |-------------------------------------------->|         |
   |                                   |                                     |       |         |
   |  comment: ":hourglass: Testing.." |                                     |       |         |
   |<----------------------------------|                                     |       |         |
   |                                   |                                     |       |         |
   |                                   |                   workflow started  |       |         |
   |                                   |<------------------------------------|       |         |
   |                                   |                                     |       |         |
   |                                   | store workflow in DB                |       |         |
   |                                   |-------------------------------------------->|         |
   |                                   |                                     |       |         |
   |                                   |                   workflow finished |       |         |
   |                                   |<------------------------------------|       |         |
   |                                   |                                     |       |         |
   |                                   | update auto build                   |       |         |
   |                                   |-------------------------------------------->|         |
   |                                   |                                     |       |         |
   |                                   |            (last) workflow finished |       |         |
   |                                   |<------------------------------------|       |         |
   |                                   |                                     |       |         |
   |                                   | update build status                 |       |         |
   |                                   |-------------------------------------------->|         |
   |                                   |                                     |       |         |
   |                                   | update check run on PR head         |       |         |
   |                                   |------------------------------------>|       |         |
   |                                   |                                     |       |         |
   |                                   | fast-forward base branch            |       |         |
   |                                   |------------------------------------>|       |         |
   |                                   |                                     |       |         |
   | comment: ":sunny: Test successful"|                                     |       |         |
   |<----------------------------------|                                     |       |         |
   |                                   |                                     |       |         |
```

The merge queue first merges the PR with the latest base branch commit in `automation/bors/auto-merge`, then pushes
this merged commit to `automation/bors/auto` where CI tests run. If all tests pass, the base branch is fast-forwarded
to the merge commit.

Only one auto build runs at a time to ensure that each PR is tested against the same branch state it will be merged into,
preventing the problem where two PRs pass tests independently but fail when combined.

Note that `automation/bors/auto-merge` should not have any CI workflows configured! These should be configured for the
`automation/bors/auto` branch instead.

## Recognizing that CI has succeeded/failed
With [homu](https://github.com/rust-lang/homu) (the old bors implementation), GitHub actions CI running repositories had
to use a "fake" job that marked the whole CI workflow as succeeded or failed, to signal to bors if it should consider
the workflow to be OK or not.

The new bors uses a different approach. In an earlier implementation, it used to ask GitHub which [check suites](https://docs.github.com/en/rest/checks/suites) were attached to a given commit, and then it waited until all of these check suites were completed (or until a timeout was reached). However, this was too "powerful" for rust-lang/rust, where we currently have only a single CI workflow per commit, and it was producing certain race conditions, where GitHub would send us a webhook that a check suite was completed, but when we then asked the GitHub API about the status of the check suite, it was still marked as pending.

To make the implementation more robust, it now behaves as follow:
- We listen for the workflow completed webhook.
- When it is received, we ask GitHub what are all the workflows attached to the workflow's check suite.
- If all of them are completed, we mark the build as finished.

Note that we explicitly do not read the "Check suite was completed" webhook, because it can actually be received *before* a webhook that tells us that the last workflow of that check suite was completed. If that happens, we could mark a build as completed without knowing the final conclusion of its workflows. That is not a big problem, but it would mean that we sometimes cannot post the real status of a workflow in the "build completed" bors comment on GitHub. So instead we just listen for the completed workflows.

In any case, with new bors there is no need to introduce fake conclusion CI jobs.
