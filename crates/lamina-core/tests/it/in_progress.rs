//! Integration tests for `Repo::in_progress_details` and
//! `Session::request_in_progress_details` against real git repositories.

use std::fs;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use lamina_core::Oid;
use lamina_core::repo::InProgressDetails;
use lamina_core::session::{Session, SessionEvent};
use lamina_core::test_support::TestRepo;

const TIMEOUT: Duration = Duration::from_secs(30);

fn short(id: Oid) -> String {
    id.to_hex().to_string()[..7].to_owned()
}

fn wait_for_details(
    rx: &Receiver<SessionEvent>,
) -> (lamina_core::session::QueryTicket, Option<InProgressDetails>) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for InProgressDetailsReady");
        let event = rx.recv_timeout(remaining).expect("receive session event");
        if let SessionEvent::InProgressDetailsReady { ticket, details } = event {
            return (ticket, details);
        }
    }
}

#[test]
fn normal_repo_has_no_in_progress_details() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "hello\n");
    fixture.commit("initial");

    let repo = fixture.open();
    assert_eq!(repo.in_progress_details().expect("details"), None);
}

#[test]
fn stopped_rebase_merge_backend_at_step_2_of_3() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "base\n");
    fixture.commit("base");

    fixture.git(&["checkout", "-q", "-b", "feature-x"]);
    fixture.write("b.txt", "step 1 ok\n");
    fixture.commit("Add feature file");
    fixture.write("a.txt", "feature change\n");
    let step2_id = fixture.commit("Fix parser");
    fixture.write("c.txt", "step 3 pending\n");
    fixture.commit("Add tests");

    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("a.txt", "main change\n");
    let main_id = fixture.commit("Update main");

    fixture.git(&["checkout", "-q", "feature-x"]);
    let out = fixture.git_output(&["rebase", "main"]);
    assert!(
        !out.status.success(),
        "rebase must stop on conflict at step 2"
    );

    let repo = fixture.open();
    let details = repo
        .in_progress_details()
        .expect("read details")
        .expect("rebase details");

    let InProgressDetails::Rebase {
        branch,
        onto,
        step,
        total_steps,
        current_commit,
    } = details
    else {
        panic!("expected Rebase details, got {details:?}");
    };

    assert_eq!(branch.as_deref(), Some("feature-x"));
    let onto = onto.expect("onto");
    assert_eq!(onto.id, Some(main_id));
    assert_eq!(onto.short_id, short(main_id));
    assert_eq!(onto.ref_name.as_deref(), Some("main"));
    assert_eq!(step, Some(2));
    assert_eq!(total_steps, Some(3));

    let current = current_commit.expect("current commit");
    assert_eq!(current.id, Some(step2_id));
    assert_eq!(current.short_id, short(step2_id));
    assert_eq!(current.summary.as_deref(), Some("Fix parser"));
}

#[test]
fn stopped_rebase_apply_backend_reads_details() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "base\n");
    fixture.commit("base");

    fixture.git(&["checkout", "-q", "-b", "topic"]);
    fixture.write("a.txt", "topic\n");
    let topic_id = fixture.commit("Topic patch");

    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("a.txt", "main\n");
    let main_id = fixture.commit("Main commit");

    fixture.git(&["checkout", "-q", "topic"]);
    let out = fixture.git_output(&["rebase", "--apply", "main"]);
    assert!(
        !out.status.success(),
        "apply-backend rebase must stop on conflict"
    );

    let repo = fixture.open();
    let details = repo
        .in_progress_details()
        .expect("read details")
        .expect("rebase details");

    let InProgressDetails::Rebase {
        branch,
        onto,
        step,
        total_steps,
        current_commit,
    } = details
    else {
        panic!("expected Rebase details, got {details:?}");
    };

    assert_eq!(branch.as_deref(), Some("topic"));
    let onto = onto.expect("onto");
    assert_eq!(onto.id, Some(main_id));
    assert_eq!(onto.ref_name.as_deref(), Some("main"));
    assert_eq!(step, Some(1));
    assert_eq!(total_steps, Some(1));

    let current = current_commit.expect("current commit");
    assert_eq!(current.id, Some(topic_id));
    assert_eq!(current.summary.as_deref(), Some("Topic patch"));
}

#[test]
fn stopped_merge_reads_head_and_source_branch() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "base\n");
    fixture.commit("base");

    fixture.git(&["checkout", "-q", "-b", "topic"]);
    fixture.write("a.txt", "topic\n");
    let topic_id = fixture.commit("Topic work");

    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("a.txt", "main\n");
    fixture.commit("Main work");

    let out = fixture.git_output(&["merge", "topic"]);
    assert!(!out.status.success(), "merge must stop on conflict");

    let repo = fixture.open();
    let details = repo
        .in_progress_details()
        .expect("read details")
        .expect("merge details");

    let InProgressDetails::Merge { heads, source } = details else {
        panic!("expected Merge details, got {details:?}");
    };
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].id, Some(topic_id));
    assert_eq!(heads[0].short_id, short(topic_id));
    assert_eq!(heads[0].summary.as_deref(), Some("Topic work"));
    assert_eq!(source.as_deref(), Some("topic"));
}

#[test]
fn stopped_cherry_pick_in_three_commit_sequence() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "base\n");
    fixture.commit("base");

    fixture.git(&["checkout", "-q", "-b", "topic"]);
    fixture.write("b.txt", "first\n");
    let c1 = fixture.commit("First pick");
    fixture.write("a.txt", "second conflict\n");
    let c2 = fixture.commit("Second conflicting pick");
    fixture.write("c.txt", "third\n");
    let c3 = fixture.commit("Third pick");

    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("a.txt", "main line\n");
    fixture.commit("Main change");

    let out = fixture.git_output(&[
        "cherry-pick",
        &c1.to_string(),
        &c2.to_string(),
        &c3.to_string(),
    ]);
    assert!(
        !out.status.success(),
        "cherry-pick sequence must stop on c2"
    );

    let repo = fixture.open();
    let details = repo
        .in_progress_details()
        .expect("read details")
        .expect("cherry-pick details");

    let InProgressDetails::CherryPick { commit, remaining } = details else {
        panic!("expected CherryPick details, got {details:?}");
    };
    let commit = commit.expect("CHERRY_PICK_HEAD commit");
    assert_eq!(commit.id, Some(c2));
    assert_eq!(commit.short_id, short(c2));
    assert_eq!(commit.summary.as_deref(), Some("Second conflicting pick"));
    assert_eq!(remaining, Some(2));
}

#[test]
fn stopped_revert_reads_commit_and_summary() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "base\n");
    fixture.commit("base");
    fixture.write("a.txt", "changed\n");
    let target = fixture.commit("Change a.txt");
    fixture.write("a.txt", "latest\n");
    fixture.commit("Overwrite a.txt");

    let out = fixture.git_output(&["revert", &target.to_string()]);
    assert!(!out.status.success(), "revert must stop on conflict");

    let repo = fixture.open();
    let details = repo
        .in_progress_details()
        .expect("read details")
        .expect("revert details");

    let InProgressDetails::Revert { commit, remaining } = details else {
        panic!("expected Revert details, got {details:?}");
    };
    let commit = commit.expect("REVERT_HEAD commit");
    assert_eq!(commit.id, Some(target));
    assert_eq!(commit.short_id, short(target));
    assert_eq!(commit.summary.as_deref(), Some("Change a.txt"));
    assert_eq!(remaining, None);
}

#[test]
fn stopped_am_reads_step_total_and_subject() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "base\n");
    fixture.commit("base");

    fixture.git(&["checkout", "-q", "-b", "patches"]);
    fixture.write("b.txt", "patch 1\n");
    fixture.commit("Add patch 1");
    fixture.write("a.txt", "patch 2 conflict\n");
    fixture.commit("Patch 2 conflicts");

    let mbox_dir = fixture.temp_path("patches");
    fs::create_dir_all(&mbox_dir).expect("mbox dir");
    let mbox_str = mbox_dir.to_string_lossy().into_owned();
    fixture.git(&["format-patch", "-q", "-o", &mbox_str, "main"]);

    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("a.txt", "main diverges\n");
    fixture.commit("Main diverges");

    let mut patch_files: Vec<String> = fs::read_dir(&mbox_dir)
        .expect("read mbox dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .path()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    patch_files.sort();
    assert_eq!(patch_files.len(), 2);

    let mut args = vec!["am"];
    let patch_refs: Vec<&str> = patch_files.iter().map(String::as_str).collect();
    args.extend(patch_refs);
    let out = fixture.git_output(&args);
    assert!(!out.status.success(), "git am must stop on second patch");

    let repo = fixture.open();
    let details = repo
        .in_progress_details()
        .expect("read details")
        .expect("am details");

    let InProgressDetails::ApplyingPatches {
        step,
        total_steps,
        subject,
    } = details
    else {
        panic!("expected ApplyingPatches details, got {details:?}");
    };
    assert_eq!(step, Some(2));
    assert_eq!(total_steps, Some(2));
    assert_eq!(subject.as_deref(), Some("Patch 2 conflicts"));
}

#[test]
fn bisect_in_progress_reads_counts_and_current_commit() {
    let fixture = TestRepo::new();
    let mut commits = Vec::new();
    for index in 0..6 {
        fixture.write("a.txt", &format!("v{index}\n"));
        commits.push(fixture.commit(&format!("Commit {index}")));
    }

    let bad = commits[5].to_string();
    let good = commits[0].to_string();
    fixture.git(&["bisect", "start", &bad, &good]);
    fixture.git(&["bisect", "good"]);

    let repo = fixture.open();
    let expected_rev: Oid = fs::read_to_string(repo.git_dir().join("BISECT_EXPECTED_REV"))
        .expect("BISECT_EXPECTED_REV")
        .trim()
        .parse()
        .expect("valid oid");

    let details = repo
        .in_progress_details()
        .expect("read details")
        .expect("bisect details");

    let InProgressDetails::Bisect {
        good_count,
        bad_count,
        current_commit,
    } = details
    else {
        panic!("expected Bisect details, got {details:?}");
    };
    assert_eq!(good_count, Some(2));
    assert_eq!(bad_count, Some(1));
    let current = current_commit.expect("current commit");
    assert_eq!(current.id, Some(expected_rev));
    assert_eq!(current.short_id, short(expected_rev));
    assert!(current.summary.is_some());
}

#[test]
fn missing_and_partial_state_files_are_tolerated() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "base\n");
    fixture.commit("base");

    fixture.git(&["checkout", "-q", "-b", "feature-x"]);
    fixture.write("a.txt", "feat\n");
    let feat_id = fixture.commit("Feat commit");
    fixture.write("b.txt", "feat 2\n");
    let feat2_id = fixture.commit("Feat commit 2");

    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("a.txt", "main\n");
    fixture.commit("main");

    fixture.git(&["checkout", "-q", "feature-x"]);
    let out = fixture.git_output(&["rebase", "main"]);
    assert!(!out.status.success());

    let repo = fixture.open();
    let rm = repo.git_dir().join("rebase-merge");

    // Deleting `stopped-sha` falls back to `git-rebase-todo`.
    fs::remove_file(rm.join("stopped-sha")).expect("remove stopped-sha");
    let todo_details = repo
        .in_progress_details()
        .expect("details without stopped-sha")
        .expect("rebase details");
    let InProgressDetails::Rebase { current_commit, .. } = todo_details else {
        panic!("expected Rebase details");
    };
    let todo_commit = current_commit.expect("fallback todo commit");
    assert_eq!(todo_commit.id, Some(feat2_id));
    assert_eq!(todo_commit.summary.as_deref(), Some("Feat commit 2"));

    // Deleting all optional metadata files inside `rebase-merge/` still returns
    // `Some(InProgressDetails::Rebase { .. })` with `None` fields.
    for file in [
        "head-name",
        "onto",
        "msgnum",
        "end",
        "git-rebase-todo",
        "done",
    ] {
        let path = rm.join(file);
        if path.exists() {
            fs::remove_file(path).expect("remove rebase-merge file");
        }
    }
    let partial = repo
        .in_progress_details()
        .expect("details with missing files")
        .expect("rebase details");
    assert_eq!(
        partial,
        InProgressDetails::Rebase {
            branch: None,
            onto: None,
            step: None,
            total_steps: None,
            current_commit: None,
        }
    );

    // Removing `rebase-merge` leaves a clean state for synthetic missing-file checks.
    fs::remove_dir_all(&rm).expect("remove rebase-merge");
    let _ = feat_id;

    // Corrupting `MERGE_HEAD` + missing `MERGE_MSG` does not fail.
    fs::write(repo.git_dir().join("MERGE_HEAD"), "not-an-oid\n").expect("write bad MERGE_HEAD");
    assert_eq!(
        repo.in_progress_details()
            .expect("merge details with bad MERGE_HEAD"),
        Some(InProgressDetails::Merge {
            heads: Vec::new(),
            source: None,
        })
    );
    fs::remove_file(repo.git_dir().join("MERGE_HEAD")).expect("remove MERGE_HEAD");

    // `CHERRY_PICK_HEAD` with invalid hex + missing `sequencer/todo` does not fail.
    fs::write(repo.git_dir().join("CHERRY_PICK_HEAD"), "bad\n")
        .expect("write bad CHERRY_PICK_HEAD");
    assert_eq!(
        repo.in_progress_details()
            .expect("cherry-pick details with bad CHERRY_PICK_HEAD"),
        Some(InProgressDetails::CherryPick {
            commit: None,
            remaining: None,
        })
    );
    fs::remove_file(repo.git_dir().join("CHERRY_PICK_HEAD")).expect("remove CHERRY_PICK_HEAD");

    // Plain `rebase-apply` directory without `next`/`last`/`onto` reports `Rebase`.
    let apply_dir = repo.git_dir().join("rebase-apply");
    fs::create_dir_all(&apply_dir).expect("create empty rebase-apply");
    assert_eq!(
        repo.in_progress_details()
            .expect("apply-rebase details with missing files"),
        Some(InProgressDetails::Rebase {
            branch: None,
            onto: None,
            step: None,
            total_steps: None,
            current_commit: None,
        })
    );
    // Adding `applying` switches it to `ApplyingPatches` (`git am`) while still tolerating missing files.
    fs::write(apply_dir.join("applying"), "").expect("write applying marker");
    assert_eq!(
        repo.in_progress_details()
            .expect("am details with missing files"),
        Some(InProgressDetails::ApplyingPatches {
            step: None,
            total_steps: None,
            subject: None,
        })
    );
    fs::remove_dir_all(&apply_dir).expect("remove rebase-apply");

    // Empty `BISECT_LOG` without `BISECT_EXPECTED_REV` or `refs/bisect/*` still
    // returns `Bisect` with `good_count: None`, `bad_count: None`.
    fs::write(repo.git_dir().join("BISECT_LOG"), "").expect("write empty BISECT_LOG");
    let bisect_partial = repo
        .in_progress_details()
        .expect("bisect details with empty BISECT_LOG")
        .expect("bisect details");
    assert!(matches!(
        bisect_partial,
        InProgressDetails::Bisect {
            good_count: None,
            bad_count: None,
            ..
        }
    ));
}

#[test]
fn session_requests_in_progress_details_as_a_query() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "base\n");
    fixture.commit("base");

    fixture.git(&["checkout", "-q", "-b", "topic"]);
    fixture.write("a.txt", "topic\n");
    fixture.commit("Topic work");

    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("a.txt", "main\n");
    fixture.commit("Main work");

    let out = fixture.git_output(&["merge", "topic"]);
    assert!(!out.status.success());

    let (tx, rx) = mpsc::channel();
    let session = Session::open(fixture.path(), move |event| {
        let _ = tx.send(event);
    })
    .expect("open session");

    let ticket = session.request_in_progress_details();
    let (got_ticket, details) = wait_for_details(&rx);
    assert_eq!(got_ticket, ticket);
    assert!(matches!(
        details,
        Some(InProgressDetails::Merge {
            source: Some(ref source),
            ..
        }) if source == "topic"
    ));
}
