use crate::feedback::{Block, Branch};
use crate::guest::Crash;
use chrono::prelude::*;
use chrono::DateTime;
use circular_queue::CircularQueue;
use core::c::{translate, Script};
use core::prog::Prog;
use core::target::Target;
use executor::Reason;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::fs::write;
use tokio::sync::Mutex;

pub struct TestCaseRecord {
    normal: Mutex<CircularQueue<ExecutedCase>>,
    failed: Mutex<CircularQueue<FailedCase>>,
    crash: Mutex<CircularQueue<CrashedCase>>,

    target: Arc<Target>,
    id_n: Mutex<usize>,
    work_dir: String,

    normal_num: Mutex<usize>,
    failed_num: Mutex<usize>,
    crashed_num: Mutex<usize>,
}

#[derive(Serialize, Clone)]
struct TestCase {
    id: usize,
    title: String,
    test_time: DateTime<Local>,
}

#[derive(Serialize, Clone)]
struct ExecutedCase {
    meta: TestCase,
    /// execute test program
    p: String,
    /// number of blocks per call
    block_num: Vec<usize>,
    /// number of branchs per call
    branch_num: Vec<usize>,
    /// new branch of last call
    new_branch: usize,
    /// new block of last call
    new_block: usize,
}

#[derive(Serialize, Clone)]
struct FailedCase {
    meta: TestCase,
    p: String,
    reason: String,
}

#[derive(Serialize, Clone)]
struct CrashedCase {
    meta: TestCase,
    p: String,
    repo: bool,
    crash: Crash,
}

impl TestCaseRecord {
    pub fn new(t: Arc<Target>, work_dir: String) -> Self {
        Self {
            normal: Mutex::new(CircularQueue::with_capacity(1024 * 64)),
            failed: Mutex::new(CircularQueue::with_capacity(1024 * 64)),
            crash: Mutex::new(CircularQueue::with_capacity(1024)),
            target: t,

            id_n: Mutex::new(0),
            work_dir,
            normal_num: Mutex::new(0),
            failed_num: Mutex::new(0),
            crashed_num: Mutex::new(0),
        }
    }

    pub async fn insert_executed(
        &self,
        p: &Prog,
        blocks: &[Vec<Block>],
        branches: &[Vec<Branch>],
        new_block: &HashSet<Block>,
        new_branch: &HashSet<Branch>,
    ) {
        let block_num = blocks.iter().map(|blocks| blocks.len()).collect();
        let branch_num = branches.iter().map(|branches| branches.len()).collect();
        let stmts = translate(&p, &self.target);
        let title = self.title_of(&p, &stmts);

        let case = ExecutedCase {
            meta: TestCase {
                id: self.next_id().await,
                title,
                test_time: Local::now(),
            },
            p: stmts.to_string(),
            block_num,
            branch_num,
            new_branch: new_branch.len(),
            new_block: new_block.len(),
        };
        {
            let mut execs = self.normal.lock().await;
            execs.push(case);
        }
        {
            let mut exec_n = self.normal_num.lock().await;
            *exec_n += 1;
        }
    }

    pub async fn insert_crash(&self, p: Prog, crash: Crash, repo: bool) {
        let stmts = translate(&p, &self.target);
        let case = CrashedCase {
            meta: TestCase {
                id: self.next_id().await,
                title: self.title_of(&p, &stmts),
                test_time: Local::now(),
            },
            p: stmts.to_string(),
            crash,
            repo,
        };

        self.persist_crash_case(&case).await;

        {
            let mut crashes = self.crash.lock().await;
            crashes.push(case);
        }
        {
            let mut crashed_num = self.crashed_num.lock().await;
            *crashed_num += 1;
        }
    }

    pub async fn insert_failed(&self, p: Prog, reason: Reason) {
        let stmts = translate(&p, &self.target);
        let case = FailedCase {
            meta: TestCase {
                id: self.next_id().await,
                title: self.title_of(&p, &stmts),
                test_time: Local::now(),
            },
            p: stmts.to_string(),
            reason: reason.to_string(),
        };
        {
            let mut failed_cases = self.failed.lock().await;
            failed_cases.push(case);
        }
        {
            let mut failed_num = self.failed_num.lock().await;
            *failed_num += 1;
        }
    }

    pub async fn psersist(&self) {
        tokio::join!(self.persist_normal_case(), self.persist_failed_case());
    }

    pub async fn len(&self) -> (usize, usize, usize) {
        tokio::join!(
            async {
                let normal_num = self.normal_num.lock().await;
                *normal_num
            },
            async {
                let failed_num = self.failed_num.lock().await;
                *failed_num
            },
            async {
                let crashed_num = self.crashed_num.lock().await;
                *crashed_num
            }
        )
    }

    async fn persist_normal_case(&self) {
        let cases = self.normal.lock().await;
        if cases.is_empty() {
            return;
        }
        let cases = cases.asc_iter().cloned().collect::<Vec<_>>();

        let path = format!("{}/normal_case.json", self.work_dir);
        let report = serde_json::to_string(&cases).unwrap();
        write(&path, report).await.unwrap_or_else(|e| {
            exits!(
                exitcode::IOERR,
                "Fail to persist normal test case to {} : {}",
                path,
                e
            )
        })
    }

    async fn persist_failed_case(&self) {
        let cases = self.failed.lock().await;
        if cases.is_empty() {
            return;
        }
        let cases = cases.asc_iter().cloned().collect::<Vec<_>>();
        let path = format!("{}/failed_case.json", self.work_dir);
        let report = serde_json::to_string(&cases).unwrap();
        write(&path, report).await.unwrap_or_else(|e| {
            exits!(
                exitcode::IOERR,
                "Fail to persist failed test case to {} : {}",
                path,
                e
            )
        })
    }

    async fn persist_crash_case(&self, case: &CrashedCase) {
        let path = format!("{}/crashes/{}", self.work_dir, &case.meta.title);
        let crash = serde_json::to_string_pretty(case).unwrap();
        write(&path, crash).await.unwrap_or_else(|e| {
            exits!(
                exitcode::IOERR,
                "Fail to persist failed test case to {} : {}",
                path,
                e
            )
        })
    }

    fn title_of(&self, p: &Prog, stmts: &Script) -> String {
        let group = String::from(self.target.group_name_of(p.gid));
        let target_call = stmts.0.last().unwrap().to_string();
        format!("{}__{}", group, target_call)
    }

    async fn next_id(&self) -> usize {
        let mut id = self.id_n.lock().await;
        let next = *id;
        *id += 1;
        next
    }
}
