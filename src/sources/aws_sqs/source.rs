use std::{collections::HashMap, iter, panic, str::FromStr, sync::Arc};

use aws_sdk_sqs::{
    model::{DeleteMessageBatchRequestEntry, MessageSystemAttributeName, QueueAttributeName},
    Client as SqsClient,
};
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, TimeZone, Utc};
use codecs::StreamDecodingError;
use futures::FutureExt;
use tokio::{pin, select, time::Duration};
use tokio_util::codec::Decoder as _;
use vector_common::byte_size_of::ByteSizeOf;

use crate::{
    codecs::Decoder,
    config::log_schema,
    event::{BatchNotifier, Event},
    internal_events::{
        AwsSqsBytesReceived, EventsReceived, SqsMessageDeleteError, StreamClosedError,
    },
    shutdown::ShutdownSignal,
    sources::util::finalizer::UnorderedFinalizer,
    SourceSender,
};

// This is the maximum SQS supports in a single batch request
const MAX_BATCH_SIZE: i32 = 10;

type Finalizer = UnorderedFinalizer<Vec<String>>;

#[derive(Clone)]
pub struct SqsSource {
    pub client: SqsClient,
    pub queue_url: String,
    pub decoder: Decoder,
    pub poll_secs: u32,
    pub visibility_timeout_secs: u32,
    pub delete_message: bool,
    pub concurrency: u32,
    pub(super) acknowledgements: bool,
}

impl SqsSource {
    pub async fn run(self, out: SourceSender, shutdown: ShutdownSignal) -> Result<(), ()> {
        let mut task_handles = vec![];
        let shutdown = shutdown.shared();
        let finalizer = self.acknowledgements.then(|| {
            let client = self.client.clone();
            let queue_url = self.queue_url.clone();
            Arc::new(Finalizer::new(shutdown.clone(), move |receipts_to_ack| {
                delete_messages(client.clone(), receipts_to_ack, queue_url.clone())
            }))
        });

        for _ in 0..self.concurrency {
            let source = self.clone();
            let shutdown = shutdown.clone().fuse();
            let mut out = out.clone();
            let finalizer = finalizer.clone();
            task_handles.push(tokio::spawn(async move {
                pin!(shutdown);
                loop {
                    select! {
                        _ = &mut shutdown => break,
                        _ = source.run_once(&mut out, finalizer.clone()) => {},
                    }
                }
            }));
        }

        // Wait for all of the processes to finish.  If any one of them panics, we resume
        // that panic here to properly shutdown Vector.
        for task_handle in task_handles.drain(..) {
            if let Err(e) = task_handle.await {
                if e.is_panic() {
                    panic::resume_unwind(e.into_panic());
                }
            }
        }
        Ok(())
    }

    async fn run_once(&self, out: &mut SourceSender, finalizer: Option<Arc<Finalizer>>) {
        let result = self
            .client
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(MAX_BATCH_SIZE)
            .wait_time_seconds(self.poll_secs as i32)
            .visibility_timeout(self.visibility_timeout_secs as i32)
            // I think this should be a known attribute
            // https://github.com/awslabs/aws-sdk-rust/issues/411
            .attribute_names(QueueAttributeName::Unknown(String::from("SentTimestamp")))
            .send()
            .await;

        let receive_message_output = match result {
            Ok(output) => output,
            Err(err) => {
                error!("SQS receive message error: {:?}.", err);
                // prevent rapid errors from flooding the logs
                tokio::time::sleep(Duration::from_secs(1)).await;
                return;
            }
        };

        if let Some(messages) = receive_message_output.messages {
            let mut receipts_to_ack = Vec::with_capacity(messages.len());
            let mut events = Vec::with_capacity(messages.len());
            let mut byte_size = 0;

            let (batch, batch_receiver) =
                BatchNotifier::maybe_new_with_receiver(finalizer.is_some());
            for message in messages {
                if let Some(body) = message.body {
                    byte_size += body.len();
                    // a receipt handle should always exist
                    if let Some(receipt_handle) = message.receipt_handle {
                        receipts_to_ack.push(receipt_handle);
                    }
                    let timestamp = get_timestamp(&message.attributes);
                    let decoded = decode_message(self.decoder.clone(), body.as_bytes(), timestamp);
                    if let Some(batch) = batch.as_ref() {
                        let decoded = decoded.map(|event| event.with_batch_notifier(batch));
                        events.extend(decoded);
                    } else {
                        events.extend(decoded);
                    }
                }
            }
            drop(batch); // Drop last reference to batch acknowledgement finalizer
            emit!(AwsSqsBytesReceived { byte_size });
            let count = events.len();

            match out.send_batch(events).await {
                Ok(()) => {
                    if self.delete_message {
                        match batch_receiver {
                            Some(receiver) => finalizer
                                .expect("Finalizer must exist for the batch receiver to be created")
                                .add(receipts_to_ack, receiver),
                            None => {
                                delete_messages(
                                    self.client.clone(),
                                    receipts_to_ack,
                                    self.queue_url.clone(),
                                )
                                .await
                            }
                        }
                    }
                }
                Err(error) => emit!(StreamClosedError { error, count }),
            }
        }
    }
}

fn get_timestamp(
    attributes: &Option<HashMap<MessageSystemAttributeName, String>>,
) -> Option<DateTime<Utc>> {
    attributes.as_ref().and_then(|attributes| {
        let sent_time_str = attributes.get(&MessageSystemAttributeName::SentTimestamp)?;
        Some(Utc.timestamp_millis(i64::from_str(sent_time_str).ok()?))
    })
}

async fn delete_messages(client: SqsClient, receipts: Vec<String>, queue_url: String) {
    if !receipts.is_empty() {
        let mut batch = client.delete_message_batch().queue_url(queue_url);

        for (id, receipt) in receipts.into_iter().enumerate() {
            batch = batch.entries(
                DeleteMessageBatchRequestEntry::builder()
                    .id(id.to_string())
                    .receipt_handle(receipt)
                    .build(),
            );
        }
        if let Err(err) = batch.send().await {
            emit!(SqsMessageDeleteError { error: &err });
        }
    }
}

fn decode_message(
    mut decoder: Decoder,
    message: &[u8],
    sent_time: Option<DateTime<Utc>>,
) -> impl Iterator<Item = Event> {
    let schema = log_schema();

    let mut buffer = BytesMut::with_capacity(message.len());
    buffer.extend_from_slice(message);

    iter::from_fn(move || loop {
        break match decoder.decode_eof(&mut buffer) {
            Ok(Some((events, _))) => {
                let count = events.len();
                Some(
                    events
                        .into_iter()
                        .map(move |mut event| {
                            if let Event::Log(ref mut log) = event {
                                log.try_insert(schema.source_type_key(), Bytes::from("aws_sqs"));
                                if let Some(sent_time) = sent_time {
                                    log.try_insert(schema.timestamp_key(), sent_time);
                                }
                            }
                            event
                        })
                        .fold_finally(
                            0,
                            |size, event: &Event| size + event.size_of(),
                            move |byte_size| emit!(EventsReceived { byte_size, count }),
                        ),
                )
            }
            Err(error) => {
                // Error is logged by `crate::codecs::Decoder`, no further handling
                // is needed here.
                if error.can_continue() {
                    continue;
                }
                None
            }
            Ok(None) => None,
        };
    })
    .flatten()
}

trait FoldFinallyExt: Sized {
    /// This adapter applies the `folder` function to every element in
    /// the iterator, much as `Iterator::fold` does. However, instead
    /// of returning the resulting folded value, it calls the
    /// `finally` function after the last element. This function
    /// returns an iterator over the original values.
    fn fold_finally<A, Fo, Fi>(
        self,
        initial: A,
        folder: Fo,
        finally: Fi,
    ) -> FoldFinally<Self, A, Fo, Fi>;
}

impl<I: Iterator + Sized> FoldFinallyExt for I {
    fn fold_finally<A, Fo, Fi>(
        self,
        initial: A,
        folder: Fo,
        finally: Fi,
    ) -> FoldFinally<Self, A, Fo, Fi> {
        FoldFinally {
            inner: self,
            accumulator: initial,
            folder,
            finally,
        }
    }
}

struct FoldFinally<I, A, Fo, Fi> {
    inner: I,
    accumulator: A,
    folder: Fo,
    finally: Fi,
}

impl<I, A, Fo, Fi> Iterator for FoldFinally<I, A, Fo, Fi>
where
    I: Iterator,
    A: Copy,
    Fo: FnMut(A, &I::Item) -> A,
    Fi: Fn(A),
{
    type Item = I::Item;
    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            Some(item) => {
                self.accumulator = (self.folder)(self.accumulator, &item);
                Some(item)
            }
            None => {
                (self.finally)(self.accumulator);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::SecondsFormat;

    use super::*;

    #[tokio::test]
    async fn test_decode() {
        let message = "test";
        let now = Utc::now();
        let events: Vec<_> = decode_message(Decoder::default(), b"test", Some(now)).collect();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]
                .clone()
                .as_log()
                .get(log_schema().message_key())
                .unwrap()
                .to_string_lossy(),
            message
        );
        assert_eq!(
            events[0]
                .clone()
                .as_log()
                .get(log_schema().timestamp_key())
                .unwrap()
                .to_string_lossy(),
            now.to_rfc3339_opts(SecondsFormat::AutoSi, true)
        );
    }

    #[test]
    fn test_get_timestamp() {
        let attributes = HashMap::from([(
            MessageSystemAttributeName::SentTimestamp,
            "1636408546018".to_string(),
        )]);

        assert_eq!(
            get_timestamp(&Some(attributes)),
            Some(Utc.timestamp_millis(1636408546018))
        );
    }
}
