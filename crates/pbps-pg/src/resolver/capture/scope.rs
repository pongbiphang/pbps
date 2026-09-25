//! Complete requested candidate membership plus the required property closure.
//! Catalog rows outside that closure are lookup input, not certified facts.

use super::bindings::{self, Binding};
use super::logical::{self, Catalog, Row};
use super::properties::{self, Field};
use super::{CandidateClass, CandidateSet, CaptureScope, Uncovered, profile, render};
use pbps_db::resolver::capture::ObjectIdentity;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy)]
pub(super) struct Locator {
    pub class: &'static str,
    pub index: usize,
}

// No source, private property or verifier implements Debug/Serialize here.
pub(super) struct Prepared {
    pub members: BTreeMap<ObjectIdentity, Locator>,
    pub bindings: BTreeMap<ObjectIdentity, Vec<Binding>>,
    pub candidates: BTreeMap<CandidateSet, BTreeSet<ObjectIdentity>>,
    pub limitations: BTreeSet<ObjectIdentity>,
    pub addresses: BTreeMap<ObjectIdentity, BTreeMap<String, Value>>,
    pub render: render::Selection,
}

struct Builder<'a> {
    catalog: &'a Catalog,
    major: u32,
    index: BTreeMap<ObjectIdentity, Locator>,
    pending: VecDeque<ObjectIdentity>,
    result: Prepared,
}

pub(super) fn prepare(
    catalog: &Catalog,
    major: u32,
    scope: &CaptureScope,
) -> Result<Prepared, Uncovered> {
    let mut index = BTreeMap::new();
    for &class in properties::CLASSES {
        if matches!(class, "pg_depend" | "pg_shdepend" | "pg_init_privs") {
            continue;
        }
        for (position, row) in catalog.rows[class].iter().enumerate() {
            if class == "pg_attribute" && row.get("attisdropped") == Some(&Value::Bool(true)) {
                continue;
            }
            let identity = catalog
                .identity(class, row)
                .map_err(|_| Uncovered::class(class, "unreadable logical identity"))?;
            if index
                .insert(
                    identity,
                    Locator {
                        class,
                        index: position,
                    },
                )
                .is_some()
            {
                return Err(Uncovered::class(class, "duplicate logical identity"));
            }
        }
    }
    let mut builder = Builder {
        catalog,
        major,
        index,
        pending: VecDeque::new(),
        result: Prepared {
            members: BTreeMap::new(),
            bindings: BTreeMap::new(),
            candidates: BTreeMap::new(),
            limitations: BTreeSet::new(),
            addresses: BTreeMap::new(),
            render: render::Selection::default(),
        },
    };
    // Authorization is a complete snapshot set, not a cache-derived effective
    // privilege answer. Include absence and all grant/membership properties.
    for &class in &[
        "pg_roles",
        "pg_auth_members",
        "pg_default_acl",
        "pg_db_role_setting",
        "pg_parameter_acl",
        "pg_database",
    ] {
        for row in &catalog.rows[class] {
            builder.add_row(class, row)?;
        }
    }
    // Both ledger recipes are qualified before baseline acceptance. Include
    // their source/type closure before either recipe may invoke rendering;
    // identity sequences and internally owned objects join the closure below.
    for row in &catalog.rows["pg_class"] {
        if catalog.identity("pg_class", row).is_ok_and(|id| {
            [
                pbps_db::ledger::STATE_TABLE_NAME,
                pbps_db::ledger::LOCK_TABLE_NAME,
            ]
            .iter()
            .any(|name| id.name == [crate::state::LEDGER_SCHEMA, *name])
        }) {
            builder.add_row("pg_class", row)?;
        }
    }
    for object in &scope.retained {
        builder.add(object.clone())?;
    }
    for candidate in &scope.candidates {
        let class = candidate.class.catalog();
        let global = matches!(
            candidate.class,
            CandidateClass::Cast | CandidateClass::Extension
        );
        if global && candidate.namespace.is_some()
            || candidate.class == CandidateClass::Cast && candidate.name.is_some()
        {
            return Err(Uncovered::class(class, "invalid candidate predicate"));
        }
        if let Some(namespace) = &candidate.namespace {
            // Namespace absence is recorded by the empty membership; when it
            // exists, its ownership/ACL are prerequisites too.
            for row in &catalog.rows["pg_namespace"] {
                if logical::string(row, "nspname") == Ok(namespace) {
                    builder.add_row("pg_namespace", row)?;
                }
            }
        }
        let mut members = BTreeSet::new();
        for row in &catalog.rows[class] {
            let id = catalog
                .identity(class, row)
                .map_err(|_| Uncovered::class(class, "unreadable candidate identity"))?;
            let name_index = usize::from(!global);
            if candidate
                .namespace
                .as_ref()
                .is_some_and(|ns| id.name.first() != Some(ns))
                || candidate
                    .name
                    .as_ref()
                    .is_some_and(|name| id.name.get(name_index) != Some(name))
            {
                continue;
            }
            members.insert(id.clone());
            builder.add(id)?;
        }
        builder.result.candidates.insert(candidate.clone(), members);
    }
    loop {
        while let Some(object) = builder.pending.pop_front() {
            builder.expand(&object)?;
        }
        builder.dependencies()?;
        if builder.pending.is_empty() {
            break;
        }
    }
    Ok(builder.result)
}

impl Builder<'_> {
    fn add(&mut self, id: ObjectIdentity) -> Result<(), Uncovered> {
        if id.class == "query-output" {
            return Ok(());
        }
        let locator = *self
            .index
            .get(&id)
            .ok_or_else(|| Uncovered::object(&id, "required object is absent or unsupported"))?;
        if self.result.members.insert(id.clone(), locator).is_none() {
            self.pending.push_back(id);
        }
        Ok(())
    }

    fn add_row(&mut self, class: &'static str, row: &Row) -> Result<(), Uncovered> {
        let id = self
            .catalog
            .identity(class, row)
            .map_err(|_| Uncovered::class(class, "unreadable prerequisite identity"))?;
        self.add(id)
    }

    fn reference(&mut self, class: &str, oid: u32) -> Result<(), Uncovered> {
        if oid == 0 {
            return Ok(());
        }
        self.add(
            self.catalog
                .object(class, oid)
                .map_err(|_| Uncovered::class(class, "unreadable referenced prerequisite"))?,
        )
    }

    fn expand(&mut self, id: &ObjectIdentity) -> Result<(), Uncovered> {
        let locator = self.result.members[id];
        let class = locator.class;
        let row = &self.catalog.rows[class][locator.index];
        let fail = || Uncovered::object(id, "unreadable prerequisite property");
        if class == "pg_class" && logical::string(row, "relkind") == Ok("f") {
            return Err(Uncovered::object(
                id,
                "foreign table properties are not qualified",
            ));
        }
        self.result.render.include(class, row).map_err(|_| fail())?;
        let mut observed = Vec::new();
        for &(name, kind) in properties::fields(class, self.major).map_err(|_| fail())? {
            let value = row.get(name).ok_or_else(fail)?;
            match properties::field(class, name, kind).map_err(|_| fail())? {
                Field::Reference(target) => {
                    self.reference(target, logical::number(row, name).map_err(|_| fail())?)?
                }
                Field::References(target) if !value.is_null() => {
                    for oid in logical::numbers(row, name).map_err(|_| fail())? {
                        self.reference(target, oid)?;
                    }
                }
                Field::Definition if !value.is_null() && value.as_str() != Some("<>") => {
                    let text = value.as_str().ok_or_else(fail)?;
                    profile::expression(self.catalog, text, self.major)?;
                    let relation = match class {
                        "pg_class" => Some("oid"),
                        "pg_rewrite" => Some("ev_class"),
                        "pg_attrdef" => Some("adrelid"),
                        "pg_constraint" => Some("conrelid"),
                        "pg_index" => Some("indrelid"),
                        "pg_partitioned_table" => Some("partrelid"),
                        "pg_policy" => Some("polrelid"),
                        "pg_trigger" => Some("tgrelid"),
                        _ => None,
                    }
                    .map(|field| logical::number(row, field))
                    .transpose()
                    .map_err(|_| fail())?
                    .filter(|n| *n != 0);
                    let mut extracted = bindings::extract(self.catalog, text, self.major, relation)
                        .map_err(|_| {
                            Uncovered::object(id, "creation-time binding surface is not qualified")
                        })?;
                    for binding in &mut extracted {
                        binding.path.insert(0, name.into());
                        self.add(binding.target.clone())?;
                    }
                    observed.extend(extracted);
                }
                Field::Scalar if kind == "anyarray" && !value.is_null() => {
                    if class != "pg_attribute" {
                        return Err(fail());
                    }
                    profile::datum(
                        self.catalog,
                        logical::number(row, "atttypid").map_err(|_| fail())?,
                    )?;
                }
                Field::Scalar
                | Field::References(_)
                | Field::Columns(_)
                | Field::Column(_)
                | Field::Acl
                | Field::Definition
                | Field::Physical
                | Field::Address => {}
            }
        }
        if class == "pg_proc" && row.get("prosqlbody").is_some_and(Value::is_null) {
            let language = self
                .catalog
                .object(
                    "pg_language",
                    logical::number(row, "prolang").map_err(|_| fail())?,
                )
                .map_err(|_| fail())?;
            if !matches!(language.name.first().map(String::as_str), Some("internal")) {
                // The header/defaults are observable; a string or procedural
                // body is runtime-bound. Never turn that into an empty proof.
                self.result.limitations.insert(id.clone());
            }
        }
        observed.sort_by(|a, b| (&a.path, &a.target).cmp(&(&b.path, &b.target)));
        self.result.bindings.insert(id.clone(), observed);
        if let Ok(oid) = logical::number(row, "oid") {
            self.children(class, oid)?;
        }
        Ok(())
    }

    fn children(&mut self, class: &str, oid: u32) -> Result<(), Uncovered> {
        let children: &[(&'static str, &'static str)] = match class {
            "pg_class" => &[
                ("pg_attribute", "attrelid"),
                ("pg_attrdef", "adrelid"),
                ("pg_rewrite", "ev_class"),
                ("pg_constraint", "conrelid"),
                ("pg_index", "indrelid"),
                ("pg_sequence", "seqrelid"),
                ("pg_partitioned_table", "partrelid"),
                ("pg_policy", "polrelid"),
                ("pg_trigger", "tgrelid"),
                ("pg_inherits", "inhrelid"),
                ("pg_inherits", "inhparent"),
            ],
            "pg_type" => &[
                ("pg_enum", "enumtypid"),
                ("pg_range", "rngtypid"),
                ("pg_range", "rngmultitypid"),
                ("pg_constraint", "contypid"),
            ],
            "pg_proc" => &[("pg_aggregate", "aggfnoid")],
            "pg_opfamily" => &[("pg_amop", "amopfamily"), ("pg_amproc", "amprocfamily")],
            _ => &[],
        };
        for &(child, field) in children {
            for row in &self.catalog.rows[child] {
                if logical::number(row, field)
                    .map_err(|_| Uncovered::class(child, "unreadable child membership"))?
                    == oid
                {
                    if child == "pg_attribute"
                        && row.get("attisdropped") == Some(&Value::Bool(true))
                    {
                        continue;
                    }
                    self.add_row(child, row)?;
                }
            }
        }
        Ok(())
    }

    fn dependencies(&mut self) -> Result<(), Uncovered> {
        for &class in &["pg_depend", "pg_shdepend", "pg_init_privs"] {
            for row in &self.catalog.rows[class] {
                let (class_field, object_field, sub_field) = if class == "pg_init_privs" {
                    ("classoid", "objoid", "objsubid")
                } else {
                    ("classid", "objid", "objsubid")
                };
                let fail = || Uncovered::class(class, "unreadable prerequisite dependency");
                let subject = self.catalog.address(
                    logical::number(row, class_field).map_err(|_| fail())?,
                    logical::number(row, object_field).map_err(|_| fail())?,
                    logical::signed(row, sub_field).map_err(|_| fail())?,
                );
                let mut selected = subject
                    .as_ref()
                    .is_ok_and(|id| self.result.members.contains_key(id));
                if class == "pg_depend"
                    && logical::string(row, "deptype")
                        .is_ok_and(|kind| matches!(kind, "e" | "i" | "a" | "P" | "S"))
                {
                    let extension = self.catalog.address(
                        logical::number(row, "refclassid").map_err(|_| fail())?,
                        logical::number(row, "refobjid").map_err(|_| fail())?,
                        logical::signed(row, "refobjsubid").map_err(|_| fail())?,
                    );
                    // Extension membership and auto/internal ownership are
                    // complete child sets. An unknown required child class
                    // refuses instead of disappearing from the closure.
                    selected |= extension
                        .as_ref()
                        .is_ok_and(|id| self.result.members.contains_key(id));
                }
                if !selected {
                    continue;
                }
                let (identity, properties) =
                    properties::address_row(self.catalog, class, row, self.major)
                        .map_err(|_| fail())?;
                for object in &identity.signature {
                    self.add(object.clone())?;
                }
                self.result.addresses.insert(identity, properties);
            }
        }
        Ok(())
    }
}
