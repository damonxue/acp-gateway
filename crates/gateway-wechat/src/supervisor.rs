use std::sync::{Arc, Mutex};

use gateway_core::SessionManager;
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
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl WechatSupervisor {
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
            tasks: Mutex::new(vec![bridge_task, poller_task]),
        }
    }

    #[must_use]
    pub fn disabled() -> Self {
        Self {
            cancel: CancellationToken::new(),
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
