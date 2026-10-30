//! The target-qualified fallback questions, separate from replay rows (DEC-717.1).

use super::{literal, required};
use pbps_db::{DbError, Row, transport::QueryConnection};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Presence {
    Present,
    Absent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub(super) struct Domain(BTreeMap<String, Presence>);

impl Domain {
    pub(super) fn differences(&self, got: &Self) -> Vec<String> {
        self.0
            .keys()
            .chain(got.0.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|name| match (self.0.get(name), got.0.get(name)) {
                (Some(want), Some(actual)) if want == actual => None,
                (Some(_), None) => Some(format!("schema:{name}:unreadable")),
                (None, Some(_)) => Some(format!("schema:{name}:scope")),
                _ => Some(format!("schema:{name}:presence")),
            })
            .collect()
    }

    pub(super) fn require_complete(
        &self,
        schemas: &BTreeMap<String, super::SchemaAuthorization>,
    ) -> Result<(), DbError> {
        for (name, presence) in &self.0 {
            if *presence == Presence::Present && !schemas.contains_key(name) {
                return Err(DbError::BadRow(format!(
                    "schema:{name}:unreadable: required target facts were omitted"
                )));
            }
        }
        for name in schemas.keys() {
            if self.0.get(name) != Some(&Presence::Present) {
                return Err(DbError::BadRow(format!(
                    "schema:{name}:scope: recipe facts are outside the captured present inventory"
                )));
            }
        }
        Ok(())
    }

    pub(super) fn extend(&mut self, extra: Self) -> Result<(), DbError> {
        if extra
            .0
            .iter()
            .any(|(name, presence)| self.0.get(name).is_some_and(|old| old != presence))
        {
            return Err(DbError::Refused(
                "the target schema presence changed during staged authorization capture".into(),
            ));
        }
        self.0.extend(extra.0);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn present(names: impl IntoIterator<Item = &'static str>) -> Self {
        Self(
            names
                .into_iter()
                .map(|name| (name.to_owned(), Presence::Present))
                .collect(),
        )
    }
}

pub(super) struct Read {
    pub(super) domain: Domain,
    /// Logical key -> catalog spelling and owner. Only present schemas replay.
    pub(super) present: BTreeMap<String, (String, String)>,
}

struct Answer {
    ordinal: String,
    name: Option<String>,
    owner: Option<String>,
}

impl Answer {
    fn from_row(row: &Row) -> Result<Self, DbError> {
        Ok(Self {
            ordinal: required(row, "ordinal", "a schema observation ordinal")?,
            name: row.try_get::<&str>("name")?.map(str::to_owned),
            owner: row.try_get::<&str>("owner")?.map(str::to_owned),
        })
    }
}

/// Every requested/fallback name gets one left-join answer, including absence.
/// SQL Server resolves spelling and equality under its own database collation.
pub(super) async fn read(
    conn: &mut impl QueryConnection,
    requested: &[String],
    configured: &str,
    effective: Option<&str>,
    frozen: Option<&Domain>,
) -> Result<Read, DbError> {
    let mut inputs: BTreeMap<String, bool> = requested
        .iter()
        .cloned()
        .map(|name| (name, frozen.is_none()))
        .collect();
    if let Some(domain) = frozen {
        // A wrong recipe or scratch default cannot remove a target question.
        for name in domain.0.keys() {
            inputs.entry(name.clone()).or_insert(false);
        }
    } else {
        inputs.insert("dbo".to_owned(), true);
        inputs.entry(configured.to_owned()).or_insert(false);
        if let Some(name) = effective {
            inputs.insert(name.to_owned(), true);
        }
    }
    let inputs: Vec<_> = inputs.into_iter().collect();
    if inputs.is_empty() || inputs.iter().any(|(name, _)| name.is_empty()) {
        return Err(DbError::BadRow(
            "the schema question domain is empty or malformed".into(),
        ));
    }
    let values = inputs
        .iter()
        .enumerate()
        .map(|(index, (name, _))| format!("(N'{index}', {})", literal(name)))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = conn
        .query(&format!(
            "SELECT q.ordinal AS ordinal, s.name AS name, USER_NAME(s.principal_id) AS owner \
         FROM (VALUES {values}) AS q(ordinal, requested) \
         LEFT JOIN sys.schemas s ON s.name = q.requested;"
        ))
        .await?;
    let answers = rows
        .iter()
        .map(Answer::from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let observed = decode(&inputs, answers, frozen)?;
    // SCHEMA_NAME() returning NULL is a known absence only after the catalog
    // independently confirms it; it is not a generic unreadable default.
    if frozen.is_none()
        && effective.is_none()
        && observed.domain.0.get(configured) != Some(&Presence::Absent)
    {
        return Err(DbError::BadRow("the effective default schema is unreadable without a confirmed absent configured default".into()));
    }
    Ok(observed)
}

fn decode(
    inputs: &[(String, bool)],
    answers: Vec<Answer>,
    frozen: Option<&Domain>,
) -> Result<Read, DbError> {
    let bad =
        || DbError::BadRow("the schema observations are incomplete, ambiguous or malformed".into());
    if answers.len() != inputs.len() {
        return Err(bad());
    }
    let mut observed = Read {
        domain: Domain::default(),
        present: BTreeMap::new(),
    };
    let mut seen = BTreeSet::new();
    for answer in answers {
        let index = answer.ordinal.parse::<usize>().map_err(|_| bad())?;
        if index >= inputs.len() || index.to_string() != answer.ordinal || !seen.insert(index) {
            return Err(bad());
        }
        let (asked, required) = &inputs[index];
        let (key, presence) = match (answer.name, answer.owner) {
            (Some(name), Some(owner)) if !name.is_empty() && !owner.is_empty() => {
                let key = if frozen.is_some_and(|d| d.0.contains_key(asked)) {
                    asked.clone()
                } else {
                    name.clone()
                };
                if let Some(previous) = observed
                    .present
                    .insert(key.clone(), (name.clone(), owner.clone()))
                    && previous != (name, owner)
                {
                    return Err(bad());
                }
                (key, Presence::Present)
            }
            (None, None) if !required => (asked.clone(), Presence::Absent),
            (None, None) => {
                return Err(DbError::BadRow(format!(
                    "the in-scope schema {asked} does not exist"
                )));
            }
            _ => return Err(bad()),
        };
        if let Some(previous) = observed.domain.0.insert(key, presence)
            && previous != presence
        {
            return Err(bad());
        }
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(index: &str, name: Option<&str>, owner: Option<&str>) -> Answer {
        Answer {
            ordinal: index.into(),
            name: name.map(str::to_owned),
            owner: owner.map(str::to_owned),
        }
    }

    #[test]
    fn absence_and_a_missing_observation_are_different() {
        let inputs = [("missing".into(), false)];
        let got = decode(&inputs, vec![answer("0", None, None)], None).unwrap();
        assert_eq!(got.domain.0["missing"], Presence::Absent);
        assert!(got.present.is_empty());
        assert!(decode(&inputs, vec![], None).is_err());
        assert!(decode(&inputs, vec![answer("0", Some("missing"), None)], None).is_err());
        assert!(
            decode(
                &[("missing".into(), true)],
                vec![answer("0", None, None)],
                None
            )
            .is_err()
        );
    }

    #[test]
    fn every_question_requires_one_complete_canonical_ordinal() {
        let inputs = [("a".into(), false), ("b".into(), false)];
        for ordinals in [["0", "0"], ["0", "2"], ["00", "1"], ["-1", "1"]] {
            assert!(
                decode(
                    &inputs,
                    ordinals
                        .into_iter()
                        .map(|i| answer(i, None, None))
                        .collect(),
                    None
                )
                .is_err()
            );
        }
        assert!(
            decode(
                &inputs,
                vec![answer("1", None, None), answer("0", None, None)],
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn only_the_engine_folds_schema_names_and_frozen_absence_remains_a_question() {
        let inputs = [("APP".into(), true), ("app".into(), true)];
        let got = decode(
            &inputs,
            vec![
                answer("0", Some("app"), Some("dbo")),
                answer("1", Some("app"), Some("dbo")),
            ],
            None,
        )
        .unwrap();
        assert_eq!(got.domain.0.len(), 1);
        assert_eq!(got.present.len(), 1);
        let want = Domain([("MiSsInG".into(), Presence::Absent)].into());
        let got = decode(
            &[("MiSsInG".into(), false)],
            vec![answer("0", Some("missing"), Some("dbo"))],
            Some(&want),
        )
        .unwrap();
        assert_eq!(want.differences(&got.domain), ["schema:MiSsInG:presence"]);
        assert!(
            want.differences(&Domain::default())
                .contains(&"schema:MiSsInG:unreadable".to_owned())
        );
    }
    #[test]
    fn staged_extension_preserves_old_questions_and_rejects_changed_presence() {
        let mut domain = Domain([("missing".into(), Presence::Absent)].into());
        let before = domain.clone();
        assert!(domain.extend(Domain::present(["missing"])).is_err());
        assert_eq!(
            domain, before,
            "refusal does not partially replace the witness"
        );
        domain.extend(Domain::present(["new_schema"])).unwrap();
        assert_eq!(domain.0["missing"], Presence::Absent);
        assert_eq!(domain.0["new_schema"], Presence::Present);
    }
    #[test]
    fn only_present_questions_require_recipe_facts_and_no_extra_fact_can_replace_absence() {
        let mut facts = BTreeMap::new();
        assert!(Domain::present(["app"]).require_complete(&facts).is_err());
        let absent = Domain([("missing".into(), Presence::Absent)].into());
        absent.require_complete(&facts).unwrap();
        facts.insert(
            "missing".to_owned(),
            super::super::SchemaAuthorization {
                owner: "dbo".into(),
                permissions: BTreeSet::new(),
                grants: vec![],
            },
        );
        assert!(absent.require_complete(&facts).is_err());
        Domain::present(["missing"])
            .require_complete(&facts)
            .unwrap();
    }
}
