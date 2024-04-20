# Bors design
This document briefly describes does the bors bot work.

`bors` is a binary that launches (an `axum`) web server, which listens on a `/github` endpoint for webhooks related to a
GitHub app attached to the bot. Once a webhook arrives, and it passes a filter of known webhooks, it is converted to
a `BorsEvent` and passed to a command service that executes it.

> Webhooks can be received concurrently, but the command service will execute them serially (currently without
any concurrency between the commands).

The command service stores persistent data (like information about ongoing try builds) inside a database and performs
queries and commands on attached GitHub repositories using the GitHub REST API.

# Repository management
On startup or when a GitHub app installation is changed, bors will try to load information about all repositories
attached to the GitHub app. Each such repository **must** contain a `rust-bors.toml` file in its main branch, otherwise
the bot will fail to start. The bot will load the repository configuration and start listening for webhooks coming from
loaded each repository.

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

# Sending commands
The bot can be controlled by commands embedded within pull request comments on GitHub. The supported command list 
can be found [here](commands.md). Each command is delivered as a webhook to the bot, which executes it and posts the result
back onto the corresponding PR as a comment.

# User permissions
Users that want to perform bors commands must have the proper permissions set. Permissions are loaded by the bot
from the [team API](https://github.com/rust-lang/team), more specifically from
`https://team-api.infra.rust-lang.org/v1/permissions/bors.<repo-name>.[try/review].json`.
The permissions are currently cached in memory, but they are reloaded from the server every minute.

There are two separate permissions, `try` (for managing try builds), and `review` (for approving PRs).

# Try build lifecycle
Here is a sequence diagram that describes what happens when a try build is scheduled (generated using
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

The merge of the PR branch with a parent branch (usually the main branch) happens in the `automation/bors/try-merge` branch.
This branch should not have any CI workflows configured! These should be configured for the `automation/bors/try` branch.
Two branches are needed, because we cannot reset a branch to the parent and merge it with the PR atomically using the
GitHub API.

# Periodic refresh
Every few minutes, the bot will check the currently active try builds and if some of them are running for too long
(based on the `timeout` configured for the repository), it will cancel them (and the whole try build).
