use std::{sync::Arc, time::Duration};

use tokio::{
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};

use crate::{log::log_warn, runtime::Runner};

const SHORTEST_INTERVAL: Duration = Duration::from_secs(1);

pub struct Jobs(Vec<JoinHandle<()>>);

impl Jobs {
    pub fn start<'a>(runners: impl Iterator<Item = &'a Arc<Runner>>) -> Jobs {
        let mut timers = Vec::new();
        for runner in runners {
            for (index, job) in (0..u32::MAX).zip(&runner.manifest.jobs) {
                let every = Duration::from_millis(job.every_ms).max(SHORTEST_INTERVAL);
                timers.push(tokio::spawn(run_every(runner.clone(), index, every)));
            }
        }
        Jobs(timers)
    }
}

async fn run_every(runner: Arc<Runner>, index: u32, every: Duration) {
    let mut tick = tokio::time::interval_at(Instant::now() + every, every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let failure = match runner.run_job(index).await {
            Ok(Ok(())) => continue,
            Ok(Err(message)) => message,
            Err(call_error) => call_error.to_string(),
        };
        log_warn!(&runner.mount, "job {index} failed: {failure}");
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        for timer in &self.0 {
            timer.abort();
        }
    }
}
