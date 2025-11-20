use dashmap::DashMap;
use once_cell::sync::Lazy;
use tokio::runtime::{Handle, Runtime};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::{sleep, Duration};


// ------------------- Global singleton -------------------

// 1. Define a static OnceCell
pub static GLOBAL_DISPATCHER: Lazy<KeyedDispatcher> = Lazy::new(|| KeyedDispatcher::new()); 

// ----------------------------------------------------

// Define the job type; a job may wrap a oneshot for returning results to the caller
type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Clone)]
pub struct KeyedDispatcher {
    // Use DashMap to manage senders for each keyed actor
    workers: Arc<DashMap<String, mpsc::Sender<Job>>>,
}

impl KeyedDispatcher {
    fn new() -> Self {
        Self {
            workers: Arc::new(DashMap::new()),
        }
    }

    // Submit a job for the given key
    pub async fn submit(&self, key: &str, job: Job) {
        // Try existing worker sender
        if let Some(sender) = self.workers.get(key) {
            // If exists, send directly
            if let Err(e) = sender.send(job).await {
                // If sending fails, the actor likely stopped; rebuild if needed
                eprintln!("Failed to send job to existing worker for key '{}': {}. The worker might have shut down.", key, e);
            }
            return;
        }

        // If missing, create a new worker (actor) using entry API for atomicity
        let key_owned = key.to_string();
        self.workers
            .entry(key_owned.clone())
            .or_insert_with(|| {
                let (tx, mut rx) = mpsc::channel::<Job>(32); // buffer size 32

                // Spawn worker task
                tokio::spawn(async move {
                    // println!("[Worker for key '{}'] Started.", key_owned);
                    // Receive and process jobs
                    while let Some(job) = rx.recv().await {
                        // All jobs for this key run serially
                        job();
                    }
                    // println!("[Worker for key '{}'] Shutting down.", key_owned);
                });

                tx
            })
            .send(job)
            .await
            .expect("Newly created worker channel should be open");
    }

    pub fn submit_blocking(&self, key: impl Into<String> + Send + 'static, job: Job) {
        let key = key.into(); // take ownership
        
        // Handle Tokio runtime context
        match Handle::try_current() {
            Ok(handle) => {
                // Use existing runtime
                handle.spawn(async move {
                    Self::execute_job(key, job).await;
                });
            }
            Err(_) => {
                // Build a dedicated runtime
                std::thread::spawn(move || {
                    let rt = Runtime::new().expect("Failed to create runtime");
                    rt.block_on(async {
                        Self::execute_job(key, job).await;
                    });
                });
            }
        }
    }

    // Extract execution flow for reuse
    async fn execute_job(key: String, job: Job) {
        GLOBAL_DISPATCHER.submit(&key, Box::new(move || {
            job(); // only once
        })).await;
    }
}

// Example serial async task
async fn process_data(key: &str, task_id: i32) {
    println!(
        "==> [Key: {}] Task {} starting processing...",
        key, task_id
    );
    // Simulate work
    sleep(Duration::from_millis(500)).await;
    println!(
        "<== [Key: {}] Task {} finished.",
        key, task_id
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn test_keyed_dispatcher() {
        let dispatcher = KeyedDispatcher::new();

        // Submit tasks for key "A" and "B"; each key runs serially, keys run in parallel
        let mut tasks = vec![];

        // Submit key "A" tasks
        for i in 1..=3 {
            let dispatcher_clone = dispatcher.clone();
            tasks.push(tokio::spawn(async move {
                let job = Box::new(move || {
                    tokio::spawn(process_data("A", i));
                });
                dispatcher_clone.submit("A", job).await;
            }));
        }

        // Submit key "B" tasks
        for i in 1..=3 {
            let dispatcher_clone = dispatcher.clone();
            tasks.push(tokio::spawn(async move {
                let job = Box::new(move || {
                    tokio::spawn(process_data("B", i));
                });
                dispatcher_clone.submit("B", job).await;
            }));
        }

        // Wait for all submitters
        for task in tasks {
            task.await.unwrap();
        }
        
        // Allow workers to finish
        sleep(Duration::from_secs(3)).await;
    }
}
