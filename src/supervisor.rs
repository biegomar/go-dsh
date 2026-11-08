//! Runs a batch: a fixed pool of workers pulls queued jobs until none are left.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::paths::batch_dir;
use crate::runner::{cancel_job, cancel_marker, run_job};
use crate::store::{read_job_raw, require_batch, update_batch, update_job};
use crate::util::{now_iso, sleep_ms, Result};

pub fn batch_cancel_marker(id: &str) -> PathBuf {
    batch_dir(id).join("cancel")
}

pub fn run_batch(id: &str, stagger_ms: u64) -> Result<()> {
    let batch = require_batch(id)?;
    let pid = std::process::id();
    update_batch(id, |b| {
        b.owner_pid = Some(pid);
        b.started_at = Some(now_iso());
    })?;
    for jid in &batch.jobs {
        if read_job_raw(jid)?.map(|j| j.status == "queued").unwrap_or(false) {
            update_job(jid, |j| j.owner_pid = Some(pid))?;
        }
    }

    let queue: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(batch.jobs.iter().cloned().collect()));
    let parallel = (batch.parallel.max(1) as usize).min(batch.jobs.len().max(1));
    let batch_id = id.to_string();
    let mut handles = vec![];
    for slot in 0..parallel {
        let queue = Arc::clone(&queue);
        let batch_id = batch_id.clone();
        handles.push(std::thread::spawn(move || {
            // Stagger the first wave so parallel dsh boots do not race on npx caches.
            if stagger_ms > 0 {
                sleep_ms(slot as u64 * stagger_ms);
            }
            loop {
                let next = queue.lock().unwrap().pop_front();
                let Some(jid) = next else { break };
                if batch_cancel_marker(&batch_id).exists() && !cancel_marker(&jid).exists() {
                    let _ = fs::write(cancel_marker(&jid), now_iso());
                }
                if let Err(e) = run_job(&jid) {
                    let _ = update_job(&jid, |j| {
                        j.status = "failed".into();
                        j.error = Some(e.message.clone());
                        j.ended_at = Some(now_iso());
                    });
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    update_batch(id, |b| b.ended_at = Some(now_iso()))?;
    Ok(())
}

/// Cancel every unfinished job of a batch; returns how many were cancelled.
pub fn cancel_batch(id: &str) -> Result<usize> {
    let batch = require_batch(id)?;
    fs::write(batch_cancel_marker(id), now_iso())?;
    let mut n = 0;
    for jid in &batch.jobs {
        if cancel_job(jid)? {
            n += 1;
        }
    }
    Ok(n)
}
