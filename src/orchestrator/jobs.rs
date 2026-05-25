use crate::orchestrator::interface::OrchestratorProvider;
use crate::orchestrator::progress::{ProgressReporter, ProgressSnapshot, ProgressStore};
use crate::orchestrator::shared::preview;
use crate::types::DelegateToOrchestratorArgs;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

// Owns queued/running orchestrator work and routes it to one worker per task
// slug. Jobs with the same slug stay ordered in one conversation; different
// slugs can run concurrently in independent orchestrator sessions.
#[derive(Debug)]
pub(crate) struct OrchestratorJob {
    pub(crate) id: String,
    pub(crate) slug: String,
    pub(crate) args: DelegateToOrchestratorArgs,
}

#[derive(Debug)]
pub(crate) enum OrchestratorJobEvent {
    Completed {
        job_id: String,
        slug: String,
        result: Result<String, String>,
    },
}

pub(crate) struct OrchestratorJobManager {
    job_tx: mpsc::UnboundedSender<OrchestratorJob>,
    event_rx: mpsc::UnboundedReceiver<OrchestratorJobEvent>,
    progress: Arc<ProgressStore>,
}

impl OrchestratorJobManager {
    pub(crate) fn spawn(provider: OrchestratorProvider) -> Self {
        let (job_tx, job_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let progress = Arc::new(ProgressStore::new());
        tokio::spawn(dispatch_orchestrator_jobs(
            provider,
            job_rx,
            event_tx,
            Arc::clone(&progress),
        ));
        eprintln!("orchestrator job manager started");
        Self {
            job_tx,
            event_rx,
            progress,
        }
    }

    pub(crate) fn enqueue(&self, job: OrchestratorJob) -> Result<(), String> {
        eprintln!(
            "orchestrator job enqueue job={} slug={} urgency={} intent_preview={}",
            job.id,
            job.slug,
            job.args.urgency,
            preview(&job.args.user_intent)
        );
        self.progress.register_job(&job.slug, "orchestrator");
        self.progress
            .push_progress(&job.slug, &format!("Queued background job {}.", job.id));
        self.job_tx
            .send(job)
            .map_err(|_| "orchestrator job queue closed".to_string())
    }

    pub(crate) fn get_progress(
        &self,
        slug: &str,
        window_size: Option<usize>,
    ) -> Option<ProgressSnapshot> {
        self.progress.get_update(slug, window_size)
    }

    pub(crate) async fn next_event(&mut self) -> Option<OrchestratorJobEvent> {
        self.event_rx.recv().await
    }
}

async fn dispatch_orchestrator_jobs(
    provider: OrchestratorProvider,
    mut job_rx: mpsc::UnboundedReceiver<OrchestratorJob>,
    event_tx: mpsc::UnboundedSender<OrchestratorJobEvent>,
    progress: Arc<ProgressStore>,
) {
    let mut workers = HashMap::<String, mpsc::UnboundedSender<OrchestratorJob>>::new();
    while let Some(job) = job_rx.recv().await {
        let slug = job.slug.clone();
        let worker_tx = workers
            .entry(slug.clone())
            .or_insert_with(|| {
                let (worker_tx, worker_rx) = mpsc::unbounded_channel();
                tokio::spawn(run_orchestrator_jobs(
                    provider.clone(),
                    slug.clone(),
                    worker_rx,
                    event_tx.clone(),
                    Arc::clone(&progress),
                ));
                worker_tx
            })
            .clone();
        if let Err(err) = worker_tx.send(job) {
            let job = err.0;
            workers.remove(&slug);
            let (worker_tx, worker_rx) = mpsc::unbounded_channel();
            tokio::spawn(run_orchestrator_jobs(
                provider.clone(),
                slug.clone(),
                worker_rx,
                event_tx.clone(),
                Arc::clone(&progress),
            ));
            worker_tx
                .send(job)
                .map_err(|err| err.0)
                .unwrap_or_else(|job| {
                    let _ = event_tx.send(OrchestratorJobEvent::Completed {
                        job_id: job.id,
                        slug: slug.clone(),
                        result: Err("orchestrator slug worker closed".to_string()),
                    });
                });
            let _ = workers.insert(slug, worker_tx);
        }
    }
    eprintln!("orchestrator job manager stopped");
}

async fn run_orchestrator_jobs(
    provider: OrchestratorProvider,
    slug: String,
    mut job_rx: mpsc::UnboundedReceiver<OrchestratorJob>,
    event_tx: mpsc::UnboundedSender<OrchestratorJobEvent>,
    progress: Arc<ProgressStore>,
) {
    let mut session = match provider.open_session(&slug).await {
        Ok(session) => session,
        Err(err) => {
            while let Some(job) = job_rx.recv().await {
                eprintln!(
                    "orchestrator job failed to open session job={} slug={} error={}",
                    job.id, slug, err
                );
                let _ = event_tx.send(OrchestratorJobEvent::Completed {
                    job_id: job.id,
                    slug: slug.clone(),
                    result: Err(err.clone()),
                });
                progress.set_failed(&slug, &err);
            }
            return;
        }
    };

    while let Some(job) = job_rx.recv().await {
        progress.set_running(&slug);
        progress.push_progress(&slug, &format!("Started job {}.", job.id));
        eprintln!(
            "orchestrator job start job={} slug={} conversation={} urgency={} intent_preview={}",
            job.id,
            slug,
            session.conversation_id(),
            job.args.urgency,
            preview(&job.args.user_intent)
        );
        let message = job.args.to_agent_message();
        let reporter = ProgressReporter::new(Arc::clone(&progress), slug.clone());
        eprintln!(
            "orchestrator job send job={} slug={} conversation={} message_bytes={}",
            job.id,
            slug,
            session.conversation_id(),
            message.len()
        );
        let result = session
            .send_message_until_done_for_job(&job.id, &message, Some(reporter))
            .await;
        match result {
            Ok(response) => {
                progress.set_completed(&slug, &response.reply);
                eprintln!(
                    "orchestrator job completed job={} slug={} conversation={} reply_bytes={} suspended={} reply_preview={}",
                    job.id,
                    slug,
                    session.conversation_id(),
                    response.reply.len(),
                    response.suspended,
                    preview(&response.reply)
                );
                let _ = event_tx.send(OrchestratorJobEvent::Completed {
                    job_id: job.id,
                    slug: slug.clone(),
                    result: Ok(response.reply),
                });
            }
            Err(err) => {
                progress.set_failed(&slug, &err);
                eprintln!(
                    "orchestrator job failed job={} slug={} conversation={} error={}",
                    job.id,
                    slug,
                    session.conversation_id(),
                    err
                );
                let _ = event_tx.send(OrchestratorJobEvent::Completed {
                    job_id: job.id,
                    slug: slug.clone(),
                    result: Err(err),
                });
            }
        }
    }
    eprintln!("orchestrator worker stopped slug={}", slug);
}
