//! Preserve PostgreSQL's NULL/default ACL separately from explicit entries.
//! An empty explicit map grants no ordinary privileges, even to the owner.
use super::{DbError, Grant};
use std::collections::BTreeMap;

/// Schema ACL storage mode, not an inference from the number of grants.
/// Default cannot carry entries; explicit revocation can leave an empty map.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum SchemaAcl {
    Default,
    Explicit(BTreeMap<String, Vec<Grant>>),
}

impl SchemaAcl {
    /// Enumeration is for role mapping and grant lookup only. Mode-sensitive
    /// replay, projection and comparison must retain the enum (DEC-699.1).
    pub(super) fn entries(&self) -> impl Iterator<Item = (&String, &Vec<Grant>)> {
        match self {
            Self::Default => None,
            Self::Explicit(entries) => Some(entries),
        }
        .into_iter()
        .flatten()
    }

    pub(super) fn materialize(
        &mut self,
        owner: &pbps_model::RoleName,
    ) -> &mut BTreeMap<String, Vec<Grant>> {
        if matches!(self, Self::Default) {
            *self = Self::Explicit(BTreeMap::from([(
                owner.raw().to_owned(),
                ["CREATE", "USAGE"]
                    .into_iter()
                    .map(|privilege| Grant {
                        privilege: privilege.into(),
                        grantable: false,
                        grantor: owner.clone(),
                    })
                    .collect(),
            )]));
        }
        match self {
            Self::Explicit(entries) => entries,
            Self::Default => unreachable!("materialized above"),
        }
    }
}

/// Nullable scalars from the LEFT JOIN, shared with the decoder's negative
/// tests: a SQL NULL entry marker is not the real PUBLIC grantee OID zero.
#[derive(Clone, Copy)]
pub(super) struct Fields<'a> {
    pub namespace: Option<&'a str>,
    pub default_acl: Option<&'a str>,
    pub grantee_oid: Option<&'a str>,
    pub grantee: Option<&'a str>,
    pub grantor: Option<&'a str>,
    pub privilege: Option<&'a str>,
    pub grantable: Option<&'a str>,
}

pub(super) fn decode<'a>(
    rows: impl IntoIterator<Item = Result<Fields<'a>, DbError>>,
) -> Result<SchemaAcl, DbError> {
    let invalid = || {
        DbError::BadRow("a schema ACL header or grant was absent, inconsistent or malformed".into())
    };
    let mut header = None;
    let mut empty = false;
    let mut entries: BTreeMap<String, Vec<Grant>> = BTreeMap::new();
    for row in rows {
        let row = row?;
        let namespace = row
            .namespace
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|oid| *oid != 0)
            .ok_or_else(invalid)?;
        let default = match row.default_acl {
            Some("true") => true,
            Some("false") => false,
            _ => return Err(invalid()),
        };
        let current = (namespace, default);
        if header.is_some_and(|previous| previous != current) || empty {
            return Err(invalid());
        }
        header = Some(current);
        match (
            row.grantee_oid,
            row.grantee,
            row.grantor,
            row.privilege,
            row.grantable,
        ) {
            (None, None, None, None, None) if entries.is_empty() => empty = true,
            (Some(oid), Some(grantee), Some(grantor), Some(privilege), Some(grantable))
                if !default =>
            {
                let oid = oid.parse::<u32>().map_err(|_| invalid())?;
                if (oid == 0 && grantee != "PUBLIC")
                    || grantee.is_empty()
                    || grantor.is_empty()
                    || !super::SCHEMA_PRIVILEGES.contains(&privilege)
                {
                    return Err(invalid());
                }
                let grantable = match grantable {
                    "true" => true,
                    "false" => false,
                    _ => return Err(invalid()),
                };
                entries.entry(grantee.into()).or_default().push(Grant {
                    privilege: privilege.into(),
                    grantable,
                    grantor: grantor.into(),
                });
            }
            _ => return Err(invalid()),
        }
    }
    let (_, default) = header.ok_or_else(invalid)?;
    for grants in entries.values_mut() {
        grants.sort();
    }
    Ok(if default {
        SchemaAcl::Default
    } else {
        SchemaAcl::Explicit(entries)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(default_acl: &'static str) -> Fields<'static> {
        Fields {
            namespace: Some("42"),
            default_acl: Some(default_acl),
            grantee_oid: None,
            grantee: None,
            grantor: None,
            privilege: None,
            grantable: None,
        }
    }

    fn grant() -> Fields<'static> {
        Fields {
            grantee_oid: Some("0"),
            grantee: Some("PUBLIC"),
            grantor: Some("owner"),
            privilege: Some("USAGE"),
            grantable: Some("false"),
            ..header("false")
        }
    }

    fn decoded(rows: &[Fields<'_>]) -> Result<SchemaAcl, DbError> {
        decode(rows.iter().copied().map(Ok))
    }

    #[test]
    fn a_zero_entry_header_retains_default_or_explicit_empty_mode() {
        assert_eq!(decoded(&[header("true")]).unwrap(), SchemaAcl::Default);
        assert_eq!(
            decoded(&[header("false")]).unwrap(),
            SchemaAcl::Explicit(BTreeMap::new())
        );
        let public = decoded(&[grant()]).unwrap();
        // Quoted "PUBLIC" is also a legal real role name on both majors.
        // Only the zero OID is the pseudo-role; a nonzero marker is still
        // a complete catalog grant, not an absent/malformed observation.
        assert_eq!(
            decoded(&[Fields {
                grantee_oid: Some("7"),
                ..grant()
            }])
            .unwrap(),
            public
        );
        let SchemaAcl::Explicit(entries) = public else {
            panic!("PUBLIC is a real entry")
        };
        assert_eq!(
            entries["PUBLIC"],
            vec![Grant {
                privilege: "USAGE".into(),
                grantable: false,
                grantor: "owner".into()
            }]
        );
    }

    #[test]
    fn repeated_headers_preserve_sorted_grants_options_and_grantors() {
        let first = Fields {
            privilege: Some("CREATE"),
            grantable: Some("true"),
            ..grant()
        };
        let second = Fields {
            grantor: Some("another"),
            ..grant()
        };
        let expected = decoded(&[first, grant(), second]).unwrap();
        assert_eq!(expected, decoded(&[second, grant(), first]).unwrap());
        let SchemaAcl::Explicit(entries) = expected else {
            panic!("explicit")
        };
        assert_eq!(entries["PUBLIC"].len(), 3);
        assert!(entries["PUBLIC"][0].grantable);
        assert_eq!(entries["PUBLIC"][1].grantor.raw(), "another");
    }

    #[test]
    fn absent_failed_partial_and_inconsistent_acl_reads_refuse() {
        assert!(decoded(&[]).is_err());
        assert!(decode([Err(DbError::BadRow("query failed".into()))]).is_err());
        for invalid in [
            Fields {
                namespace: None,
                ..header("false")
            },
            Fields {
                namespace: Some("0"),
                ..header("false")
            },
            Fields {
                namespace: Some("invalid"),
                ..header("false")
            },
            Fields {
                default_acl: None,
                ..header("false")
            },
            header("invalid"),
            Fields {
                default_acl: Some("true"),
                ..grant()
            },
            Fields {
                grantee_oid: None,
                ..grant()
            },
            Fields {
                grantee: None,
                ..grant()
            },
            Fields {
                grantor: None,
                ..grant()
            },
            Fields {
                privilege: None,
                ..grant()
            },
            Fields {
                grantable: None,
                ..grant()
            },
            Fields {
                grantee_oid: Some("invalid"),
                ..grant()
            },
            Fields {
                grantee: Some("reader"),
                ..grant()
            },
            Fields {
                grantee: Some(""),
                ..grant()
            },
            Fields {
                grantor: Some(""),
                ..grant()
            },
            Fields {
                privilege: Some("SELECT"),
                ..grant()
            },
            Fields {
                grantable: Some("invalid"),
                ..grant()
            },
        ] {
            assert!(decoded(&[invalid]).is_err());
        }
        for rows in [
            vec![header("false"), header("false")],
            vec![header("true"), header("false")],
            vec![header("false"), grant()],
            vec![grant(), header("false")],
            vec![
                grant(),
                Fields {
                    namespace: Some("43"),
                    ..grant()
                },
            ],
            vec![
                grant(),
                Fields {
                    default_acl: Some("true"),
                    ..grant()
                },
            ],
        ] {
            assert!(decoded(&rows).is_err());
        }
    }
}
