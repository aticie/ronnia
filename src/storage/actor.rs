//! The single SQLite writer: a bounded mailbox served by one dedicated thread,
//! plus the revision watch that announces channel-list changes.
use anyhow::{Context, Result, anyhow, ensure};
use rusqlite::{Connection, Transaction};
use std::{path::Path, thread};
use tokio::sync::{mpsc, oneshot, watch};

use super::schema;

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
    channel_revision: watch::Sender<u64>,
}

fn serve(path: &Path, mut receiver: mpsc::Receiver<Job>, ready: oneshot::Sender<Result<()>>) {
    match schema::connect(path) {
        Ok(mut connection) => {
            if ready.send(Ok(())).is_err() {
                return;
            }
            while let Some(job) = receiver.blocking_recv() {
                job(&mut connection);
            }
        }
        Err(error) => {
            let _ = ready.send(Err(error));
        }
    }
}

impl Store {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        ensure!(
            !path.as_ref().as_os_str().is_empty()
                && !path
                    .as_ref()
                    .as_os_str()
                    .to_string_lossy()
                    .trim()
                    .is_empty(),
            "database path must not be empty"
        );
        let path = path.as_ref().to_path_buf();
        let (sender, receiver) = mpsc::channel::<Job>(128);
        let (ready, ready_rx) = oneshot::channel();
        thread::Builder::new()
            .name("ronnia-sqlite".into())
            .spawn(move || serve(&path, receiver, ready))
            .context("starting SQLite actor")?;
        ready_rx
            .await
            .context("SQLite actor stopped during startup")??;
        let (channel_revision, _) = watch::channel(0);
        Ok(Self {
            sender,
            channel_revision,
        })
    }
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job = Box::new(move |c: &mut Connection| {
            let _ = tx.send(f(c));
        });
        self.sender
            .send(job)
            .await
            .map_err(|_| anyhow!("SQLite actor is unavailable"))?;
        rx.await.context("SQLite actor stopped before replying")?
    }
    pub(super) async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
    {
        self.call(move |c| {
            let tx = c.transaction()?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })
        .await
    }
    pub(super) async fn write_notifying<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
    {
        let value = self.write(f).await?;
        self.notify_channels_changed();
        Ok(value)
    }
    pub fn channel_changes(&self) -> watch::Receiver<u64> {
        self.channel_revision.subscribe()
    }
    pub fn notify_channels_changed(&self) {
        self.channel_revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}
