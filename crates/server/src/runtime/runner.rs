use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::Value;
use wasmtime::{Engine, Store, Trap};

use super::{
    Plugin, PluginPre, WitError, WitRequest, WitResponse,
    host::{Shared, State, new_store},
    ticks,
};
use crate::{manifest::Manifest, netguard::Policy, sync::lock};

const MAX_INSTANCES_PER_EXTENSION: usize = 64;
const RECYCLE_AFTER_CALLS: u32 = 10_000;
const INIT_DEADLINE: Duration = Duration::from_secs(5);
const SETUP_DEADLINE: Duration = Duration::from_secs(30);
const JOB_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum CallError {
    Timeout,
    Busy,
    Crashed(String),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Timeout => write!(f, "timed out"),
            CallError::Busy => write!(f, "all instances busy"),
            CallError::Crashed(details) => write!(f, "{details}"),
        }
    }
}

pub struct Runner {
    pub manifest: Manifest,
    pub mount: String,
    pub source: String,
    pre: PluginPre<State>,
    engine: Engine,
    shared: Arc<Shared>,
    config_json: String,
    max_memory_bytes: usize,
    idle: Mutex<Vec<Instance>>,
    live: Arc<AtomicUsize>,
}

struct Instance {
    store: Store<State>,
    plugin: Plugin,
    calls: u32,
    _slot: Slot,
}

struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn reserve(live: &Arc<AtomicUsize>) -> Option<Slot> {
        let others = live.fetch_add(1, Ordering::SeqCst);
        let slot = Slot(live.clone());
        (others < MAX_INSTANCES_PER_EXTENSION).then_some(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Runner {
    pub(super) fn new(manifest: Manifest, pre: PluginPre<State>, engine: Engine, shared: Arc<Shared>, source: String, config: &Value, max_memory_bytes: usize) -> Runner {
        Runner {
            manifest,
            mount: shared.name.clone(),
            source,
            pre,
            engine,
            shared,
            config_json: config.to_string(),
            max_memory_bytes,
            idle: Mutex::new(Vec::new()),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.shared.policy
    }

    pub(super) async fn warm_up(&self) -> anyhow::Result<()> {
        let mut instance = self.checkout().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        if !self.manifest.setup {
            self.give_back(instance);
            return Ok(());
        }
        instance.store.set_epoch_deadline(ticks(SETUP_DEADLINE));
        let result = instance.plugin.samishisa_extension_extension().call_setup(&mut instance.store).await;
        match self.finish(instance, result) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => anyhow::bail!("setup failed: {message}"),
            Err(e) => anyhow::bail!("setup crashed: {e}"),
        }
    }

    pub async fn handle(&self, request: &WitRequest, timeout: Duration) -> Result<Result<WitResponse, WitError>, CallError> {
        let mut instance = self.checkout().await?;
        instance.store.set_epoch_deadline(ticks(timeout));
        let result = instance.plugin.samishisa_extension_extension().call_handle(&mut instance.store, request).await;
        self.finish(instance, result)
    }

    pub async fn run_job(&self, index: u32) -> Result<Result<(), String>, CallError> {
        let mut instance = self.checkout().await?;
        instance.store.set_epoch_deadline(ticks(JOB_DEADLINE));
        let result = instance.plugin.samishisa_extension_extension().call_run_job(&mut instance.store, index).await;
        self.finish(instance, result)
    }

    async fn checkout(&self) -> Result<Instance, CallError> {
        if let Some(instance) = lock(&self.idle).pop() {
            return Ok(instance);
        }
        let slot = Slot::reserve(&self.live).ok_or(CallError::Busy)?;
        let mut store = new_store(&self.engine, self.shared.clone(), INIT_DEADLINE, self.max_memory_bytes);
        let plugin = self.pre.instantiate_async(&mut store).await.map_err(|e| CallError::Crashed(format!("instantiate: {e:?}")))?;
        match plugin.samishisa_extension_extension().call_init(&mut store, &self.config_json).await {
            Ok(Ok(())) => Ok(Instance { store, plugin, calls: 0, _slot: slot }),
            Ok(Err(message)) => Err(CallError::Crashed(format!("init: {message}"))),
            Err(trap) => Err(classify(&store, trap)),
        }
    }

    fn finish<T>(&self, instance: Instance, result: wasmtime::Result<T>) -> Result<T, CallError> {
        match result {
            Ok(answer) => {
                self.give_back(instance);
                Ok(answer)
            }
            Err(trap) => Err(classify(&instance.store, trap)),
        }
    }

    fn give_back(&self, mut instance: Instance) {
        instance.calls += 1;
        if instance.calls < RECYCLE_AFTER_CALLS {
            lock(&self.idle).push(instance);
        }
    }
}

fn classify(store: &Store<State>, trap: wasmtime::Error) -> CallError {
    let memory = &store.data().memory;
    if memory.was_hit {
        return CallError::Crashed(format!("ran out of memory (the cap is {} MB; raise it with \"memoryMb\" in the config entry)", memory.max_bytes >> 20));
    }
    if trap.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
        return CallError::Timeout;
    }
    CallError::Crashed(format!("{trap:?}"))
}
