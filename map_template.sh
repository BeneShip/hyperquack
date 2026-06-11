#!/usr/bin/env bash
#
# This script is run once per matched MotherDuck service account by `hq map`
# (or once per matched database by `hq map-db`).
#
# The following environment variables are prepared for you:
#
#   motherduck_token     short-lived token for the service account
#   motherduck_user      the service account name
#   motherduck_database  the database to operate on (set by `hq map-db` only)
#
# Run with:
#
#   hq map <glob_pattern> -- ./this_script.sh
#
set -euo pipefail

# Example: dump each catalog's per-table column counts to a CSV.
duckdb -no-init "md:${motherduck_database:-}" <<'SQL'
copy (
    select
        current_catalog() as catalog,
        table_name,
        count(*) as column_count
    from information_schema.columns
    where table_catalog = current_catalog()
    group by table_name
) to (current_catalog() || '.csv') (format csv, header);
SQL