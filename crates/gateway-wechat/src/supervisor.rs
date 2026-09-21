use std::sync::{Arc, Mutex, RwLock};

use gateway_core::SessionId;
use gateway_core::SessionManager;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::api::WeixinApi;
use crate::bridge::{Binding, WechatBridge};
use crate::journal::Journal;
use crate::poller::Poller;
use crate::types::Credentials;

/// Owns the two tasks for one embedded binding and gives daemon shutdown one
/// cancellation boundary.
#[derive(Debug)]
pub struct WechatSupervisor {
    cancel: CancellationToken,
    rebind_tx: Option<mpsc::Sender<SessionId>>,
    binding_session: Option<Arc<RwLock<SessionId>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl WechatSupervisor {
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.rebind_tx.is_some()
    }

    /// Current session shown by the local health/UI surface.
    pub fn binding_session_id(&self) -> Option<String> {
        self.binding_session
            .as_ref()
            .and_then(|session| session.read().ok().map(|session| session.to_string()))
    }

    pub fn start<A, J>(
        manager: Arc<SessionManager>,
        api: Arc<A>,
        journal: Arc<J>,
        credentials: Credentials,
        binding: Binding,
    ) -> Self
    where
        A: WeixinApi + 'static,
        J: Journal + 'static,
    {
        Self::start_with_limits(
            manager,
            api,
            journal,
            credentials,
            binding,
            crate::types::MAX_TEXT_BYTES,
            crate::types::MAX_CHUNK_BYTES,
        )
    }

    pub fn start_with_limits<A, J>(
        manager: Arc<SessionManager>,
        api: Arc<A>,
        journal: Arc<J>,
        credentials: Credentials,
        binding: Binding,
        max_text_bytes: usize,
        max_chunk_bytes: usize,
    ) -> Self
    where
        A: WeixinApi + 'static,
        J: Journal + 'static,
    {
        let cancel = CancellationToken::new();
        let bridge = Arc::new(WechatBridge::new_with_limits(
            manager,
            binding.clone(),
            credentials.clone(),
            Arc::clone(&api),
            Arc::clone(&journal),
            cancel.clone(),
            max_text_bytes,
            max_chunk_bytes,
        ));
        let bridge_task = tokio::spawn({
            let bridge = Arc::clone(&bridge);
            async move {
                if let Err(error) = bridge.run().await {
                    tracing::warn!(error = %error, "wechat session bridge stopped");
                }
            }
        });
        let binding_session = Arc::new(RwLock::new(binding.session_id.clone()));
        let (rebind_tx, mut rebind_rx) = mpsc::channel::<SessionId>(16);
        let rebind_bridge = Arc::clone(&bridge);
        let rebind_session = Arc::clone(&binding_session);
        let rebind_task = tokio::spawn(async move {
            while let Some(session_id) = rebind_rx.recv().await {
                rebind_bridge.rebind(session_id.clone()).await;
                if let Ok(mut current) = rebind_session.write() {
                    *current = session_id;
                }
            }
        });
        let poller = Poller::new(
            api,
            journal,
            credentials,
            binding.binding_id,
            cancel.clone(),
        )
        .with_max_text_bytes(max_text_bytes);
        let poller_task = tokio::spawn({
            let bridge = Arc::clone(&bridge);
            async move {
                let result = poller
                    .run(|message| {
                        let bridge = Arc::clone(&bridge);
                        async move {
                            bridge.accept_inbound(&message).await.map_err(|error| {
                                crate::poller::PollerError::Bridge(error.to_string())
                            })
                        }
                    })
                    .await;
                if let Err(error) = result {
                    tracing::warn!(error = %error, "wechat poller stopped");
                }
            }
        });
        Self {
            cancel,
            rebind_tx: Some(rebind_tx),
            binding_session: Some(binding_session),
            tasks: Mutex::new(vec![bridge_task, poller_task, rebind_task]),
        }
    }

    /// Switch the WeChat bridge to another live Gateway session.
    pub async fn rebind(&self, session_id: SessionId) -> Result<(), String> {
        let Some(sender) = &self.rebind_tx else {
            return Err("WeChat adapter is disabled".to_owned());
        };
        sender
            .send(session_id)
            .await
            .map_err(|_| "WeChat adapter is stopped".to_owned())
    }

    #[must_use]
    pub fn disabled() -> Self {
        Self {
            cancel: CancellationToken::new(),
            rebind_tx: None,
            binding_session: None,
            tasks: Mutex::new(Vec::new()),
        }
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
        if let Ok(mut tasks) = self.tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
    }
}

impl Drop for WechatSupervisor {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
