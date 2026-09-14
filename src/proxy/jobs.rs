//! The router's short memory of which rung submitted which video job.
//!
//! The marketplace owns a video job, and the router relays its poll rather
//! than tracking it -- a restart between submit and poll loses nothing. But a
//! job can fail long after the router handed it over: a seller takes it, the
//! confirmation window closes, and two minutes into the render the poll
//! reads `failed` with `provider_error`. That failure is the rung's, and the
//! only thing that can make the caller's resubmission land somewhere else is
//! the router remembering whose job it was and parking that rung when the
//! poll says so. So this is not a job table; it is the one fact per recent
//! job that turns a relayed failure into a cooldown, kept for as long as a
//! render plausibly takes and capped so a flood of submissions cannot grow it.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Who submitted a job: the rung, by provider and model, and the ladder it
/// sat on, for the log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOwner {
    /// The ladder the submission walked.
    pub ladder: String,
    /// The provider the job went to.
    pub provider: String,
    /// The model the job named.
    pub model: String,
}

/// Recent video submissions, oldest first, by job id.
#[derive(Debug)]
pub struct RecentJobs {
    owners: HashMap<String, (JobOwner, Instant)>,
    order: VecDeque<String>,
    ttl: Duration,
    max_entries: usize,
}

impl RecentJobs {
    /// A store that forgets a job after `ttl` and never holds more than
    /// `max_entries`.
    #[must_use]
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            owners: HashMap::new(),
            order: VecDeque::new(),
            ttl,
            max_entries: max_entries.max(1),
        }
    }

    /// Records who submitted a job.
    pub fn insert(&mut self, id: &str, owner: JobOwner) {
        self.evict();
        if self.owners.insert(id.to_string(), (owner, Instant::now())).is_none() {
            self.order.push_back(id.to_string());
        }
        while self.order.len() > self.max_entries {
            if let Some(oldest) = self.order.pop_front() {
                self.owners.remove(&oldest);
            }
        }
    }

    /// Who submitted a job, if it is recent enough to still be remembered.
    #[must_use]
    pub fn owner(&self, id: &str) -> Option<&JobOwner> {
        self.owners
            .get(id)
            .filter(|(_, at)| at.elapsed() < self.ttl)
            .map(|(owner, _)| owner)
    }

    /// Forgets a job, once its fate is known.
    pub fn forget(&mut self, id: &str) {
        if self.owners.remove(id).is_some() {
            self.order.retain(|entry| entry != id);
        }
    }

    /// How many jobs are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.owners.len()
    }

    /// Whether nothing is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    fn evict(&mut self) {
        let ttl = self.ttl;
        self.owners.retain(|_, (_, at)| at.elapsed() < ttl);
        let owners = &self.owners;
        self.order.retain(|id| owners.contains_key(id));
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn owner(model: &str) -> JobOwner {
        JobOwner {
            ladder: "video".to_string(),
            provider: "surplus".to_string(),
            model: model.to_string(),
        }
    }

    #[test]
    fn remembers_who_submitted_a_job_and_forgets_it_on_request() {
        let mut jobs = RecentJobs::new(Duration::from_secs(60), 10);
        jobs.insert("a", owner("seedance"));
        assert_eq!(jobs.owner("a"), Some(&owner("seedance")));
        assert_eq!(jobs.owner("b"), None);
        jobs.forget("a");
        assert!(jobs.is_empty());
    }

    #[test]
    fn drops_the_oldest_past_the_cap_and_everything_past_the_ttl() {
        let mut jobs = RecentJobs::new(Duration::from_secs(60), 2);
        jobs.insert("a", owner("one"));
        jobs.insert("b", owner("two"));
        jobs.insert("c", owner("three"));
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs.owner("a"), None);
        assert_eq!(jobs.owner("c"), Some(&owner("three")));

        let mut stale = RecentJobs::new(Duration::ZERO, 2);
        stale.insert("a", owner("one"));
        assert_eq!(stale.owner("a"), None);
        stale.insert("b", owner("two"));
        assert_eq!(stale.len(), 1);
    }
}
