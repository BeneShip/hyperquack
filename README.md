# HyperQuack

The missing hyper-tenancy tool for MotherDuck.

| Subcommand   | Description                                                                           |
|--------------|---------------------------------------------------------------------------------------|
| **list**     | List all service accounts in the MotherDuck organization                              |
| **identify** | Print name of user associated with current motherduck_token environment variable      |
| **env**      | Provide MotherDuck shell environment variables for the given service account          |
| **duckdb**   | Start a DuckDB session for the given service account                                  |
| **map**      | Run a command against all matching service accounts in parallel                       |
| **map-db**   | Run a command against each matching database of the given service account in parallel |
| **shell**    | Start a new instance of your shell ($SHELL) for the given service account             |
| **delete**   | Permanently delete a service account                                                  |
| **init**     | Create skeleton for use with `hq map`                                                 |
| **help**     | Print this message or the help of the given subcommand(s)                             |

All subcommands read an admin token from the `motherduck_token` environment
variable. Accounts are looked up through the official
[MotherDuck REST API](https://motherduck.com/docs/key-tasks/service-accounts-guide/manage-service-accounts-and-tokens/).

## `hq list --json`

Prints one JSON object per line, in the shape returned by the REST API's
`GET /v1/users`:

```json
{"id":"…","username":"sa_example","email":"…","first_name":"…","last_name":"…","is_service_account":true,"is_deprovisioned":false,"roles":["builder"]}
```

`roles` lists only roles assigned directly to the account, not those inherited
through a custom role.

## `hq delete`

Deleting a service account immediately revokes its tokens and permanently
deletes the data it owns. MotherDuck cannot undo it. To make that hard to do by
accident, `hq delete`:

- refuses accounts that are not service accounts;
- refuses if any table in a database the account owns (attached or detached)
  contains at least one row, and lists those tables. Empty or drop them first;
- asks you to retype the account name. Pass `--yes` to skip this; it is required
  when stdin is not a terminal.

# Installation

For now, clone this repo and run this:

```
cargo install --path .
```
