use super::*;
use aion_types::llm::LlmEvent;
use aion_types::message::{StopReason, TokenUsage};

#[tokio::test]
async fn aggregate_response_builds_completed_response_from_text_events() {
    let (tx, rx) = mpsc::channel(8);
    tx.send(LlmEvent::TextDelta("Hello".into())).await.unwrap();
    tx.send(LlmEvent::TextDelta(", world".into())).await.unwrap();
    tx.send(LlmEvent::Done {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 3,
            output_tokens: 2,
            ..Default::default()
        },
    })
    .await
    .unwrap();
    drop(tx);

    let response = aggregate_response(rx, "kimi-k3".into()).await;

    assert_eq!(response["status"], "completed");
    assert_eq!(response["output"][0]["content"][0]["text"], "Hello, world");
    assert_eq!(response["usage"]["input_tokens"], 3);
    assert_eq!(response["usage"]["output_tokens"], 2);
}

#[tokio::test]
async fn aggregate_response_defensive_default_when_channel_closes_without_done() {
    let (tx, rx) = mpsc::channel(8);
    tx.send(LlmEvent::TextDelta("partial".into())).await.unwrap();
    drop(tx);

    let response = aggregate_response(rx, "kimi-k3".into()).await;

    assert_eq!(response["status"], "failed");
}

#[tokio::test]
async fn stream_events_forwards_events_in_order_and_stops_after_done() {
    let (tx, rx) = mpsc::channel(8);
    tx.send(LlmEvent::TextDelta("hi".into())).await.unwrap();
    tx.send(LlmEvent::Done {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    })
    .await
    .unwrap();

    let mut out = stream_events(rx, "kimi-k3".into());
    let mut names = Vec::new();
    while let Some(event) = out.recv().await {
        names.push(event.name);
    }

    assert_eq!(
        names,
        vec![
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
}

#[tokio::test]
async fn stream_events_finalizes_when_upstream_channel_drops_without_done() {
    let (tx, rx) = mpsc::channel(8);
    tx.send(LlmEvent::TextDelta("partial".into())).await.unwrap();
    drop(tx);

    let mut out = stream_events(rx, "kimi-k3".into());
    let mut names = Vec::new();
    while let Some(event) = out.recv().await {
        names.push(event.name);
    }

    assert_eq!(
        names,
        vec![
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "error",
            "response.completed",
        ]
    );
}
