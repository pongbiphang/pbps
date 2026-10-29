//! Complete requested candidate membership plus the required property closure.
//! Catalog rows outside that closure are lookup input, not certified facts.

use super::bindings::{self, Binding};
use super::logical::{self, Catalog, Row};
use super::properties::{self, Field};
use super::{CandidateClass, CandidateSet, CaptureScope, Uncovered, profile, render};
use pbps_db::resolver::capture::ObjectIdentity;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Locator {
    pub class: &'static str,
    pub index: usize,
}

type NamedRows<'a> = BTreeMap<(Option<&'a str>, &'a str), Vec<usize>>;

// Only positions and shallow names live here. Hashes shortlist comparisons;
// complete logical identities still decide equality, including collisions.
struct Index<'a> {
    identities: BTreeMap<&'static str, BTreeMap<u64, Vec<usize>>>,
    names: BTreeMap<&'static str, NamedRows<'a>>,
    children: BTreeMap<(&'static str, &'static str), BTreeMap<u32, Vec<usize>>>,
}

impl<'a> Index<'a> {
    fn new(catalog: &'a Catalog) -> Result<Self, Uncovered> {
        let mut result = Self {
            identities: BTreeMap::new(),
            names: BTreeMap::new(),
            children: BTreeMap::new(),
        };
        for &class in properties::CLASSES {
            if class == "pg_depend" {
                continue;
            }
            for (index, row) in catalog.rows[class].iter().enumerate() {
                if class == "pg_attribute" && row.get("attisdropped") == Some(&Value::Bool(true)) {
                    continue;
                }
                let id = catalog
                    .identity(class, row)
                    .map_err(|_| Uncovered::class(class, "unreadable logical identity"))?;
                let bucket = result
                    .identities
                    .entry(class)
                    .or_default()
                    .entry(logical::identity_key(&id))
                    .or_default();
                for previous in bucket.iter() {
                    if catalog
                        .identity(class, &catalog.rows[class][*previous])
                        .is_ok_and(|previous| previous == id)
                    {
                        return Err(Uncovered::class(class, "duplicate logical identity"));
                    }
                }
                bucket.push(index);
            }
        }
        // Candidate predicates examine only these names, never routine kinds,
        // relation kinds or signatures. All overloads retain source order.
        for (class, name, namespace) in [
            ("pg_namespace", "nspname", None),
            ("pg_class", "relname", Some("relnamespace")),
            ("pg_proc", "proname", Some("pronamespace")),
            ("pg_type", "typname", Some("typnamespace")),
            ("pg_operator", "oprname", Some("oprnamespace")),
            ("pg_collation", "collname", Some("collnamespace")),
            ("pg_opclass", "opcname", Some("opcnamespace")),
            ("pg_opfamily", "opfname", Some("opfnamespace")),
            ("pg_extension", "extname", None),
        ] {
            let names = result.names.entry(class).or_default();
            for (index, row) in catalog.rows[class].iter().enumerate() {
                // Keep this fallible even if identity validation stops reading
                // one of these fields in a future catalog-class definition.
                let namespace = if let Some(field) = namespace {
                    let oid = logical::number(row, field)
                        .map_err(|_| Uncovered::class(class, "unreadable logical identity"))?;
                    let namespace = catalog
                        .row("pg_namespace", oid)
                        .map_err(|_| Uncovered::class(class, "unreadable logical identity"))?;
                    Some(
                        logical::string(namespace, "nspname")
                            .map_err(|_| Uncovered::class(class, "unreadable logical identity"))?,
                    )
                } else {
                    None
                };
                let name = logical::string(row, name)
                    .map_err(|_| Uncovered::class(class, "unreadable logical identity"))?;
                names.entry((namespace, name)).or_default().push(index);
            }
        }
        Ok(result)
    }

    fn named(&self, class: &str, namespace: Option<&str>, name: Option<&str>) -> Vec<usize> {
        let mut rows = Vec::new();
        if class == "pg_cast" {
            // Cast predicates have neither a namespace nor a name.
            if let Some(buckets) = self.identities.get(class) {
                rows.extend(buckets.values().flatten().copied());
            }
        } else if let Some(names) = self.names.get(class) {
            if let (Some(namespace), Some(name)) = (namespace, name) {
                rows.extend(
                    names
                        .get(&(Some(namespace), name))
                        .into_iter()
                        .flatten()
                        .copied(),
                );
            } else {
                for (&(ns, spelling), positions) in names {
                    if namespace.is_none_or(|wanted| ns == Some(wanted))
                        && name.is_none_or(|wanted| spelling == wanted)
                    {
                        rows.extend(positions);
                    }
                }
            }
        }
        rows.sort_unstable();
        rows
    }

    fn find(&self, catalog: &Catalog, id: &ObjectIdentity) -> Option<Locator> {
        let class = match id.class.as_str() {
            "column" => "pg_attribute",
            "pg_authid" => "pg_roles",
            class => class,
        };
        let (&class, buckets) = self.identities.get_key_value(class)?;
        buckets
            .get(&logical::identity_key(id))?
            .iter()
            .copied()
            .find_map(|index| {
                catalog
                    .identity(class, &catalog.rows[class][index])
                    .is_ok_and(|found| found == *id)
                    .then_some(Locator { class, index })
            })
    }

    fn object(&self, catalog: &Catalog, class: &str, oid: u32) -> Option<Locator> {
        let class = if class == "pg_authid" {
            "pg_roles"
        } else {
            class
        };
        let class = *properties::CLASSES
            .iter()
            .find(|&&candidate| candidate == class && candidate != "pg_depend")?;
        Some(Locator {
            class,
            index: catalog.position(class, oid).ok()?,
        })
    }

    fn address(
        &self,
        catalog: &Catalog,
        class_oid: u32,
        oid: u32,
        subobject: i32,
    ) -> Option<Locator> {
        let class_row = catalog.row("pg_class", class_oid).ok()?;
        let namespace = catalog
            .row(
                "pg_namespace",
                logical::number(class_row, "relnamespace").ok()?,
            )
            .ok()?;
        if logical::string(namespace, "nspname").ok()? != "pg_catalog" {
            return None;
        }
        let class = logical::string(class_row, "relname").ok()?;
        if subobject == 0 {
            self.object(catalog, class, oid)
        } else if class == "pg_class" {
            Some(Locator {
                class: "pg_attribute",
                index: catalog.column_position(oid, subobject).ok()?,
            })
        } else {
            None
        }
    }
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
    index: Index<'a>,
    selected: BTreeSet<Locator>,
    pending: VecDeque<ObjectIdentity>,
    result: Prepared,
}

pub(super) fn prepare(
    catalog: &Catalog,
    major: u32,
    scope: &CaptureScope,
) -> Result<Prepared, Uncovered> {
    let index = Index::new(catalog)?;
    let mut builder = Builder {
        catalog,
        major,
        index,
        selected: BTreeSet::new(),
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
    // Role attributes, memberships and role/database settings are complete
    // snapshot sets. Object ACLs and owners are not binding inputs (see
    // properties.rs); the deployer's effective privileges are the separate
    // AuthorizationCondition.
    for &class in &[
        "pg_roles",
        "pg_auth_members",
        "pg_db_role_setting",
        "pg_parameter_acl",
        "pg_database",
    ] {
        for index in 0..catalog.rows[class].len() {
            builder.add_row(Locator { class, index })?;
        }
    }
    // Both ledger recipes are qualified before baseline acceptance. Include
    // their source/type closure before either recipe may invoke rendering;
    // identity sequences and internally owned objects join the closure below.
    let mut ledger = Vec::new();
    for name in [
        pbps_db::ledger::STATE_TABLE_NAME,
        pbps_db::ledger::LOCK_TABLE_NAME,
    ] {
        ledger.extend(builder.index.named(
            "pg_class",
            Some(crate::state::LEDGER_SCHEMA),
            Some(name),
        ));
    }
    ledger.sort_unstable();
    for index in ledger {
        builder.add_row(Locator {
            class: "pg_class",
            index,
        })?;
    }
    for object in &scope.retained {
        // A dependency row is not a catalog object; it is read when its
        // subject is, below.
        if object.class == "pg_depend" {
            continue;
        }
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
            for index in builder.index.named("pg_namespace", None, Some(namespace)) {
                builder.add_row(Locator {
                    class: "pg_namespace",
                    index,
                })?;
            }
        }
        let mut members = BTreeSet::new();
        for index in builder.index.named(
            class,
            candidate.namespace.as_deref(),
            candidate.name.as_deref(),
        ) {
            let id = catalog
                .identity(class, &catalog.rows[class][index])
                .map_err(|_| Uncovered::class(class, "unreadable candidate identity"))?;
            members.insert(id.clone());
            builder.add_at(id, Locator { class, index });
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
        let locator = self
            .index
            .find(self.catalog, &id)
            .ok_or_else(|| Uncovered::object(&id, "required object is absent or unsupported"))?;
        self.add_at(id, locator);
        Ok(())
    }

    fn add_at(&mut self, id: ObjectIdentity, locator: Locator) {
        if self.selected.insert(locator) {
            self.result.members.insert(id.clone(), locator);
            self.pending.push_back(id);
        }
    }

    fn add_row(&mut self, locator: Locator) -> Result<(), Uncovered> {
        if self.selected.contains(&locator) {
            return Ok(());
        }
        let id = self
            .catalog
            .identity(
                locator.class,
                &self.catalog.rows[locator.class][locator.index],
            )
            .map_err(|_| Uncovered::class(locator.class, "unreadable prerequisite identity"))?;
        self.add_at(id, locator);
        Ok(())
    }

    fn reference(&mut self, class: &str, oid: u32) -> Result<(), Uncovered> {
        if oid == 0 {
            return Ok(());
        }
        let fail = || Uncovered::class(class, "unreadable referenced prerequisite");
        let locator = self
            .index
            .object(self.catalog, class, oid)
            .ok_or_else(fail)?;
        self.add_row(locator)
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
                    .filter(|n| *n != 0)
                    .map(|oid| match (class, name) {
                        ("pg_trigger", "tgqual") => bindings::Relation::Trigger(oid),
                        _ => bindings::Relation::Table(oid),
                    });
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
                | Field::Authorization
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
            "pg_ts_config" => &[("pg_ts_config_map", "mapcfg")],
            _ => &[],
        };
        for &(child, field) in children {
            // Build on first use: malformed membership still refuses at
            // the original child traversal, never ahead of an earlier error.
            if !self.index.children.contains_key(&(child, field)) {
                let mut parents: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
                for (index, row) in self.catalog.rows[child].iter().enumerate() {
                    let parent = logical::number(row, field)
                        .map_err(|_| Uncovered::class(child, "unreadable child membership"))?;
                    if child == "pg_attribute"
                        && row.get("attisdropped") == Some(&Value::Bool(true))
                    {
                        continue;
                    }
                    parents.entry(parent).or_default().push(index);
                }
                self.index.children.insert((child, field), parents);
            }
            // Each parent expands once. Taking its rows avoids cloning and
            // retains the snapshot's order, including system attributes.
            if let Some(rows) = self
                .index
                .children
                .get_mut(&(child, field))
                .unwrap()
                .remove(&oid)
            {
                for index in rows {
                    self.add_row(Locator {
                        class: child,
                        index,
                    })?;
                }
            }
        }
        Ok(())
    }

    /// Whether a dependency row's dependent is a TOAST relation. A table's
    /// out-of-line storage is physical, as its relfilenode is: no expression
    /// binds it, and its name carries the table's OID, so it differs between
    /// the target and scratch. It and everything under it stay out of the
    /// capture (DEC-1274.1).
    /// Whether a dependency row's dependent is authorization metadata: a
    /// default ACL depends automatically on its schema, but owners and ACLs
    /// are not binding inputs (rule v2, DEC-1274.1), so the row is not one
    /// either. Any other dependent of a class the capture cannot read still
    /// refuses.
    fn authorization(&self, row: &Row) -> Result<bool, ()> {
        let class = self
            .catalog
            .row("pg_class", logical::number(row, "classid").map_err(|_| ())?)
            .map_err(|_| ())?;
        Ok(logical::string(class, "relname").map_err(|_| ())? == "pg_default_acl")
    }

    fn toast(&self, row: &Row) -> Result<bool, ()> {
        let relation = |field| logical::number(row, field).map_err(|_| ());
        let class = self
            .catalog
            .row("pg_class", relation("classid")?)
            .map_err(|_| ())?;
        if logical::string(class, "relname").map_err(|_| ())? != "pg_class"
            || logical::signed(row, "objsubid").map_err(|_| ())? != 0
        {
            return Ok(false);
        }
        let dependent = self
            .catalog
            .row("pg_class", relation("objid")?)
            .map_err(|_| ())?;
        Ok(logical::string(dependent, "relkind").map_err(|_| ())? == "t")
    }

    fn dependencies(&mut self) -> Result<(), Uncovered> {
        for &class in &["pg_depend"] {
            for row in &self.catalog.rows[class] {
                let (class_field, object_field, sub_field) = ("classid", "objid", "objsubid");
                let fail = || Uncovered::class(class, "unreadable prerequisite dependency");
                let subject = self.index.address(
                    self.catalog,
                    logical::number(row, class_field).map_err(|_| fail())?,
                    logical::number(row, object_field).map_err(|_| fail())?,
                    logical::signed(row, sub_field).map_err(|_| fail())?,
                );
                let mut selected = subject.is_some_and(|locator| self.selected.contains(&locator));
                if class == "pg_depend"
                    && logical::string(row, "deptype")
                        .is_ok_and(|kind| matches!(kind, "e" | "i" | "a" | "P" | "S"))
                {
                    let extension = self.index.address(
                        self.catalog,
                        logical::number(row, "refclassid").map_err(|_| fail())?,
                        logical::number(row, "refobjid").map_err(|_| fail())?,
                        logical::signed(row, "refobjsubid").map_err(|_| fail())?,
                    );
                    // Extension membership and auto/internal ownership are
                    // complete child sets. An unknown required child class
                    // refuses instead of disappearing from the closure.
                    selected |= extension.is_some_and(|locator| self.selected.contains(&locator));
                }
                if !selected
                    || self.toast(row).map_err(|_| fail())?
                    || self.authorization(row).map_err(|_| fail())?
                {
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

#[cfg(test)]
mod tests;
