# HyperQuack

The missing hyper-tenancy tool for MotherDuck.

| Subcommand   | Description                                                                            |
|--------------|----------------------------------------------------------------------------------------|
| **list**     | List all service accounts in the MotherDuck organization                               |
| **identify** | Print name of user associated with current motherduck_token environment variable       |
| **env**      | Provide MotherDuck shell environment variables for the given service account           |
| **duckdb**   | Start a DuckDB session for the given service account                                   |
| **map**      | Run a command against all matching service accounts in parallel                        |
| **map-db**   | Run a command against each matching database of the given service account in parallel  |
| **shell**    | Start a new instance of your shell ($SHELL) for the given service account              |
| **init**     | Create skeleton for use with `hq map`                                                  |
| **help**     | Print this message or the help of the given subcommand(s)                              |
| ------------ | -------------------------------------------------------------------------------------- |

# Installation

For now, clone this repo and run this: ```
cargo install --path .
```
