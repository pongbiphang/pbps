//! Editor schemas, shell completions and man pages (SPEC §14.1).
//!
//! # Why these are commands and not just files in the repository
//!
//! Files in the repository are the wrong version. A user runs the binary they
//! installed, and everything here is generated from that binary's own
//! definitions — the JSON Schema from the loader's and the config's types, the
//! completions and the man pages from the `clap` command tree. There is no
//! second description of the format to keep in step, which is the failure mode
//! every hand-written editor schema eventually has.
//!
//! Checked-in copies exist as well, under `schemas/`, for editors that resolve a
//! `$schema` URL and for anyone browsing the repository. A test regenerates them
//! and fails if they differ, so the copy cannot drift from the binary either.
//!
//! # Why the schema is a command at all
//!
//! Air-gapped editors (SPEC §1.1's adoption argument) cannot fetch a schema over
//! the network. `pbps schema > .pbps-schema.json` is the whole of the offline
//! path, and it needs no service to exist.

use std::io::Write as _;

use anyhow::Context as _;
use clap::CommandFactory as _;

/// Which schema to print.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SchemaKind {
    /// The project file, `pbps.yml`.
    Config,
    /// One declaration file: a table, or a view, procedure, function or trigger.
    Declaration,
}

/// The version of the *published schemas*, independent of the tool's.
///
/// It moves when a schema changes in a way an editor would notice, which the
/// tool version does — for reasons no editor cares about (SPEC §14.2,
/// acceptance criterion 6).
pub const SCHEMA_VERSION: u32 = 7;
// 2: the `data:` block (ADR-0004). An editor notices — it completes a block
//    that did not exist — which is exactly the criterion above.
// 3: the `hooks.on_apply_attempt` event hook.
// 4: the `role:` file (ADR-0005) and the `policies:` block in `pbps.yml`
//    (ADR-0008). Both schemas grew, and a consumer keying on this number
//    could not tell the widened schemas from the version-3 ones.
// 5: `policies.rules` is keyed by the rule catalogue and a suppression's
//    `rule` is one of its ids, where both were any string. An editor notices
//    in the way that matters most: it completes the ids, and it stops
//    accepting a misspelt one (DECISIONS 172).
// 6: each rule's entry in `policies.rules` is that rule's own shape — its
//    own parameters and no others — where all ten shared one. An editor
//    stops completing `rows` into a naming rule (DECISIONS 188).
// 7: a grant's permissions are the closed list the loader accepts, where they
//    were any string — and the list is the union of the engines' (ADR-0010
//    §6, DECISIONS 210). An editor completes `usage` and stops accepting
//    `contrl`.

pub fn schema(kind: SchemaKind) -> serde_json::Value {
    let mut v = match kind {
        SchemaKind::Config => serde_json::to_value(schemars::schema_for!(pbps_config::Config)),
        SchemaKind::Declaration => {
            serde_json::to_value(schemars::schema_for!(pbps_load::dto::DeclarationFile))
        }
    }
    // These are generated from types that derive the trait; a failure would mean
    // the derive itself produced something unserializable, which the build would
    // have refused long before this line.
    .expect("a generated schema always serializes");

    if let Some(obj) = v.as_object_mut() {
        // Stamped so a schema found on disk can say which binary produced it —
        // acceptance criterion 6, and the difference between "this is stale" and
        // "this is wrong".
        obj.insert("x-pbps-schema-version".to_owned(), SCHEMA_VERSION.into());
        obj.insert(
            "x-pbps-tool-version".to_owned(),
            env!("CARGO_PKG_VERSION").into(),
        );
    }
    v
}

/// The bytes written both by `pbps schema` and by the checked-in copies.
///
/// One function, so the file in the repository and the command's output cannot
/// come to differ in whitespace and make the drift test fail for nothing.
pub fn rendered(kind: SchemaKind) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&schema(kind)).expect("a generated schema always serializes")
    )
}

pub fn cmd_schema(kind: SchemaKind, out: Option<&std::path::Path>) -> anyhow::Result<()> {
    let text = rendered(kind);
    match out {
        Some(path) => {
            std::fs::write(path, &text)
                .with_context(|| format!("cannot write `{}`", path.display()))?;
            // To stderr: stdout is the schema when --out is absent, and a
            // progress line inside a piped JSON file would corrupt it.
            eprintln!("wrote {}", path.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

pub fn cmd_completions(shell: clap_complete::Shell) -> anyhow::Result<()> {
    let mut cmd = crate::Cli::command();
    let name = cmd.get_name().to_owned();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
    Ok(())
}

/// Writes one man page per command into `dir`.
///
/// A directory rather than stdout: `pbps` has subcommands, and a single page
/// documenting all of them is the page nobody reads. `man pbps-apply` is what an
/// operator actually types at three in the morning.
pub fn cmd_man(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create `{}`", dir.display()))?;
    let cmd = crate::Cli::command();

    let mut written = 0usize;
    let write = |name: &str, cmd: clap::Command| -> anyhow::Result<()> {
        let path = dir.join(format!("{name}.1"));
        let mut file = std::fs::File::create(&path)
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        clap_mangen::Man::new(cmd)
            .render(&mut file)
            .with_context(|| format!("cannot render `{}`", path.display()))?;
        file.flush()?;
        Ok(())
    };

    write("pbps", cmd.clone())?;
    written += 1;
    for sub in cmd.get_subcommands() {
        // The rendered page has to announce itself as `pbps-apply`, not
        // `apply`: `man` looks the page up by the name inside it, and a page
        // calling itself `apply` would collide with whatever else on the system
        // is called that.
        let name = format!("pbps-{}", sub.get_name());
        write(&name, sub.clone().name(name.clone()))?;
        written += 1;
    }
    eprintln!("wrote {written} man page(s) into {}", dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The checked-in copies under `schemas/` are what an editor resolving a
    /// `$schema` URL fetches, and what anyone browsing the repository reads. A
    /// copy that has fallen behind the binary is worse than no copy: it blesses
    /// files the loader refuses, and quietly. Regenerate with
    /// `pbps schema --kind <k> --out schemas/<file>`.
    #[test]
    fn the_checked_in_schemas_are_what_this_binary_produces() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("schemas");
        for (kind, file) in [
            (SchemaKind::Declaration, "declaration.schema.json"),
            (SchemaKind::Config, "pbps.yml.schema.json"),
        ] {
            let path = root.join(file);
            let on_disk = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            assert_eq!(
                on_disk,
                rendered(kind),
                "{} is stale; regenerate it with `pbps schema`",
                path.display()
            );
        }
    }

    /// `deny_unknown_fields` is what turns a typo into an error rather than a
    /// silent no-op (ADR-0003), and it only reaches the editor as
    /// `additionalProperties: false`. Losing it would make the schema accept
    /// exactly what the loader refuses — worse than shipping none.
    #[test]
    fn the_declaration_schema_refuses_unknown_fields() {
        let v = schema(SchemaKind::Declaration);
        let defs = &v["$defs"];
        for name in [
            "TableDto",
            "ColumnDto",
            "IndexDto",
            "ForeignKeyDto",
            "ModuleDto",
        ] {
            assert_eq!(
                defs[name]["additionalProperties"],
                serde_json::json!(false),
                "{name} must refuse unknown fields: {v}"
            );
        }
    }

    /// The point of generating rather than writing: the words a user types come
    /// from the enum the loader converts into.
    #[test]
    fn the_schema_lists_the_words_the_loader_accepts() {
        let v = schema(SchemaKind::Declaration);
        let actions = serde_json::to_string(&v["$defs"]["ReferentialAction"]).unwrap();
        for word in ["no_action", "cascade", "set_null", "set_default"] {
            assert!(actions.contains(word), "{actions}");
        }
    }

    /// A grant's permissions are the words `Permission` spells — all of them,
    /// both engines' (ADR-0010 §6) — and nothing else: an editor that accepted
    /// any string blessed `contrl`, and one listing a single engine's words
    /// would refuse a PostgreSQL project's `usage`. Which word a dialect lacks
    /// is `validate`'s finding.
    #[test]
    fn the_schema_lists_every_permission_and_no_other_word() {
        let v = schema(SchemaKind::Declaration);
        let items = &v["$defs"]["RoleDto"]["properties"]["grants"]["additionalProperties"]["items"];
        let listed: Vec<&str> = items["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("a grant's permissions are not a closed list: {items}"))
            .iter()
            .map(|v| v.as_str().expect("a permission is a string"))
            .collect();
        let all: Vec<&str> = pbps_model::Permission::ALL
            .iter()
            .map(|p| p.as_str())
            .collect();
        assert_eq!(listed, all, "{items}");
        assert!(listed.contains(&"usage") && listed.contains(&"view-definition"));
        assert!(!listed.contains(&"control"));
    }

    /// The rule ids a project writes come from the catalogue the checker
    /// looks them up in — one list, not two. Derived from
    /// `BTreeMap<String, _>` the schema accepted any key, so an editor blessed
    /// `naming.tabel` and the typo waited for `pbps validate`
    /// (DECISIONS 172).
    #[test]
    fn the_config_schema_knows_every_rule_and_no_others() {
        let v = schema(SchemaKind::Config);
        let catalogue: Vec<&str> = pbps_policy::rules::RULES.iter().map(|r| r.id).collect();

        let rules = &v["$defs"]["Policies"]["properties"]["rules"];
        let keys: Vec<&str> = rules["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("`rules` has no properties: {rules}"))
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, catalogue, "{rules}");
        // The half that refuses the typo. Listing the ids while still
        // accepting any other key would autocomplete correctly and validate
        // nothing.
        assert_eq!(
            rules["additionalProperties"],
            serde_json::json!(false),
            "{rules}"
        );

        // And the same list where a suppression names a rule: suppressing a
        // rule that does not exist suppresses nothing.
        let suppression = &v["$defs"]["Suppression"]["properties"]["rule"];
        let named: Vec<&str> = suppression["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("a suppression's rule is not a closed list: {suppression}"))
            .iter()
            .map(|v| v.as_str().expect("a rule id is a string"))
            .collect();
        assert_eq!(named, catalogue, "{suppression}");
    }

    /// Each rule's entry names that rule's own parameters and no others.
    /// One shared `RuleSetting` for all ten let the editor bless
    /// `naming.table: {rows: 5}`, which `Policies::check` refuses
    /// (DECISIONS 188).
    #[test]
    fn every_rule_schema_takes_only_that_rules_parameters() {
        let v = schema(SchemaKind::Config);
        let rules = &v["$defs"]["Policies"]["properties"]["rules"]["properties"];
        for rule in &pbps_policy::rules::RULES {
            let entry = &rules[rule.id];
            let forms = entry["anyOf"]
                .as_array()
                .unwrap_or_else(|| panic!("{}: no forms: {entry}", rule.id));
            // `off` as a YAML boolean, and only `false`: `true` says nothing
            // about the severity and is refused.
            assert!(
                forms.iter().any(|f| f["const"] == serde_json::json!(false)),
                "{}: {entry}",
                rule.id
            );
            assert!(
                !forms.iter().any(|f| f["const"] == serde_json::json!(true)),
                "{}: {entry}",
                rule.id
            );
            let detailed = forms
                .iter()
                .find(|f| f["type"] == "object")
                .unwrap_or_else(|| panic!("{}: no map form: {entry}", rule.id));
            let mut named: Vec<&str> = detailed["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{}: {entry}", rule.id))
                .keys()
                .map(String::as_str)
                .filter(|k| *k != "severity")
                .collect();
            named.sort_unstable();
            let mut expected: Vec<&str> = rule.params.to_vec();
            expected.sort_unstable();
            assert_eq!(named, expected, "{}: {entry}", rule.id);
            assert_eq!(
                detailed["additionalProperties"],
                serde_json::json!(false),
                "{}: {entry}",
                rule.id
            );
            // Every parameter has a shape; an empty one accepts anything,
            // which is the failure this whole schema exists to prevent.
            for p in rule.params {
                assert!(
                    detailed["properties"][*p]
                        .as_object()
                        .is_some_and(|o| { o.contains_key("type") }),
                    "{}: `{p}` has no type: {entry}",
                    rule.id
                );
            }
        }
    }

    /// A config schema that did not know the dialects would autocomplete a
    /// project into an engine that does not exist.
    #[test]
    fn the_config_schema_knows_the_dialects_and_the_required_key() {
        let v = schema(SchemaKind::Config);
        let text = serde_json::to_string(&v).unwrap();
        assert!(text.contains("mssql"), "{text}");
        assert!(text.contains("postgres"), "{text}");
        // `dialect` is the one key with no default; a schema that made it
        // optional would let an editor bless a file the loader rejects.
        assert_eq!(v["required"], serde_json::json!(["dialect"]), "{v}");
    }

    /// Both schemas carry their own version, so a copy found on disk can say
    /// whether it is the one this binary would produce.
    #[test]
    fn every_schema_is_stamped_with_its_version() {
        for kind in [SchemaKind::Config, SchemaKind::Declaration] {
            let v = schema(kind);
            assert_eq!(v["x-pbps-schema-version"], SCHEMA_VERSION);
            assert_eq!(v["x-pbps-tool-version"], env!("CARGO_PKG_VERSION"));
        }
    }
}
