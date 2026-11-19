//! The SQL for the changes SQLite makes in place (item 3 of #2950): create and
//! drop table, add, rename and drop column, create and drop index and view.
//! Anything that needs the table rebuilt is refused by name until item 8.
