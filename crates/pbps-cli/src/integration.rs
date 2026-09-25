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
pub const SCHEMA_VERSION: u32 = 16;
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
    /// (DEC-997.1). An object that constrains the properties it does not name —
    /// `additionalProperties: false`, or a schema every unnamed value must
    /// match — refuses some additions, so each one is named here on purpose.
    ///
    /// `ResolverProfile` is the configuration's own type, echoed by a connected
    /// plan's resolver selection, and stays closed so `pbps.yml` refuses a
    /// misspelt key. `Discovery`'s two maps accept new keys whose values are
    /// `Observation`s, and nothing else.
    #[test]
    fn only_the_named_envelope_objects_constrain_unnamed_properties() {
        let constrained = constrained_objects(&schema(SchemaKind::Envelope));
        let names: Vec<&str> = constrained.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "Discovery/properties/observations",
                "Discovery/properties/qualification",
                "ResolverProfile/oneOf/0",
                "ResolverProfile/oneOf/1",
            ]
        );
    }

    /// Naming the constrained objects says where an addition is breaking; this
    /// says that such a change moves the wire version. Every archived envelope
    /// document stamped with the current `output::SCHEMA_VERSION` must still
    /// accept each constrained object this build publishes (`still_accepted`,
    /// descriptions aside). The archives never change (DECISIONS 465), so a
    /// change the old document would refuse fails here until the wire version
    /// and the schema's `const` move with it (DECISIONS 224, DEC-997.1).
    #[test]
    fn a_constrained_object_changes_only_with_the_wire_version() {
        let current = schema(SchemaKind::Envelope);
        let archives = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/published-schemas");
        let mut compared = 0;
        for entry in std::fs::read_dir(&archives).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let Ok(version) = name.parse::<u32>() else {
                continue;
            };
            let archived = archived_schema(version, "envelope.schema.json");
            let wire =
                &archived["$defs"]["envelope.status"]["properties"]["schema_version"]["const"];
            if *wire != serde_json::json!(crate::output::SCHEMA_VERSION) {
                continue;
            }
            let (count, refused) = refused_changes(&archived, &current);
            assert!(
                refused.is_empty(),
                "schema set {version} refuses under the same envelope wire version: {}; \
                 move output::SCHEMA_VERSION (DEC-997.1)",
                refused.join(", ")
            );
            compared += count;
        }
        assert!(
            compared > 0,
            "no archive under this wire version was compared"
        );
    }

    /// Whether every value `now` describes is accepted by `then`, for one
    /// constrained object. A property may be dropped and a property may become
    /// required; a property may not appear, stop being required, or change its
    /// own schema, and nothing else about the object may change. Comparing a
    /// property's schema by equality is conservative: deciding JSON Schema
    /// containment in general is not attempted (DECISIONS 465).
    fn still_accepted(then: &serde_json::Value, now: &serde_json::Value) -> Result<(), String> {
        let empty = serde_json::Map::new();
        let properties = |schema: &serde_json::Value| {
            schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default()
        };
        let required = |schema: &serde_json::Value| -> std::collections::BTreeSet<String> {
            schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect()
        };
        let (was, is) = (properties(then), properties(now));
        for (name, schema) in &is {
            match was.get(name) {
                None => return Err(format!("adds property `{name}`")),
                Some(old) if old != schema => return Err(format!("changes property `{name}`")),
                Some(_) => {}
            }
        }
        if let Some(name) = required(then).difference(&required(now)).next() {
            return Err(format!("no longer requires `{name}`"));
        }
        let rest = |schema: &serde_json::Value| -> serde_json::Map<String, serde_json::Value> {
            schema
                .as_object()
                .unwrap_or(&empty)
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "properties" | "required"))
                .map(|(key, v)| (key.clone(), v.clone()))
                .collect()
        };
        if rest(then) != rest(now) {
            return Err("changes a keyword other than its properties".to_owned());
        }
        Ok(())
    }

    #[test]
    fn a_constrained_object_may_drop_an_optional_property_and_nothing_looser() {
        let then = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "kind": { "const": "docker" }, "pull": { "type": "string" } },
            "required": ["kind"],
        });
        let mut dropped = then.clone();
        dropped["properties"]
            .as_object_mut()
            .unwrap()
            .remove("pull");
        assert_eq!(still_accepted(&then, &dropped), Ok(()));
        let mut tightened = then.clone();
        tightened["required"] = serde_json::json!(["kind", "pull"]);
        assert_eq!(still_accepted(&then, &tightened), Ok(()));

        let mut added = then.clone();
        added["properties"]["platform"] = serde_json::json!({ "type": "string" });
        assert!(
            still_accepted(&then, &added)
                .unwrap_err()
                .contains("adds property `platform`")
        );
        let mut loosened = then.clone();
        loosened["required"] = serde_json::json!([]);
        assert!(
            still_accepted(&then, &loosened)
                .unwrap_err()
                .contains("no longer requires `kind`")
        );
        let mut retyped = then.clone();
        retyped["properties"]["pull"] = serde_json::json!({ "type": "integer" });
        assert!(
            still_accepted(&then, &retyped)
                .unwrap_err()
                .contains("changes property `pull`")
        );
        let mut opened = then.clone();
        opened["additionalProperties"] = serde_json::json!({ "type": "string" });
        assert!(still_accepted(&then, &opened).is_err());
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
                "Nested": { "oneOf": [{ "properties": {
                    "inner": {
                        "description": "dropped before comparison",
                        "type": "object",
                        "properties": { "description": { "type": "string" } },
                        "additionalProperties": false
                    }
                } }] },
            }
        });
        let constrained = constrained_objects(&document);
        let names: Vec<&str> = constrained.keys().map(String::as_str).collect();
        assert_eq!(names, ["Map", "Nested/oneOf/0/properties/inner"]);
        assert_eq!(
            constrained["Nested/oneOf/0/properties/inner"],
            serde_json::json!({
                "type": "object",
                "properties": { "description": { "type": "string" } },
                "additionalProperties": false
            })
        );
    }

    /// Keywords whose value is an instance literal, not a schema: nothing
    /// beneath them is a keyword, so nothing there is dropped or walked.
    const LITERALS: [&str; 4] = ["const", "enum", "default", "examples"];
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
            _ if LITERALS.contains(&key) => Vec::new(),
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

    /// A schema with its `description` annotations removed: they change no
    /// validation. A property named `description` and a literal holding one
    /// are kept.
    fn without_annotations(schema: &serde_json::Value) -> serde_json::Value {
        let serde_json::Value::Object(map) = schema else {
            return schema.clone();
        };
        map.iter()
            .filter(|(key, _)| key.as_str() != "description")
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
        fn constrains(map: &serde_json::Map<String, serde_json::Value>) -> bool {
            match map.get("additionalProperties") {
                Some(serde_json::Value::Bool(false)) => true,
                Some(serde_json::Value::Object(schema)) => !schema.is_empty(),
                Some(_) | None => false,
            }
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

    /// For every object `archived` constrains, whether `current` still
    /// describes only values `archived` accepts. The current object is found
    /// by its pointer whether or not it is still constrained, so one that
    /// opened is compared too. A pointer that no longer resolves is a removed
    /// property of an enclosing open object, which #1038 covers. Returns how
    /// many objects were compared, and what each refused change was.
    fn refused_changes(
        archived: &serde_json::Value,
        current: &serde_json::Value,
    ) -> (usize, Vec<String>) {
        let mut compared = 0;
        let mut refused = Vec::new();
        for (path, then) in constrained_objects(archived) {
            let Some(now) = current["$defs"].pointer(&format!("/{path}")) else {
                continue;
            };
            compared += 1;
            if let Err(why) = still_accepted(&then, &without_annotations(now)) {
                refused.push(format!("`{path}` {why}"));
            }
        }
        (compared, refused)
    }

    #[test]
    fn an_object_that_opens_or_changes_a_literal_is_refused_and_an_unchanged_one_is_not() {
        let archived = serde_json::json!({ "$defs": { "D": { "properties": {
            "map": {
                "type": "object",
                "additionalProperties": { "$ref": "#/$defs/Observation" }
            },
            "closed": {
                "type": "object",
                "additionalProperties": false,
                "properties": { "tag": { "const": { "description": "old" } } }
            }
        } } } });
        assert_eq!(refused_changes(&archived, &archived), (2, Vec::new()));

        let mut opened = archived.clone();
        opened["$defs"]["D"]["properties"]["map"]
            .as_object_mut()
            .unwrap()
            .remove("additionalProperties");
        let (_, refused) = refused_changes(&archived, &opened);
        assert_eq!(
            refused,
            ["`D/properties/map` changes a keyword other than its properties"]
        );

        let mut relabelled = archived.clone();
        relabelled["$defs"]["D"]["properties"]["closed"]["properties"]["tag"]["const"]["description"] =
            "new".into();
        let (_, refused) = refused_changes(&archived, &relabelled);
        assert_eq!(refused, ["`D/properties/closed` changes property `tag`"]);

        let mut annotated = archived.clone();
        annotated["$defs"]["D"]["properties"]["closed"]["description"] = "reworded".into();
        assert_eq!(refused_changes(&archived, &annotated), (2, Vec::new()));
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
