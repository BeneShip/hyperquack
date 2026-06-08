set enable_progress_bar = false;
prepare job as copy (
    select
        current_catalog() as catalog,
        table_name,
        count(*) as column_count,
    from information_schema.columns
    where table_catalog = current_catalog()
    group by table_name
) to $1 (format csv);
execute job(current_catalog() || '.csv');
