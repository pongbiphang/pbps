//! The catalog reader (item 4 of #2950): `sqlite_schema` and the `pragma_*`
//! functions for structure, then a tokenizer for what exists only in the stored
//! `CREATE` text (CHECK, partial index predicates, constraint names, view
//! bodies). Text it does not fully recognise is refused, never compared.

pub(crate) mod text;
