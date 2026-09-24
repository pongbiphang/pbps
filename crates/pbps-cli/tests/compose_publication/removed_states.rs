//! The superseded same-checkout protocol's states cannot be resumed (#748).
//!
//! ADR-0017 removed source placement, index installation, HEAD repair and
//! per-path undo. The receipt's phase type has no variant for any of them, so
//! no publisher can write one; these cases prove that a receipt claiming one
//! anyway (an old binary, a hand edit) is refused and preserved, never
//! reinterpreted as the nearest modern phase. They are synthetic records
//! derived from an actual delivered receipt, not interruption coverage.

use super::*;

#[test]
fn receipts_in_removed_or_unknown_states_refuse_without_mutation() {
    for (label, edit) in [
        ("placing", ("phase", "placing")),
        ("composed", ("phase", "composed")),
        ("index-installed", ("phase", "index_installed")),
        ("rolling-back", ("phase", "rolling_back")),
        ("head-repair", ("phase", "restoring_head")),
        ("version", ("version", "2")),
    ] {
        let f = Fixture::new(&format!("removed-{label}"));
        let (_store, preview, candidate) = f.ready();
        let delivered = f.publisher().confirm(&candidate);
        assert_eq!(delivered.status, Status::Delivered, "{delivered:?}");
        let id = &preview.operation_id;
        let path = f.record(id);
        let mut receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match edit {
            ("version", value) => receipt["version"] = value.parse::<u32>().unwrap().into(),
            (_, phase) => receipt["state"]["phase"] = phase.into(),
        }
        let bytes = serde_json::to_vec(&receipt).unwrap();
        fs::write(&path, &bytes).unwrap();
        let before = f.repo.preserved();
        let output_ref = format!("refs/heads/pbps-compose/{id}");
        let (local, remote) = (
            git(&f.repo.root, &["rev-parse", &output_ref]),
            f.remote_ref(&output_ref),
        );
        let mut publisher = f.publisher();
        for outcome in [
            publisher.recover(id),
            publisher.retry(id),
            publisher.republish(id, &generation(&delivered)),
            publisher.cleanup(id),
        ] {
            assert_eq!(
                outcome.problem,
                Some(Problem::ReceiptUnavailable),
                "{label}: {outcome:?}"
            );
            assert_eq!(outcome.details, None, "{label}: nothing is inferred");
        }
        let listed = publisher.list().unwrap_err().to_string();
        assert!(
            listed.contains(&path.display().to_string()),
            "{label}: discovery names the receipt: {listed}"
        );
        assert!(publisher.forget(id).is_err(), "{label}: forget");
        // Positive control is the delivered receipt above; here nothing moved.
        assert_eq!(fs::read(&path).unwrap(), bytes, "{label}");
        assert_eq!(git(&f.repo.root, &["rev-parse", &output_ref]), local);
        assert_eq!(f.remote_ref(&output_ref), remote);
        assert_eq!(f.repo.preserved(), before, "{label}");
    }
}

#[test]
fn a_receipt_from_a_replaced_source_checkout_is_named() {
    // Same source path, different inode: not another worktree, and not this
    // one either. Discovery refuses and names the receipt (#810).
    let f = Fixture::new("removed-replaced-source");
    let (_store, preview, candidate) = f.ready();
    assert_eq!(f.publisher().confirm(&candidate).status, Status::Delivered);
    let path = f.record(&preview.operation_id);
    let mut receipt: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let inode = &mut receipt["description"]["repository"]["source_inode"];
    *inode = (inode.as_u64().unwrap() + 1).into();
    let bytes = serde_json::to_vec(&receipt).unwrap();
    fs::write(&path, &bytes).unwrap();
    let listed = f.publisher().list().unwrap_err().to_string();
    assert!(listed.contains("source identity changed"), "{listed}");
    assert!(listed.contains(&path.display().to_string()), "{listed}");
    assert_eq!(fs::read(&path).unwrap(), bytes);
}
