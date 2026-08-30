//! Renders each kind of load error for real, to inspect diagnostic quality.
//!
//! `cargo run -p pbps-load --example diagnostics`

use std::path::Path;

fn show(title: &str, yaml: &str) {
    println!("\n\x1b[1m### {title}\x1b[0m");
    match pbps_load::load_table_str(Path::new("schema/dbo.café.yml"), yaml) {
        Ok(t) => println!("(loaded successfully: {})", t.name),
        Err(errs) => {
            for e in errs {
                println!("{:?}", miette::Report::new(e));
            }
        }
    }
}

fn main() {
    // The non-ASCII content is deliberate: miette wants byte offsets, while
    // serde-saphyr's `Span::offset()` is a character offset. Any multi-byte
    // character makes the two diverge, and the label lands in the wrong place.
    show(
        "semantic error: unclosed parenthesis in a type (non-ASCII file, checks byte offsets)",
        "table: dbo.café\ndescription: Café régulier — clients français\ncolumns:\n  identifiant:\n    type: bigint\n  solde:\n    type: \"decimal(18, 2\"\n    nullable: false\n",
    );

    show(
        "semantic error: table name is not schema-qualified",
        "table: customer\ncolumns:\n  a:\n    type: int\n",
    );

    show(
        "semantic error: malformed foreign key target",
        "table: dbo.t\ncolumns:\n  a:\n    type: int\nforeign_keys:\n  fk_a:\n    columns: [a]\n    references: dbo.region\n",
    );

    show(
        "structural error: misspelled field name",
        "table: dbo.t\ncolumns:\n  a:\n    type: int\n    nulable: false\n",
    );

    show(
        "structural error: duplicate column name",
        "table: dbo.t\ncolumns:\n  email:\n    type: nvarchar(255)\n  email:\n    type: varchar(50)\n",
    );
}
