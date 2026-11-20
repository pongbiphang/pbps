//! Tests of the text tokenizer (#2950, item 4). The engine is the oracle:
//! every corpus text is run on a real SQLite, what the tokenizer reads from the
//! stored text is compared with what the pragmas say (`facts`), and the
//! constraint names and checks it claims are probed with the engine's own
//! `DROP CONSTRAINT` and with the rows the engine refuses (`oracles`). The
//! corpus is mutated (`mutations`), every prefix and single-character deletion
//! of it is fed in (`robust`), and the forms of `ALTER TABLE` are applied in
//! random sequences with the stored text tokenised after every step
//! (`closure`).

mod closure;
mod corpus;
mod facts;
mod mutations;
mod oracles;
mod refusals;
mod robust;
mod structure;
