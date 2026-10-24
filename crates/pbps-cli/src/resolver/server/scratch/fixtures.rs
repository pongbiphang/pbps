//! Fixtures the resolver's live tests share: a target to seed, a declared
//! pair whose overload and default the engine binds, and a project with an
//! environment key.
//!
//! The target is created by hand. In particular, `unmanaged_ix` exists in the
//! catalog but is absent from the declaration inventory: its automatic
//! dependency on `t.c` must never grant pbps ownership. The desired overload
//! arrives only in the declared schema, so the engine decides which existing
//! expressions would bind differently after a fresh creation.

use super::super::BindingRequest;
use pbps_db::fingerprint::FingerprintKey;
use pbps_model::resolver::Surface;
use pbps_model::{
    CheckConstraint, Column, Hints, IdsFile, Index, IndexColumn, IndexKey, Module, ModuleKind,
    Schema, Table,
};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

pub(crate) const SCHEMA: &str = "pbps_evidence1274";

pub(crate) const TARGET_SETUP: &[&str] = &[
    "CREATE SCHEMA pbps_evidence1274",
    "CREATE FUNCTION pbps_evidence1274.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    "CREATE TABLE pbps_evidence1274.t (c numeric DEFAULT pbps_evidence1274.f(1), CONSTRAINT ck CHECK (c > pbps_evidence1274.f(1)))",
    "CREATE INDEX ix ON pbps_evidence1274.t (c) WHERE c > pbps_evidence1274.f(1)",
    "CREATE INDEX unmanaged_ix ON pbps_evidence1274.t (c)",
    "CREATE VIEW pbps_evidence1274.v AS SELECT pbps_evidence1274.f(1) AS x",
    "CREATE VIEW pbps_evidence1274.control AS SELECT pbps_evidence1274.f(1::numeric) AS x",
];

fn module(schema: &mut Schema, id: &str, kind: ModuleKind, definition: &str) {
    schema.modules.insert(
        id.parse().unwrap(),
        Module {
            kind,
            description: None,
            definition: definition.into(),
        },
    );
}

/// The model owns exactly the named default, check and predicate. The
/// unmanaged index is a read prerequisite, not part of this declaration.
pub(crate) fn pair_with_cross_kind_surfaces() -> (Schema, Schema) {
    let mut base = Schema::default();
    module(
        &mut base,
        "pbps_evidence1274.f(numeric)",
        ModuleKind::Function,
        "(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    );
    module(
        &mut base,
        "pbps_evidence1274.v",
        ModuleKind::View,
        "SELECT f(1) AS x",
    );
    module(
        &mut base,
        "pbps_evidence1274.control",
        ModuleKind::View,
        "SELECT f(1::numeric) AS x",
    );
    let mut table = Table::default();
    let mut column = Column::new("numeric".parse().unwrap());
    column.value_source = pbps_model::ValueSource::Default("f(1)".into());
    table.columns.insert("c".into(), column);
    table.checks.insert(
        "ck".into(),
        CheckConstraint {
            expression: "c > f(1)".into(),
        },
    );
    table.indexes.insert(
        "ix".into(),
        Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("c".into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: Some("c > f(1)".into()),
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    base.tables
        .insert("pbps_evidence1274.t".parse().unwrap(), table);
    let mut desired = base.clone();
    module(
        &mut desired,
        "pbps_evidence1274.f(integer)",
        ModuleKind::Function,
        "(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    );
    (base, desired)
}

/// [`pair_with_cross_kind_surfaces`], and the plan also creates a view, a
/// function and a column default that each carry an `xml` fragment, which
/// `xmloption = document` refuses to read and `content` reads (#1997,
/// DEC-1634.1). The reconstruction compiles the first two on scratch; all
/// three are the declarations a deployment under the dialect's pins accepts.
pub(crate) fn pair_with_xml_fragments() -> (Schema, Schema) {
    let (base, mut desired) = pair_with_cross_kind_surfaces();
    module(
        &mut desired,
        "pbps_evidence1274.xv",
        ModuleKind::View,
        "SELECT '<a/><b/>'::xml AS x",
    );
    module(
        &mut desired,
        "pbps_evidence1274.xf()",
        ModuleKind::Function,
        "() RETURNS xml LANGUAGE sql IMMUTABLE RETURN '<a/><b/>'::xml",
    );
    let table = desired
        .tables
        .get_mut(&"pbps_evidence1274.t".parse().unwrap())
        .expect("the pair declares t");
    let mut column = Column::new("xml".parse().unwrap());
    column.value_source = pbps_model::ValueSource::Default("'<a/><b/>'::xml".into());
    table.columns.insert("x".into(), column);
    (base, desired)
}

pub(crate) const ENVIRONMENT: &str = "fixture";

pub(crate) struct ProjectKey {
    root: PathBuf,
    pub(crate) project: pbps_config::Project,
}

impl ProjectKey {
    pub(crate) fn new(with_key: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pbps-1274-evidence-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root).unwrap();
        if with_key {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            // A key file others can read is refused on Unix.
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(root.join("key")).unwrap();
            writeln!(file, "{}", FingerprintKey::generate()).unwrap();
        }
        let config = format!(
            "dialect: postgres\nenvironments:\n  {ENVIRONMENT}:\n    url_env: PBPS_1274_UNUSED\n{}",
            if with_key {
                "    fingerprint_key_file: key\n"
            } else {
                ""
            }
        );
        let path = root.join("pbps.yml");
        std::fs::write(&path, &config).unwrap();
        let parsed = pbps_config::Config::parse(&config, &path).unwrap();
        let project = pbps_config::Project {
            root: root.clone(),
            config: parsed,
        };
        Self { root, project }
    }
}

impl Drop for ProjectKey {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

pub(crate) fn ids(schema: &Schema, previous: &IdsFile) -> IdsFile {
    pbps_diff::resolve(
        schema,
        previous,
        &[],
        &pbps_diff::Context {
            operator: "1274-test".into(),
            today: "2026-09-29".into(),
        },
    )
    .unwrap()
    .ids
}

pub(crate) struct Inputs {
    dependencies: pbps_model::dependency::Dependencies,
    base: Schema,
    desired: Schema,
    base_ids: IdsFile,
    desired_ids: IdsFile,
    bootstrap: pbps_diff::order::DiffPlan,
    pub(crate) hints: Hints,
}

impl Inputs {
    pub(crate) fn overload() -> Self {
        Self::from_pair(pair_with_cross_kind_surfaces())
    }

    pub(crate) fn from_pair((base, desired): (Schema, Schema)) -> Self {
        let base_ids = ids(&base, &IdsFile::default());
        let desired_ids = ids(&desired, &base_ids);
        let empty = Schema::default();
        let dialect = pbps_pg::Postgres::with_write_path_extras(Vec::new());
        let bootstrap = pbps_diff::diff(
            pbps_diff::Side {
                schema: &empty,
                ids: &IdsFile::default(),
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &desired_ids,
            },
            &dialect,
            &Hints::default(),
        )
        .unwrap();
        Self {
            dependencies: Default::default(),
            base,
            desired,
            base_ids,
            desired_ids,
            bootstrap,
            hints: Hints::default(),
        }
    }

    pub(crate) fn binding(&self) -> BindingRequest<'_> {
        BindingRequest {
            dependencies: &self.dependencies,
            bootstrap: &self.bootstrap,
            desired: &self.desired,
            base: &self.base,
        }
    }

    pub(crate) fn base(&self) -> pbps_diff::Side<'_> {
        pbps_diff::Side {
            schema: &self.base,
            ids: &self.base_ids,
        }
    }

    pub(crate) fn desired(&self) -> pbps_diff::Side<'_> {
        pbps_diff::Side {
            schema: &self.desired,
            ids: &self.desired_ids,
        }
    }
}

pub(crate) fn view<'a>(
    evidence: &'a pbps_model::resolver::ResolverEvidence,
    name: &str,
) -> &'a pbps_model::resolver::SurfaceResolution {
    let wanted = Surface::Module(format!("{}.{}", SCHEMA, name).parse().unwrap());
    evidence
        .surfaces()
        .iter()
        .find(|surface| surface.surface == wanted)
        .expect("the declared view has a connected binding surface")
}
