use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gateway_wechat::{
    Credentials, Journal, LoginPoll, MemoryJournal, OutboundRole, OutboundSender, Poller,
    QrChallenge, SendMessage, Updates, WeixinApi, WeixinError,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
struct FakeApi {
    updates: Mutex<VecDeque<Updates>>,
    sent: Mutex<Vec<SendMessage>>,
}

#[async_trait]
impl WeixinApi for FakeApi {
    async fn begin_login(&self) -> Result<QrChallenge, WeixinError> {
        Err(WeixinError::Protocol("unused".into()))
    }

    async fn poll_login(
        &self,
        _challenge: &QrChallenge,
        _verify_code: Option<&str>,
    ) -> Result<LoginPoll, WeixinError> {
        Err(WeixinError::Protocol("unused".into()))
    }

    async fn updates(
        &self,
        _credentials: &Credentials,
        _cursor: &str,
    ) -> Result<Updates, WeixinError> {
        Ok(self.updates.lock().unwrap().pop_front().unwrap_or(Updates {
            messages: Vec::new(),
            cursor: None,
        }))
    }

    async fn send(
        &self,
        _credentials: &Credentials,
        message: &SendMessage,
    ) -> Result<(), WeixinError> {
        self.sent.lock().unwrap().push(message.clone());
        Ok(())
    }
}

fn credentials() -> Credentials {
    Credentials {
        bot_id: "bot".into(),
        owner_id: "owner".into(),
        token: "token".into(),
        base_url: "https://ilinkai.weixin.qq.com".into(),
    }
}

fn raw_message(id: &str) -> Value {
    serde_json::json!({
        "from_user_id": "owner", "to_user_id": "bot", "message_id": id,
        "message_type": 1, "message_state": 2, "context_token": "ctx",
        "item_list": [{"type": 1, "text_item": {"text": "hello"}}]
    })
}

#[tokio::test]
async fn poller_deduplicates_and_advances_cursor() {
    let api = Arc::new(FakeApi::default());
    api.updates.lock().unwrap().push_back(Updates {
        messages: vec![raw_message("m1"), raw_message("m1")],
        cursor: Some("next".into()),
    });
    let journal = Arc::new(MemoryJournal::new());
    let poller = Poller::new(
        Arc::clone(&api),
        Arc::clone(&journal),
        credentials(),
        "binding",
        CancellationToken::new(),
    );
    let received = Arc::new(Mutex::new(Vec::new()));
    let received_for_callback = Arc::clone(&received);
    let count = poller
        .poll_once(move |message| {
            received_for_callback.lock().unwrap().push(message.id);
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(received.lock().unwrap().len(), 1);
    assert_eq!(journal.cursor("binding").await.unwrap(), "next");
}

#[tokio::test]
async fn outbound_chunks_and_marks_sent() {
    let api = Arc::new(FakeApi::default());
    let journal = Arc::new(MemoryJournal::new());
    let sender = OutboundSender::new(Arc::clone(&api), Arc::clone(&journal));
    let result = sender
        .send_text(
            &credentials(),
            "binding",
            "source",
            OutboundRole::VsCodeUser,
            &"x".repeat(4_000),
            Some("ctx"),
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        gateway_wechat::SendResult::Sent { parts: 2 }
    ));
    let sent = api.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[0].item_list[0]
            .text_item
            .text
            .starts_with("[acp-gw User 1/2]\n")
    );
    assert!(sent.iter().all(|message| message.context_token == "ctx"));
}

#[tokio::test]
async fn waiting_for_context_is_flushed_after_first_inbound_context() {
    let api = Arc::new(FakeApi::default());
    let journal = Arc::new(MemoryJournal::new());
    let sender = OutboundSender::new(Arc::clone(&api), Arc::clone(&journal));
    assert!(matches!(
        sender
            .send_text(
                &credentials(),
                "binding",
                "queued",
                OutboundRole::Agent,
                "queued",
                None
            )
            .await
            .unwrap(),
        gateway_wechat::SendResult::WaitingForContext
    ));
    assert!(matches!(
        sender
            .send_text(
                &credentials(),
                "binding",
                "current",
                OutboundRole::Agent,
                "current",
                Some("ctx")
            )
            .await
            .unwrap(),
        gateway_wechat::SendResult::Sent { parts: 1 }
    ));
    assert_eq!(api.sent.lock().unwrap().len(), 2);
}
