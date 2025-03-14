# Commands
Here is a list of commands currently supported by bors. All commands have to be prefixed by the *command prefix*,
which is by default set to `@bors`.

| **Command**                           | **Permissions** | **Description**                                                                    |
|---------------------------------------|-----------------|------------------------------------------------------------------------------------|
| `ping`                                |                 | Send a ping to bors to check that it responds.                                     |
| `help`                                |                 | Print help message with available commands.                                        |
| `r+`                                  | `review`        | Approve this PR.                                                                   |
| `r+ p=<priority>`                     | `review`        | Approve this PR with specified priority.                                           |
| `r+ rollup=<never/iffy/maybe/always>` | `review`        | Approve this PR with specified rollup status.                                      |
| `r=<user>`                            | `review`        | Approve this PR on behalf of specified user.                                       |
| `r=<user> p=<priority>`               | `review`        | Approve this PR on behalf of specified user with priority.                         |
| `r-`                                  | `review`        | Unapprove this PR.                                                                 |
| `try`                                 | `try`           | Start a try build based on the most recent commit from the main branch.            |
| `try parent=<sha>`                    | `try`           | Start a try build based on the specified parent commit `sha`.                      |
| `try parent=last`                     | `try`           | Start a try build based on the parent commit of the last try build.                |
| `try jobs=<job1,job2,...>`            | `try`           | Start a try build with specific CI jobs (up to 10).                                |
| `try cancel`                          | `try`           | Cancel a running try build.                                                        |
| `p=<priority>`                        | `review`        | Set the priority of a PR. Alias for `priority=`                                    |
| `delegate+`                           | `review`        | Delegate approval authority to the PR author.                                      |
| `delegate-`                           | `review`        | Remove any previously granted delegation.                                          |
| `rollup=<never/iffy/maybe/always>`    | `review`        | Set the rollup mode of a PR.                                                       |
| `rollup`                              | `review`        | Mark PR for rollup with "always" status.                                           |
| `rollup-`                             | `review`        | Mark PR for rollup with "maybe" status.                                            |
| `info`                                |                 | Get information about the current PR.                                              |
