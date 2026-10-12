//! End-to-end: a sync journaled in the database, and recovery from a run
//! that "crashed" between moving the old destination aside and putting the
//! new file in place.

use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use pcompare_core::folder::FolderCompareOptions;
use pcompare_index::{Index, JobStatus, NewJob, OpState};
use pcompare_sync::exec::{self, ExecOptions, Recovery};
use pcompare_sync::fsops::{self, PermanentDeleter};
use pcompare_sync::journal::Journal;
use pcompare_sync::sync::{plan_sync, SyncMode};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn journaled_run_and_crash_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let (left, right) = (dir.path().join("L"), dir.path().join("R"));
    write(&left, "a.txt", "new a");
    write(&left, "sub/b.txt", "b");
    write(&right, "a.txt", "old a, longer");
    write(&right, "stale.txt", "x");
    let db = dir.path().join("index.db");
    let ix = Index::open(&db).unwrap();
    let (l, r) = (left.to_string_lossy().into_owned(), right.to_string_lossy().into_owned());
    let opts = ExecOptions::default();

    // 1. A normal run: every op ends up journaled as done.
    let plan = plan_sync(&left, &right, SyncMode::MirrorLeftToRight, &FolderCompareOptions::default()).unwrap();
    let job = NewJob { left: &l, right: &r, mode: plan.mode, options: &opts, ops: &plan.ops };
    let id = ix.create_job(&job).unwrap();
    let report = {
        let mut journal = ix.journal(&id);
        exec::execute(&left, &right, &plan.ops, &opts, &PermanentDeleter, &mut journal, &AtomicBool::new(false), &mut |_| {})
    };
    assert_eq!(report.failed, 0, "{:?}", report.results);
    ix.finish_job(&id, &report, JobStatus::Completed).unwrap();
    let detail = ix.job_detail(&id).unwrap();
    assert!(detail.ops.iter().all(|o| o.state == OpState::Done && o.result.as_ref().is_some_and(|r| r.ok)));
    assert_eq!(detail.ops.iter().filter(|o| o.staged.is_some()).count(), 1, "the overwrite of a.txt");

    // 2. A run that dies mid-swap: the old a.txt is staged, the new one never lands.
    write(&left, "a.txt", "newest a");
    let plan = plan_sync(&left, &right, SyncMode::MirrorLeftToRight, &FolderCompareOptions::default()).unwrap();
    assert_eq!(plan.ops.len(), 1);
    let job = NewJob { left: &l, right: &r, mode: plan.mode, options: &opts, ops: &plan.ops };
    let id = ix.create_job(&job).unwrap();
    {
        let mut journal = ix.journal(&id);
        journal.op_started(plan.ops[0].id, 1);
        let staged = fsops::stage_aside(&right.join("a.txt")).unwrap();
        journal.backup_staged(plan.ops[0].id, &staged);
        // ...and the process is killed here: no op_finished, no finish_job.
    }
    drop(ix);

    // 3. Next launch: the job shows up as interrupted with the staged path.
    let ix = Index::open(&db).unwrap();
    assert_eq!(ix.mark_interrupted().unwrap(), 1);
    let job = ix.interrupted_job(&l, &r).unwrap().expect("interrupted job");
    assert_eq!(job.id, id);
    let detail = ix.job_detail(&id).unwrap();
    let op = &detail.ops[0];
    assert_eq!(op.state, OpState::Running);
    assert!(!right.join("a.txt").exists(), "the crash left a.txt missing");

    let rec = exec::recover_op(&left, &right, &op.op, op.staged.as_deref().map(Path::new), &PermanentDeleter);
    assert_eq!(rec, Recovery::Restored);
    assert_eq!(fs::read_to_string(right.join("a.txt")).unwrap(), "new a", "previous version back in place");
}
