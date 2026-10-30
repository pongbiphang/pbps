//! Domain creation facets use the standing-domain delta, not a second SQL recipe.
use pbps_model::object_ref::{ObjectRef, Root, Selector};
use pbps_model::{Change, DomainHalf, ManagedType, operation::PartKey};
use std::collections::BTreeSet;

pub(crate) fn parts(
    change: &Change,
    functions: &[pbps_model::ModuleId],
    demanded: &BTreeSet<ObjectRef>,
) -> Option<Vec<Change>> {
    let Change::CreateType {
        uid,
        name,
        definition,
    } = change
    else {
        return None;
    };
    let ManagedType::Domain {
        not_null,
        default,
        checks,
        ..
    } = definition.as_ref()
    else {
        return None;
    };
    let selected = |part| {
        demanded.contains(&ObjectRef {
            root: Root::Type(name.clone()),
            part,
        })
    };
    let selected_facet = default.is_some() && selected(Selector::Default)
        || *not_null && selected(Selector::NotNull)
        || checks
            .keys()
            .any(|name| selected(Selector::Part(PartKey::Check(name.clone()))));
    if !selected_facet {
        return None;
    }
    // Once a creation is selected, its prefix must also be legal before the
    // planned routines. Use the same engine scan as the table recipe; leave
    // every other unselected expression in its original CREATE.
    let calls = |text: &str| functions.iter().any(|id| super::may_call(text, id.name()));
    let take_default = default
        .as_deref()
        .is_some_and(|text| selected(Selector::Default) || calls(text));
    let take_null = *not_null && selected(Selector::NotNull);
    let take_checks: Vec<_> = checks
        .iter()
        .filter(|(name, text)| {
            selected(Selector::Part(PartKey::Check((*name).clone()))) || calls(text)
        })
        .map(|(name, _)| name.clone())
        .collect();
    let mut now = definition.as_ref().clone();
    if let ManagedType::Domain {
        not_null,
        default,
        checks,
        ..
    } = &mut now
    {
        if take_default {
            *default = None;
        }
        if take_null {
            *not_null = false;
        }
        for name in &take_checks {
            checks.remove(name);
        }
    }
    let mut out = vec![Change::CreateType {
        uid: uid.clone(),
        name: name.clone(),
        definition: Box::new(now.clone()),
    }];
    let install = |now: &mut ManagedType, out: &mut Vec<Change>, next: ManagedType, half| {
        out.push(Change::AlterDomain {
            uid: uid.clone(),
            name: name.clone(),
            from: Box::new(now.clone()),
            to: Box::new(next.clone()),
            half,
        });
        *now = next;
    };
    if take_default {
        let mut next = now.clone();
        if let ManagedType::Domain { default: value, .. } = &mut next {
            *value = default.clone();
        }
        install(&mut now, &mut out, next, DomainHalf::Loosen);
    }
    for check in take_checks {
        let mut next = now.clone();
        if let ManagedType::Domain { checks: values, .. } = &mut next {
            values.insert(check.clone(), checks[&check].clone());
        }
        install(&mut now, &mut out, next, DomainHalf::Tighten);
    }
    if take_null {
        let mut next = now.clone();
        if let ManagedType::Domain { not_null, .. } = &mut next {
            *not_null = true;
        }
        install(&mut now, &mut out, next, DomainHalf::Tighten);
    }
    Some(out)
}
