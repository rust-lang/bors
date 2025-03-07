# Commands
Here is a list of commands currently supported by bors. All commands have to be prefixed by the *command prefix*,
which is by default set to `@bors`.

| **Command**        | **Permissions** | **Description**                                                         |
|--------------------|-----------------|-------------------------------------------------------------------------|
| `ping`             |                 | Send a ping to bors to check that it responds.                          |
| `try`              | `try`           | Start a try build based on the most recent commit from the main branch. |
| `try parent=<sha>` | `try`           | Start a try build based on the specified parent commit `sha`.           |
| `try cancel`       | `try`           | Cancel a running try build.                                             |
| `p=<priority>`     | `review`        | Set the priority of a PR.                                               |
| `delegate+`        | `review`        | Delegate approval authority to the PR author.                           |
| `delegate-`        | `review`        | Remove any previously granted delegation.                               |
