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
    /// The `--format json` envelope of SPEC §9.8, one branch per command.
    Envelope,
}

/// The version of the *published schemas*, independent of the tool's.
///
/// Advance once per merged change to the published JSON content of any kind,
/// excluding only whitespace, object-key order and the tool-version stamp.
/// Archive the complete new set; keep previous archives unchanged (SPEC §14.2,
/// acceptance criterion 6, DECISIONS 465).
pub const SCHEMA_VERSION: u32 = 60;
// 60: preview rehearsal selects a shared scratch entry (#587).
// 59: explicit enum label rename annotations (#1635 slice 3a).
// 58: declared SQL Server cross-database dependencies and comparison reports (#2322).
// 57: historical state show, diff and export envelopes (issue #2289).
// 56: declared type rename annotations and deferred domain vetting (#1635 slice two).
// 55: SQL Server same_instance configuration and explain provenance (#2270).
// 54: declared enum and domain files (#1635 slice one).
// 27: a table's `unlogged:` (issue #1443).
// 28: a table's `system_time:` (issue #1176).
// 29: a table's `partition_by:` and `partitions:` (issue #1170).
// 31: a connected plan's `resolver_assessment` (issue #1515).
// 32: a partition's own `checks:` and `indexes:` under `partitions:` (issue #1577).
// 33: a partition's own `columns:` defaults and NOT NULLs under `partitions:` (issue #1578).
// 35: a foreign key's `restrict` action (issue #1633).
// 36: a server resolver's scratch `standard:` (issue #1708).
// 37: a server resolver's `baseline:` file (issue #1673).
// 38: a parent's `partitions: external` (issue #1533).
// 39: a Docker resolver's `baseline:`, and either's `outside_writes:` (issue #1674).
// 40: a column's `serial` types, `sequence:` and optional `nullable:` (issue #1631).
// 41: pbps.yml's `partition_runway:` (issue #1789).
// 42: a connected plan's `resolver_assessment` need `resolved` (issue #1516).
// 43: a connected plan's `time_zone` (issue #1916).
// 44: `explain`'s `resolver`: a resolved plan's sealed evidence (issue #1517).
// 45: a resolver entry's `externals:`, formerly `baseline:` (issue #2007).
// 46: an `identity:` mapping and a `sequence:`'s `min`, `max`, `cycle`, `cache` (issue #1921).
// 47: `partition_by:`'s `list:` and `hash:`, and a partition's `in:`, `modulus:`, `remainder:` (issue #1632).
// 50: a plan's `origins`, the steps a planning rule added (issue #1958).
// 51: a table's `row_security:` and `row_policies:` (issue #1172 PR A).
// 52: created-table row-security operations (issue #1172 PR C).
// 53: PostgreSQL type references, trusted_roles and reviewed input-path routines (#1523, #2181).
// 26: index, key and unique-constraint `storage_parameters` (issue #1442).
// 25: a table's `storage_parameters:` (issue #1441).
// 24: a table's `replica_identity:` (issue #1444).
// 23: a table's `computed:` (issue #1174).
// 22: a column's `generated:` (issue #1168).
// 21: an index's `keys:`, which may hold expression keys (issue #1169).
// 20: an index's `method:` and a key's operator class (issue #1169).
// 19: a column's `collation:` (issue #1175).
// 18: explain distinguishes unsupported apply from offline previews (issue #1217).
// 17: a table's `clustered:` layout selector (issue #1178).
// 16: `explain`'s `unchecked`, the checks a plan implies that cannot be asked
//     before it runs (issue #478).
// 15: an environment's `fingerprint_key_env` / `fingerprint_key_file`, where
//     its fingerprint key is read from (issue #952, DEC-952.1).
// 14: doctor's explicit paths-only projection, before declaration/identity or
//     environment reads, for contained isolated compose capture (issue #745).
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
// 8: a grant's permission is every spelling `Permission::from_str` folds, not
//    the canonical word alone. The closed list version 7 published refused
//    `SELECT` and `view_definition`, which the loader accepts, so an editor
//    notices in the way that matters: it stops flagging a valid file
//    (DECISIONS 213). A consumer keying on this number could not otherwise
//    tell the widened grammar from the version-7 enum.
// 9: the `envelope` kind exists. Nothing an editor completes changed, and no
//    published schema's contents moved; the number moves because a consumer
//    keying on it can now ask this binary for a schema a version-8 one would
//    have refused, and "which kinds does this binary publish" is exactly what
//    the stamp is for.
// 10: all accumulated schema changes after 9, including envelope constraints
//     and payloads. Versioned archives now pin each set independently of the
//     current checked-in copies (DECISIONS 465).
// 11: doctor's advisory resolver environment observations and qualification
//     gaps for PostgreSQL and SQL Server (ADR-0016; issue #597).
// 12: named resolver profiles and connected planning's unacquired selection
//     report (ADR-0016; issue #606).

/// Every command that emits an envelope, with the payload its `data` carries.
///
/// The list is the contract ADR-0015 decision 1 rests on. Nothing else names
/// it: the published schema is built from it, and the flow test reads the
/// command names back out of that schema and compares them with the commands
/// `--help` says take `--format json`, so a command added here without the
/// flag, or given the flag without a line here, is a failing test rather than
/// an envelope nothing describes.
macro_rules! envelope_branches {
    ($mac:ident) => {
        $mac! {
            "plan" => crate::PlanData,
            "validate" => crate::ValidateData,
            "fmt" => crate::FmtData,
            "explain" => crate::explain::Explanation,
            "doctor" => crate::doctor::DoctorData,
            "verify" => pbps_model::DriftReport,
            "status" => Vec<crate::status::EnvStatus>,
            "state list" => crate::state_list::StateListData,
            "state show" => crate::state_history::ShowData,
            "state diff" => crate::state_history::DiffData,
            "state export" => crate::state_history::ExportData,
        }
    };
}

/// The published schema of the envelope: one branch per command, selected by
/// the `command` field.
///
/// One document with a `oneOf` rather than one document per command, because
/// what a consumer holds is *an envelope*: it reads `command` and only then
/// knows what `data` is. Publishing them separately would make it choose the
/// schema before reading the field that decides which one applies.
///
/// Every branch is generated from the type the command serializes, the way the
/// other two kinds are generated from the loader's and the config's types, so
/// the published shape cannot drift from the emitted one.
fn envelope_schema() -> serde_json::Value {
    let mut defs = serde_json::Map::new();
    let mut branches = Vec::new();

    macro_rules! build {
        ($($name:literal => $ty:ty),* $(,)?) => {$({
            let mut doc =
                serde_json::to_value(schemars::schema_for!(crate::output::Report<$ty>))
                    .expect("a generated schema always serializes");
            let obj = doc.as_object_mut().expect("a generated schema is an object");

            // Each `schema_for!` brings its own `$defs`, and the shared types —
            // `Finding`, `Severity`, `Outcome` — appear in all of them under
            // the same name with the same body, so merging is safe. The assert
            // is what says so if it ever stops being true, rather than one
            // branch silently winning.
            if let Some(serde_json::Value::Object(d)) = obj.remove("$defs") {
                for (k, v) in d {
                    match defs.get(&k) {
                        Some(existing) => assert_eq!(
                            existing, &v,
                            "two envelope branches define `{k}` differently"
                        ),
                        None => {
                            defs.insert(k, v);
                        }
                    }
                }
            }
            obj.remove("$schema");
            obj.remove("title");
            // Two constants, for two different reasons.
            //
            // `command` is pinned to the one value that selects this branch.
            // Without it every branch would match every envelope and `oneOf`
            // would reject all of them for matching more than one.
            //
            // `schema_version` is pinned because of what SPEC §9.8 says the
            // field is: the version of the envelope alone, which "moves when a
            // consumer would have to change". Left as a plain integer, this
            // document accepted an envelope from a later version whose extra
            // fields it does not know — the schema saying "fine" about exactly
            // the case the field exists to refuse (DECISIONS 224).
            if let Some(serde_json::Value::Object(props)) = obj.get_mut("properties") {
                props.insert("command".to_owned(), serde_json::json!({ "const": $name }));
                props.insert(
                    "schema_version".to_owned(),
                    serde_json::json!({ "const": crate::output::SCHEMA_VERSION }),
                );
            }

            let def = format!("envelope.{}", $name.replace(' ', "-"));
            defs.insert(def.clone(), doc);
            branches.push(serde_json::json!({ "$ref": format!("#/$defs/{def}") }));
        })*};
    }
    envelope_branches!(build);

    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "pbps --format json envelope",
        "description":
            "One envelope per read-only command (SPEC 9.8). `command` selects which \
             branch applies, and `data` is that command's own payload.",
        "oneOf": branches,
        "$defs": defs,
    })
}

pub fn schema(kind: SchemaKind) -> serde_json::Value {
    let mut v = match kind {
        SchemaKind::Config => serde_json::to_value(schemars::schema_for!(pbps_config::Config)),
        SchemaKind::Declaration => {
            serde_json::to_value(schemars::schema_for!(pbps_load::dto::DeclarationFile))
        }
        SchemaKind::Envelope => Ok(envelope_schema()),
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

    #[test]
    fn dev_schema_permits_one_shared_or_anonymous_source() {
        let document = schema(SchemaKind::Config);
        let validator = jsonschema::validator_for(&document).unwrap();
        for (dev, valid) in [
            (serde_json::json!({"resolver":"scratch"}), true),
            (
                serde_json::json!({"resolver":"scratch", "docker":null}),
                true,
            ),
            (serde_json::json!({"docker":"sql"}), true),
            (serde_json::json!({"url_env":"SCRATCH"}), true),
            (serde_json::json!({}), false),
            (
                serde_json::json!({"resolver":"scratch", "docker":"sql"}),
                false,
            ),
            (
                serde_json::json!({"resolver":"scratch", "url_env":"SCRATCH"}),
                false,
            ),
        ] {
            let config = serde_json::json!({"dialect":"mssql", "dev":dev});
            assert_eq!(validator.is_valid(&config), valid, "{config}");
        }
    }

    /// The config schema and `Config::parse` agree on which `schema_dir`
    /// spellings name the project root or a directory above it (#739, review
    /// on #1548). Only the lexical half is the schema's to state: `a/..` is
    /// judged on disk, where a symlink can lead it elsewhere, so both accept
    /// it here.
    #[test]
    fn same_instance_schema_accepts_only_server_policy_values() {
        let schema = schema(SchemaKind::Config);
        let validator = jsonschema::validator_for(&schema).unwrap();
        for policy in [None, Some("allow"), Some("refuse")] {
            let mut config = serde_json::json!({"dialect":"mssql","resolvers":{"s":{"kind":"server","url_env":"S"}}});
            if let Some(policy) = policy {
                config["resolvers"]["s"]["same_instance"] = policy.into();
            }
            assert!(validator.is_valid(&config), "{config}");
        }
        for policy in [
            serde_json::json!("sometimes"),
            serde_json::Value::Null,
            serde_json::json!(true),
        ] {
            let config = serde_json::json!({"dialect":"mssql","resolvers":{"s":{"kind":"server","url_env":"S","same_instance":policy}}});
            assert!(!validator.is_valid(&config), "{config}");
        }
        let docker = serde_json::json!({"dialect":"mssql","resolvers":{"s":{"kind":"docker","image":"mssql:2025","same_instance":"allow"}}});
        assert!(!validator.is_valid(&docker));
    }

    #[test]
    fn config_schema_and_loader_agree_on_a_schema_dir_holding_the_project() {
        let document = schema(SchemaKind::Config);
        let validator = jsonschema::validator_for(&document).unwrap();
        for (dir, valid) in [
            ("schema", true),
            ("./schema", true),
            ("db/tables", true),
            ("../shared/schema", true),
            ("a/..", true),
            ("...", true),
            (".hidden", true),
            ("/abs/schema", true),
            ("", false),
            (".", false),
            ("./", false),
            ("./.", false),
            ("..", false),
            ("../..", false),
            ("./../.", false),
            (".//..//", false),
        ] {
            let config = serde_json::json!({"dialect": "mssql", "schema_dir": dir});
            assert_eq!(validator.is_valid(&config), valid, "{config}");
            assert_eq!(
                pbps_config::Config::parse(&config.to_string(), std::path::Path::new("pbps.yml"))
                    .is_ok(),
                valid,
                "{config}"
            );
        }
        // Windows separates with `\` too, and so does the schema, on every
        // platform: a config is portable, the platform reading it is not.
        let config = serde_json::json!({"dialect": "mssql", "schema_dir": "..\\.."});
        assert!(!validator.is_valid(&config), "{config}");
    }

    #[test]
    fn resolver_profile_schema_and_loader_agree_on_backends_and_policies() {
        let document = schema(SchemaKind::Config);
        let validator = jsonschema::validator_for(&document).unwrap();
        for (profile, valid) in [
            (
                serde_json::json!({"kind": "docker", "image": "postgres:18"}),
                true,
            ),
            (
                serde_json::json!({"kind": "docker", "image": "registry.local:5000/team/pg:18", "pull": "if_missing"}),
                true,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "SCRATCH_DB"}),
                true,
            ),
            (serde_json::json!({}), false),
            (serde_json::json!({"kind": "server", "url_env": ""}), false),
            (
                serde_json::json!({"kind": "server", "url_env": "postgres://secret@host/db"}),
                false,
            ),
            (
                serde_json::json!({"kind": "server", "url": "postgres://secret@host/db"}),
                false,
            ),
            (serde_json::json!({"kind": "docker", "image": ""}), false),
            (
                serde_json::json!({"kind": "docker", "image": "https://registry/pg"}),
                false,
            ),
            (
                serde_json::json!({"kind": "docker", "image": "pg:18 --privileged"}),
                false,
            ),
            (
                serde_json::json!({"kind": "docker", "image": "pg:18", "url_env": "SCRATCH"}),
                false,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "SCRATCH", "pull": "never"}),
                false,
            ),
            (
                serde_json::json!({"kind": "docker", "image": "pg:18", "pull": "always"}),
                false,
            ),
            (
                serde_json::json!({"kind": "docker", "image": "pg:18", "verified": true}),
                false,
            ),
            // A scratch standard's domain, in both (#1708 review).
            (
                serde_json::json!({"kind": "server", "url_env": "S", "standard": {"connection_limit": -1, "public": {"grants": [{"to": "r", "privileges": ["USAGE"]}]}}}),
                true,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "S", "standard": {"connection_limit": 1}}),
                true,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "S", "standard": {"connection_limit": 0}}),
                false,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "S", "standard": {"connection_limit": -2}}),
                false,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "S", "standard": {"connection_limit": null}}),
                false,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "S", "standard": {"public": {"grants": [{"to": "r", "privileges": []}]}}}),
                false,
            ),
            // An externals file, in both (#1673, #2007).
            (
                serde_json::json!({"kind": "server", "url_env": "S", "externals": "db/externals.sql"}),
                true,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "S", "externals": ""}),
                false,
            ),
            // Null is an omitted externals file, in both (#1673 review).
            (
                serde_json::json!({"kind": "server", "url_env": "S", "externals": null}),
                true,
            ),
            (
                serde_json::json!({"kind": "docker", "image": "pg:18", "externals": "db/externals.sql"}),
                true,
            ),
            // The field's former name, refused by both (#2007).
            (
                serde_json::json!({"kind": "server", "url_env": "S", "baseline": "db/baseline.sql"}),
                false,
            ),
            // An externals file's outside-write choice, on either kind (#1674).
            (
                serde_json::json!({"kind": "docker", "image": "pg:18", "outside_writes": "allow"}),
                true,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "S", "outside_writes": "refuse"}),
                true,
            ),
            (
                serde_json::json!({"kind": "server", "url_env": "S", "outside_writes": "sometimes"}),
                false,
            ),
        ] {
            let config =
                serde_json::json!({"dialect": "postgres", "resolvers": {"scratch": profile}});
            assert_eq!(validator.is_valid(&config), valid, "{config}");
            assert_eq!(
                pbps_config::Config::parse(&config.to_string(), std::path::Path::new("pbps.yml"))
                    .is_ok(),
                valid,
                "{config}"
            );
        }
        for config in [
            serde_json::json!({"dialect": "mssql", "resolve_with": ""}),
            serde_json::json!({"dialect": "mssql", "resolvers": {"bad name": {"kind": "server", "url_env": "SCRATCH"}}}),
            serde_json::json!({"dialect": "mssql", "environments": {"prod": {"url_env": "TARGET", "resolve_with": "bad/name"}}}),
        ] {
            assert!(!validator.is_valid(&config), "{config}");
            assert!(
                pbps_config::Config::parse(&config.to_string(), std::path::Path::new("pbps.yml"))
                    .is_err(),
                "{config}"
            );
        }
    }

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
            (SchemaKind::Envelope, "envelope.schema.json"),
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

    #[test]
    fn dependency_schema_and_loader_agree_on_shapes_and_named_refusals() {
        let validator = jsonschema::validator_for(&schema(SchemaKind::Declaration)).unwrap();
        let valid = serde_json::json!({
            "dependency": "otherdb.dbo.customers", "kind": "table",
            "columns": {"id": {"type": "int", "nullable": false}, "name": {"type": "nvarchar(100)"}}
        });
        for kind in ["table", "view"] {
            let mut value = valid.clone();
            value["kind"] = kind.into();
            assert!(validator.is_valid(&value));
            assert!(
                pbps_load::load_file_str(
                    std::path::Path::new("dependency.yml"),
                    &value.to_string()
                )
                .is_ok()
            );
        }
        for (pointer, bad) in [
            ("/dependency", serde_json::json!("dbo.customers")),
            ("/kind", serde_json::json!("procedure")),
            ("/columns", serde_json::json!({})),
            ("/columns/id/nullable", serde_json::Value::Null),
        ] {
            let mut value = valid.clone();
            *value.pointer_mut(pointer).unwrap() = bad;
            assert!(!validator.is_valid(&value), "{value}");
            assert!(
                pbps_load::load_file_str(
                    std::path::Path::new("dependency.yml"),
                    &value.to_string()
                )
                .is_err()
            );
        }
    }

    /// Current output and the mutable copies may drift together. An archived
    /// document selected by the version does not drift with either of them.
    #[test]
    fn a_published_version_keeps_its_archived_schema_contract() {
        for (kind, file) in [
            (SchemaKind::Declaration, "declaration.schema.json"),
            (SchemaKind::Config, "pbps.yml.schema.json"),
            (SchemaKind::Envelope, "envelope.schema.json"),
        ] {
            let mut current = schema(kind);
            let mut published = archived_schema(SCHEMA_VERSION, file);
            // This annotation identifies a build, not the schema contract.
            current
                .as_object_mut()
                .unwrap()
                .remove("x-pbps-tool-version");
            published
                .as_object_mut()
                .unwrap()
                .remove("x-pbps-tool-version");
            assert!(
                current == published,
                "{file} changed under published schema version {SCHEMA_VERSION}; \
                 bump the schema-set version and add its archive, keeping existing archives"
            );
        }
    }

    fn archived_schema(version: u32, file: &str) -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/published-schemas")
            .join(version.to_string())
            .join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing published schema archive {}: {e}", path.display()));
        let document: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            document["x-pbps-schema-version"],
            version,
            "{}",
            path.display()
        );
        document
    }

    /// Version 9 was already reused for incompatible documents. Keep one real
    /// legacy publication to prove why the current contract needs a new key.
    #[test]
    fn a_changed_envelope_contract_has_a_distinct_published_version() {
        let legacy = archived_schema(9, "envelope.schema.json");
        let current = schema(SchemaKind::Envelope);
        let old_validator = jsonschema::validator_for(&legacy).unwrap();
        let new_validator = jsonschema::validator_for(&current).unwrap();
        let mut envelope = serde_json::json!({
            "schema_version": 1,
            "tool_version": "0.0.0",
            "command": "state list",
            "result": "ok",
            "findings": [],
            "data": {
                "environment": "test",
                "initialized": true,
                "limit": 1,
                "entries": []
            }
        });
        assert!(old_validator.is_valid(&envelope));
        assert!(new_validator.is_valid(&envelope));
        envelope["data"]["limit"] = 0.into();
        assert!(old_validator.is_valid(&envelope));
        assert!(!new_validator.is_valid(&envelope));
        assert_ne!(
            legacy["x-pbps-schema-version"],
            current["x-pbps-schema-version"]
        );
        // The envelope's own wire version is independent of the schema set.
        envelope["data"]["limit"] = 1.into();
        envelope["schema_version"] = 2.into();
        assert!(!old_validator.is_valid(&envelope));
        assert!(!new_validator.is_valid(&envelope));
    }

    /// An additive `data` field keeps the envelope's wire version only where
    /// the published schema leaves an object open: a consumer validating
    /// against the older document still accepts the envelope that carries it
    /// (DEC-997.1). An object that constrains the properties it does not name
    /// (through `additionalProperties`, `unevaluatedProperties`,
    /// `patternProperties`, `propertyNames` or `maxProperties`) refuses some
    /// additions, so each one is named here on purpose.
    ///
    /// `ResolverProfile` is the configuration's own type, echoed by a connected
    /// plan's resolver selection, and stays closed so `pbps.yml` refuses a
    /// misspelt key; so do the server entry's `standard:` types within it
    /// (#1708). `Discovery`'s two maps accept new keys whose values are
    /// `Observation`s, and nothing else. `TypeRoutine` retains the saved
    /// plan's closed source-record shape (DEC-2181.2). Dependency records
    /// retain their closed saved shape and three-part map-key grammar.
    #[test]
    fn only_the_named_envelope_objects_constrain_unnamed_properties() {
        let constrained = constrained_objects(&schema(SchemaKind::Envelope));
        let names: Vec<&str> = constrained.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "Dependency",
                "DependencyColumn",
                "Discovery/properties/observations",
                "Discovery/properties/qualification",
                "Explanation/properties/dependencies",
                "PlanData/properties/dependencies",
                "PlannedDependency",
                "PublicGrant",
                "PublicStandard",
                "ResolverProfile/oneOf/0",
                "ResolverProfile/oneOf/1",
                "ScratchStandard",
                "TypeRoutine",
            ]
        );
    }

    #[test]
    fn a_closed_or_schema_valued_object_is_constrained_and_an_open_one_is_not() {
        let document = serde_json::json!({
            "oneOf": [{ "additionalProperties": false }],
            "$defs": {
                "Open": { "type": "object", "additionalProperties": true },
                "Empty": { "type": "object", "additionalProperties": {} },
                "Unset": { "type": "object", "properties": { "a": {} } },
                "Map": { "type": "object", "additionalProperties": { "type": "string" } },
                "Named": { "type": "object", "propertyNames": { "pattern": "^a" } },
                "Bounded": { "type": "object", "maxProperties": 2 },
                "Patterned": {
                    "type": "object",
                    "patternProperties": { "^x": { "type": "string" } }
                },
                "Unevaluated": { "type": "object", "unevaluatedProperties": false },
                "Nested": { "oneOf": [{ "properties": {
                    "inner": {
                        "description": "dropped before comparison",
                        "title": "dropped too",
                        "type": "object",
                        "properties": { "description": { "type": "string" } },
                        "additionalProperties": false
                    }
                } }] },
            }
        });
        let constrained = constrained_objects(&document);
        let names: Vec<&str> = constrained.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "Bounded",
                "Map",
                "Named",
                "Nested/oneOf/0/properties/inner",
                "Patterned",
                "Unevaluated"
            ]
        );
        assert_eq!(
            constrained["Nested/oneOf/0/properties/inner"],
            serde_json::json!({
                "type": "object",
                "properties": { "description": { "type": "string" } },
                "additionalProperties": false
            })
        );
    }

    /// Keywords whose value is instance data that validation compares
    /// against, literals or property names: nothing beneath them is a
    /// keyword, so it is kept verbatim.
    const LITERALS: [&str; 3] = ["const", "enum", "dependentRequired"];
    /// Annotation keywords: they change no validation, so a change to one
    /// never needs the wire version to move.
    const ANNOTATIONS: [&str; 8] = [
        "description",
        "title",
        "$comment",
        "default",
        "examples",
        "deprecated",
        "readOnly",
        "writeOnly",
    ];
    /// Keywords whose value maps names to schemas: the keys are names, never
    /// keywords, and each value is a schema of its own.
    const NAME_MAPS: [&str; 4] = [
        "properties",
        "patternProperties",
        "$defs",
        "dependentSchemas",
    ];

    /// Each schema directly beneath one keyword's value, with its pointer
    /// segment: a name map's entries, an array's items, or the value itself.
    fn subschemas<'a>(
        key: &'a str,
        value: &'a serde_json::Value,
    ) -> Vec<(String, &'a serde_json::Value)> {
        let escape = |segment: &str| segment.replace('~', "~0").replace('/', "~1");
        match value {
            _ if LITERALS.contains(&key) || ANNOTATIONS.contains(&key) => Vec::new(),
            serde_json::Value::Object(named) if NAME_MAPS.contains(&key) => named
                .iter()
                .map(|(name, schema)| (format!("{}/{}", escape(key), escape(name)), schema))
                .collect(),
            serde_json::Value::Object(_) => vec![(escape(key), value)],
            serde_json::Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, schema)| (format!("{}/{i}", escape(key)), schema))
                .collect(),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => Vec::new(),
        }
    }

    /// A schema with its annotation keywords removed. A property named like
    /// one, and a `const` or `enum` literal holding one, are kept.
    fn without_annotations(schema: &serde_json::Value) -> serde_json::Value {
        let serde_json::Value::Object(map) = schema else {
            return schema.clone();
        };
        map.iter()
            .filter(|(key, _)| !ANNOTATIONS.contains(&key.as_str()))
            .map(|(key, value)| {
                let value = match value {
                    _ if LITERALS.contains(&key.as_str()) => value.clone(),
                    serde_json::Value::Object(named) if NAME_MAPS.contains(&key.as_str()) => named
                        .iter()
                        .map(|(name, s)| (name.clone(), without_annotations(s)))
                        .collect(),
                    serde_json::Value::Array(items) => {
                        items.iter().map(without_annotations).collect()
                    }
                    serde_json::Value::Object(_) => without_annotations(value),
                    serde_json::Value::Null
                    | serde_json::Value::Bool(_)
                    | serde_json::Value::Number(_)
                    | serde_json::Value::String(_) => value.clone(),
                };
                (key.clone(), value)
            })
            .collect()
    }

    /// Every object under `$defs` that constrains the properties it does not
    /// name, keyed by its JSON pointer below `$defs`, without annotations.
    fn constrained_objects(
        document: &serde_json::Value,
    ) -> std::collections::BTreeMap<String, serde_json::Value> {
        // A keyword constrains unnamed properties unless it is absent,
        // `true` or `{}`, the three spellings of "anything".
        fn restricts(value: Option<&serde_json::Value>) -> bool {
            match value {
                Some(serde_json::Value::Bool(allowed)) => !allowed,
                Some(serde_json::Value::Object(schema)) => !schema.is_empty(),
                Some(_) => true,
                None => false,
            }
        }
        fn constrains(map: &serde_json::Map<String, serde_json::Value>) -> bool {
            restricts(map.get("additionalProperties"))
                || restricts(map.get("unevaluatedProperties"))
                || restricts(map.get("propertyNames"))
                || map.contains_key("maxProperties")
                || map
                    .get("patternProperties")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|patterns| !patterns.is_empty())
        }
        fn walk(
            schema: &serde_json::Value,
            path: String,
            found: &mut std::collections::BTreeMap<String, serde_json::Value>,
        ) {
            let serde_json::Value::Object(map) = schema else {
                return;
            };
            if constrains(map) {
                found.insert(path, without_annotations(schema));
                return;
            }
            for (key, value) in map {
                for (segment, sub) in subschemas(key, value) {
                    walk(sub, format!("{path}/{segment}"), found);
                }
            }
        }
        let mut found = std::collections::BTreeMap::new();
        let defs = &document["$defs"];
        assert!(
            defs.is_object(),
            "a published schema keeps its definitions in `$defs`"
        );
        for (segment, body) in subschemas("$defs", defs) {
            let path = segment
                .strip_prefix("$defs/")
                .expect("a `$defs` segment")
                .to_owned();
            walk(body, path, &mut found);
        }
        found
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
            "RowPolicyDto",
            "RowSecurityDto",
        ] {
            assert_eq!(
                defs[name]["additionalProperties"],
                serde_json::json!(false),
                "{name} must refuse unknown fields: {v}"
            );
        }
    }

    #[test]
    fn type_declaration_schema_and_loader_agree_on_kinds_and_unique_labels() {
        let validator = jsonschema::validator_for(&schema(SchemaKind::Declaration)).unwrap();
        for (value, valid) in [
            (serde_json::json!({"enum": "app.e", "values": []}), true),
            (
                serde_json::json!({"enum": "app.e", "values": ["sad", "ok"]}),
                true,
            ),
            (
                serde_json::json!({"domain": "app.d", "type": "varchar(20)", "checks": {"c": "VALUE <> ''"}}),
                true,
            ),
            (
                serde_json::json!({"enum": "app.e", "values": ["x", "x"]}),
                false,
            ),
            (
                serde_json::json!({"enum": "app.e", "values": ["x"], "unknown": true}),
                false,
            ),
            (
                serde_json::json!({"enum": "app.e", "domain": "app.d", "values": ["x"]}),
                false,
            ),
            (
                serde_json::json!({"domain": "app.d", "type": "text", "values": ["x"]}),
                false,
            ),
            (serde_json::json!({"domain": "app.d"}), false),
        ] {
            assert_eq!(validator.is_valid(&value), valid, "{value}");
            assert_eq!(
                pbps_load::load_type_str(std::path::Path::new("type.yml"), &value.to_string())
                    .is_ok(),
                valid,
                "{value}"
            );
        }
    }

    #[test]
    fn sequence_grants_use_the_published_schemas_existing_target_key_shape() {
        let document = schema(SchemaKind::Declaration);
        let validator = jsonschema::validator_for(&document).unwrap();
        for (permissions, valid) in [
            (serde_json::json!(["usage"]), true),
            (serde_json::json!(["select", "update"]), true),
            (serde_json::json!(["serial-sequences"]), false),
            (serde_json::json!(["unknown"]), false),
            (serde_json::json!("usage"), false),
        ] {
            let value = serde_json::json!({"role": "reader", "grants": {"sequence::app.orders.id": permissions}});
            assert_eq!(validator.is_valid(&value), valid, "{value}");
            let yaml = serde_json::to_string(&value).unwrap();
            assert_eq!(
                pbps_load::load_role_str(std::path::Path::new("reader.yml"), &yaml).is_ok(),
                valid,
                "{value}"
            );
        }
    }

    #[test]
    fn row_security_declaration_schema_and_loader_agree() {
        let validator = jsonschema::validator_for(&schema(SchemaKind::Declaration)).unwrap();
        for (policy, valid) in [
            (serde_json::json!({}), true),
            (serde_json::json!({"to": "public"}), true),
            (serde_json::json!({"to": ["PUBLIC", "app"]}), true),
            (
                serde_json::json!({"command": "update", "restrictive": true, "using": "id > 0", "with_check": "id > 0"}),
                true,
            ),
            (
                serde_json::json!({"command": "insert", "with_check": "id > 0"}),
                true,
            ),
            (
                serde_json::json!({"command": "delete", "using": "id > 0"}),
                true,
            ),
            (
                serde_json::json!({"command": "select", "using": "id > 0", "with_check": null}),
                true,
            ),
            (
                serde_json::json!({"command": "insert", "using": "id > 0"}),
                false,
            ),
            (
                serde_json::json!({"command": "select", "with_check": "id > 0"}),
                false,
            ),
            (
                serde_json::json!({"command": "delete", "with_check": "id > 0"}),
                false,
            ),
            (serde_json::json!({"command": "truncate"}), false),
            (serde_json::json!({"to": []}), false),
            (serde_json::json!({"to": ["public", "app"]}), false),
            (serde_json::json!({"to": ["current_user"]}), false),
            (serde_json::json!({"to": ["session_user"]}), false),
            (serde_json::json!({"to": ["current_role"]}), false),
            (serde_json::json!({"to": "app"}), false),
            (serde_json::json!({"to": null}), false),
            (serde_json::json!({"with_chek": "true"}), false),
        ] {
            let declaration = serde_json::json!({
                "table": "app.t", "columns": {"id": {"type": "integer"}},
                "row_policies": {"p": policy},
            });
            assert_eq!(validator.is_valid(&declaration), valid, "schema: {policy}");
            assert_eq!(
                pbps_load::load_table_str(std::path::Path::new("t.yml"), &declaration.to_string())
                    .is_ok(),
                valid,
                "loader: {policy}"
            );
        }
        for security in [
            serde_json::json!({}),
            serde_json::json!({"enabled": true, "forced": false}),
            serde_json::json!({"enabled": false, "forced": true}),
        ] {
            let declaration = serde_json::json!({"table": "app.t", "columns": {"id": {"type": "integer"}}, "row_security": security});
            assert!(validator.is_valid(&declaration));
            assert!(
                pbps_load::load_table_str(std::path::Path::new("t.yml"), &declaration.to_string())
                    .is_ok()
            );
        }
        for security in [
            serde_json::json!({"enable": true}),
            serde_json::json!({"enabled": null}),
        ] {
            let declaration = serde_json::json!({"table": "app.t", "columns": {"id": {"type": "integer"}}, "row_security": security});
            assert!(!validator.is_valid(&declaration));
            assert!(
                pbps_load::load_table_str(std::path::Path::new("t.yml"), &declaration.to_string())
                    .is_err()
            );
        }
    }

    /// The declaration schema and the loader agree on a `partitions:` entry
    /// (#1577): `from:` and `to:` both, or `default: true` alone, beside what a
    /// partition has of its own. Optional fields alone blessed `{}`.
    #[test]
    fn declaration_schema_and_loader_agree_on_a_partition_entry() {
        let document = schema(SchemaKind::Declaration);
        let validator = jsonschema::validator_for(&document).unwrap();
        for (entry, valid) in [
            (serde_json::json!("default"), true),
            (
                serde_json::json!({"from": ["MINVALUE"], "to": ["2025-01-01"]}),
                true,
            ),
            (
                serde_json::json!({"from": [1], "to": [2], "default": false,
                    "checks": {"c": "ts > 0"},
                    "indexes": {"i": {"columns": ["ts"]}}}),
                true,
            ),
            (serde_json::json!({"default": true}), true),
            (
                serde_json::json!({"default": true, "from": null, "indexes": {"i": {"columns": ["ts"]}}}),
                true,
            ),
            (serde_json::json!({}), false),
            (serde_json::json!({"from": [1]}), false),
            (serde_json::json!({"to": [1]}), false),
            (serde_json::json!({"from": null, "to": [1]}), false),
            (serde_json::json!({"default": false}), false),
            (serde_json::json!({"checks": {"c": "ts > 0"}}), false),
            (
                serde_json::json!({"default": true, "from": [1], "to": [2]}),
                false,
            ),
            (serde_json::json!({"default": true, "to": [2]}), false),
            (
                serde_json::json!({"from": [1], "to": [2], "default": true}),
                false,
            ),
            (serde_json::json!({"default": true, "columns": {}}), true),
            // A partition's own columns (#1578): a default, NOT NULL, or both,
            // and never a type or `nullable: true`.
            (
                serde_json::json!({"default": true, "columns": {"ts": {"default": "0"}}}),
                true,
            ),
            (
                serde_json::json!({"default": true, "columns": {"ts": {"nullable": false}}}),
                true,
            ),
            (
                serde_json::json!({"default": true,
                    "columns": {"ts": {"default": "0", "nullable": false}}}),
                true,
            ),
            (
                serde_json::json!({"default": true,
                    "columns": {"ts": {"default": "0", "nullable": null}}}),
                true,
            ),
            (
                serde_json::json!({"default": true,
                    "columns": {"ts": {"default": null, "nullable": false}}}),
                true,
            ),
            (
                serde_json::json!({"default": true, "columns": {"ts": {}}}),
                false,
            ),
            (
                serde_json::json!({"default": true, "columns": {"ts": {"default": null}}}),
                false,
            ),
            (
                serde_json::json!({"default": true, "columns": {"ts": {"nullable": null}}}),
                false,
            ),
            (
                serde_json::json!({"default": true, "columns": {"ts": {"nullable": true}}}),
                false,
            ),
            (
                serde_json::json!({"default": true, "columns": {"ts": {"type": "int"}}}),
                false,
            ),
        ] {
            let table = serde_json::json!({
                "table": "app.ev",
                "columns": {"ts": {"type": "int", "nullable": false}},
                "partition_by": ["ts"],
                "partitions": {"p": entry},
            });
            assert_eq!(validator.is_valid(&table), valid, "schema: {entry}");
            // JSON is YAML: the same document, read by the loader.
            assert_eq!(
                pbps_load::load_table_str(std::path::Path::new("t.yml"), &table.to_string())
                    .is_ok(),
                valid,
                "loader: {entry}"
            );
        }
    }

    /// The declaration schema and the loader agree on a LIST or HASH parent
    /// and its entries (#1632): exactly one strategy and exactly one bound,
    /// another strategy's or bound's key absent or `null`, as serde reads it.
    #[test]
    fn declaration_schema_and_loader_agree_on_list_and_hash_partitions() {
        let document = schema(SchemaKind::Declaration);
        let validator = jsonschema::validator_for(&document).unwrap();
        let agree = |table: serde_json::Value, valid: bool, what: &str| {
            assert_eq!(validator.is_valid(&table), valid, "schema: {what}");
            assert_eq!(
                pbps_load::load_table_str(std::path::Path::new("t.yml"), &table.to_string())
                    .is_ok(),
                valid,
                "loader: {what}"
            );
        };
        for (by, valid) in [
            (serde_json::json!({"list": ["ts"]}), true),
            (serde_json::json!({"hash": ["ts"]}), true),
            (serde_json::json!({"list": ["ts"], "hash": null}), true),
            (serde_json::json!({"hash": ["ts"], "list": null}), true),
            (serde_json::json!({"list": null}), false),
            (serde_json::json!({"list": ["ts"], "hash": ["ts"]}), false),
            (serde_json::json!({}), false),
            (serde_json::json!({"range": ["ts"]}), false),
        ] {
            let table = serde_json::json!({
                "table": "app.ev",
                "columns": {"ts": {"type": "int", "nullable": false}},
                "partition_by": by,
            });
            agree(table, valid, &by.to_string());
        }
        for (entry, valid) in [
            (serde_json::json!({"in": ["a", 1, true, null]}), true),
            (
                serde_json::json!({"in": ["a"], "modulus": null, "remainder": null,
                    "from": null, "to": null, "default": false}),
                true,
            ),
            (serde_json::json!({"modulus": 4, "remainder": 0}), true),
            (
                serde_json::json!({"modulus": 4, "remainder": 0, "in": null, "default": false}),
                true,
            ),
            (
                serde_json::json!({"from": [1], "to": [2], "in": null, "modulus": null}),
                true,
            ),
            (serde_json::json!({"default": true, "in": null}), true),
            (serde_json::json!({"in": null}), false),
            (serde_json::json!({"modulus": 4}), false),
            (serde_json::json!({"modulus": 4, "remainder": null}), false),
            (
                serde_json::json!({"in": ["a"], "modulus": 4, "remainder": 0}),
                false,
            ),
            (
                serde_json::json!({"in": ["a"], "from": [1], "to": [2]}),
                false,
            ),
            (serde_json::json!({"in": ["a"], "default": true}), false),
            (serde_json::json!({"in": [1.5]}), false),
        ] {
            let table = serde_json::json!({
                "table": "app.ev",
                "columns": {"ts": {"type": "int", "nullable": false}},
                "partition_by": {"list": ["ts"]},
                "partitions": {"p": entry},
            });
            agree(table, valid, &entry.to_string());
        }
    }

    /// The declaration schema and the loader agree on `partitions:` as a
    /// whole (#1533): a mapping of partitions, or the one word `external`.
    #[test]
    fn declaration_schema_and_loader_agree_on_external_partitions() {
        let document = schema(SchemaKind::Declaration);
        let validator = jsonschema::validator_for(&document).unwrap();
        for (partitions, valid) in [
            (serde_json::json!("external"), true),
            (serde_json::json!({}), true),
            (serde_json::json!({"p": "default"}), true),
            (serde_json::json!("externl"), false),
            (serde_json::json!("EXTERNAL"), false),
            (serde_json::json!(["external"]), false),
            (serde_json::json!(true), false),
        ] {
            let table = serde_json::json!({
                "table": "app.ev",
                "columns": {"ts": {"type": "int", "nullable": false}},
                "partition_by": ["ts"],
                "partitions": partitions,
            });
            assert_eq!(validator.is_valid(&table), valid, "schema: {partitions}");
            assert_eq!(
                pbps_load::load_table_str(std::path::Path::new("t.yml"), &table.to_string())
                    .is_ok(),
                valid,
                "loader: {partitions}"
            );
        }
    }

    /// The configuration schema and the loader agree on `partition_runway:`
    /// (#1789): a count of at least one partition, or a width as text.
    #[test]
    fn config_schema_and_loader_agree_on_partition_runway() {
        let validator = jsonschema::validator_for(&schema(SchemaKind::Config)).unwrap();
        for (value, valid) in [
            (serde_json::json!(3), true),
            (serde_json::json!(1), true),
            (serde_json::json!("2month"), true),
            (serde_json::json!("3 weeks"), true),
            (serde_json::json!(0), false),
            (serde_json::json!(-1), false),
            (serde_json::json!(1.5), false),
            (serde_json::json!([3]), false),
            (serde_json::json!({"count": 3}), false),
            (serde_json::json!(true), false),
        ] {
            let config = serde_json::json!({"dialect": "postgres", "partition_runway": value});
            assert_eq!(validator.is_valid(&config), valid, "schema: {value}");
            assert_eq!(
                pbps_config::Config::parse(&config.to_string(), std::path::Path::new("pbps.yml"))
                    .is_ok(),
                valid,
                "loader: {value}"
            );
        }
    }

    /// The point of generating rather than writing: the words a user types come
    /// from the enum the loader converts into.
    #[test]
    fn the_schema_lists_the_words_the_loader_accepts() {
        let v = schema(SchemaKind::Declaration);
        let actions = serde_json::to_string(&v["$defs"]["ReferentialAction"]).unwrap();
        for word in [
            "no_action",
            "restrict",
            "cascade",
            "set_null",
            "set_default",
        ] {
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
        let listed = completed_permissions();
        let all: Vec<String> = pbps_model::Permission::ALL
            .iter()
            .map(|p| p.as_str().to_owned())
            .collect();
        assert_eq!(listed, all);
        assert!(
            listed.iter().any(|w| w == "usage") && listed.iter().any(|w| w == "view-definition")
        );
        assert!(!listed.iter().any(|w| w == "control"));
    }

    /// The `items` schema for a grant's permissions.
    fn permission_items() -> serde_json::Value {
        schema(SchemaKind::Declaration)["$defs"]["RoleDto"]["properties"]["grants"]
            ["additionalProperties"]["items"]
            .clone()
    }

    /// The closed list an editor completes from: the canonical words, the ones
    /// `fmt` writes.
    fn completed_permissions() -> Vec<String> {
        let items = permission_items();
        let branches = items["anyOf"]
            .as_array()
            .unwrap_or_else(|| panic!("a grant's permissions have no branches: {items}"));
        let listed = branches
            .iter()
            .find_map(|b| b["enum"].as_array())
            .unwrap_or_else(|| panic!("no branch completes from a closed list: {items}"));
        listed
            .iter()
            .map(|v| v.as_str().expect("a permission is a string").to_owned())
            .collect()
    }

    /// The branch that says which spellings validate, compiled.
    fn accepted_spellings() -> regex_lite::Regex {
        let items = permission_items();
        let pattern = items["anyOf"]
            .as_array()
            .and_then(|b| b.iter().find_map(|b| b["pattern"].as_str()))
            .unwrap_or_else(|| panic!("no branch says which spellings are accepted: {items}"));
        regex_lite::Regex::new(pattern).expect("the schema's pattern is a regex")
    }

    /// Every spelling the loader folds into a permission has to validate, or a
    /// schema-aware editor flags what `pbps validate` accepts — and the editor
    /// is what a user reads first. `Permission::from_str` folds case and reads
    /// `_` or a space where the canonical word has `-`, so that a user who
    /// types what the engine prints is not corrected (ADR-0010 §6). The
    /// spellings are enumerated from `Permission::ALL`, so the schema cannot
    /// drift from the loader.
    #[test]
    fn every_spelling_the_loader_accepts_validates_against_the_schema() {
        use std::str::FromStr;
        let re = accepted_spellings();
        for permission in pbps_model::Permission::ALL {
            let word = permission.as_str();
            for spelling in [
                word.to_owned(),
                word.to_ascii_uppercase(),
                word.replace('-', "_"),
                word.replace('-', " "),
                word.to_ascii_uppercase().replace('-', " "),
                format!("  {word}  "),
            ] {
                // Both halves: asserting only the pattern would let the test
                // pass over a spelling the loader does not actually take.
                assert_eq!(
                    pbps_model::Permission::from_str(&spelling),
                    Ok(permission),
                    "the loader reads `{spelling}`"
                );
                assert!(re.is_match(&spelling), "the schema takes `{spelling}`");
            }
        }
    }

    /// The padding the schema allows is what `trim` removes.
    ///
    /// The trap this pins is `\s`: JSON Schema's patterns are ECMA-262, whose
    /// `\s` leaves out U+0085 — which `trim` removes — and takes in U+FEFF,
    /// which `trim` leaves in place. Writing the pattern with it would have
    /// broken both halves of "the schema is the loader" (DECISIONS 172) at
    /// once: refusing a padded declaration `pbps validate` accepts, and
    /// blessing one it refuses. The padding characters are enumerated from
    /// `char::is_whitespace`, which is what `trim` asks, so the set follows a
    /// later Unicode table wherever `trim` goes; `dto.rs` pins the published
    /// class against the same predicate, character for character.
    #[test]
    fn the_schema_pads_a_permission_with_what_trim_removes_and_nothing_else() {
        use std::str::FromStr;
        let re = accepted_spellings();
        let refused = [
            // The two ECMA-262 `\s` would have got wrong, and three more
            // characters an editor's user could mistake for a space.
            '\u{feff}', '\u{200b}', '\u{180e}', '\u{2060}', '-',
        ];
        for c in (char::MIN..=char::MAX)
            .filter(|c| c.is_whitespace())
            .chain(refused)
        {
            let padded = format!("{c}select{c}");
            let loader = pbps_model::Permission::from_str(&padded).is_ok();
            assert_eq!(
                loader,
                c.is_whitespace(),
                "`trim` and `is_whitespace` disagree about U+{:04X}",
                c as u32
            );
            assert_eq!(
                re.is_match(&padded),
                loader,
                "the schema and the loader disagree about U+{:04X} as padding",
                c as u32
            );
        }
    }

    /// The half that still refuses. A pattern that took the near misses would
    /// autocomplete correctly and validate nothing — which is where this
    /// started: a `grants` list that accepted any string blessed `contrl`.
    #[test]
    fn a_word_the_loader_refuses_does_not_validate_against_the_schema() {
        use std::str::FromStr;
        let re = accepted_spellings();
        for word in [
            "contrl",
            "control",
            "sel ect",
            "view--definition",
            "view-definitions",
            "viewdefinition",
            "",
        ] {
            assert!(
                pbps_model::Permission::from_str(word).is_err(),
                "the loader refuses `{word}`"
            );
            assert!(!re.is_match(word), "the schema refuses `{word}`");
        }
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

    /// Every schema carries its version, so a copy found on disk can say
    /// whether it is the one this binary would produce.
    #[test]
    fn every_schema_is_stamped_with_its_version() {
        for kind in [
            SchemaKind::Config,
            SchemaKind::Declaration,
            SchemaKind::Envelope,
        ] {
            let v = schema(kind);
            assert_eq!(v["x-pbps-schema-version"], SCHEMA_VERSION);
            assert_eq!(v["x-pbps-tool-version"], env!("CARGO_PKG_VERSION"));
        }
    }
}
