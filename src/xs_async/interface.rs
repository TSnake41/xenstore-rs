use std::{cell::Cell, collections::HashMap};

use anyhow::{anyhow, bail};
use futures::{
    channel::{mpsc, oneshot},
    io::{self, Error, ErrorKind},
    SinkExt, StreamExt,
};
use log::{debug, info, warn};
use uuid::Uuid;

use crate::wire::{XsMessage, XsMessageType};

/// Maximum number of pending requests.
const MAX_REQUEST_COUNT: usize = 32;

#[derive(Clone, Copy)]
pub struct XsWatchToken(Uuid);

pub struct XsAsyncRequest {
    pub request: XsMessage,
    pub response_sender: oneshot::Sender<XsMessage>,
}

pub enum XsAsyncMessage {
    Request(XsAsyncRequest),
    WatchSubscribe {
        path: Box<str>,
        event_sender: mpsc::Sender<Box<str>>,
        result_channel: oneshot::Sender<io::Result<XsWatchToken>>,
    },
    WatchUnsubscribe(XsWatchToken),
}

enum XsAsyncTask {
    Request(oneshot::Sender<XsMessage>),
    WatchSubscribe {
        subscriber_info: WatchSubscriberInfo,
        result_channel: oneshot::Sender<io::Result<XsWatchToken>>,
        token: XsWatchToken,
    },
    WatchUnsubscribe(XsWatchToken),
}

struct WatchSubscriberInfo {
    channel: mpsc::Sender<Box<str>>,
    // We need to store the watch patch as it is required by UNWATCH.
    path: Box<str>,
}

#[derive(Default)]
pub struct XsAsyncState {
    pending_tasks: [Cell<Option<XsAsyncTask>>; MAX_REQUEST_COUNT],
    watch_subscribers: HashMap<Uuid, WatchSubscriberInfo>,
    task_count: usize,
}

fn find_suitable_token<V>(watch_subscribers: &HashMap<Uuid, V>) -> XsWatchToken {
    // loop until there is no collision
    loop {
        let uuid = Uuid::new_v4();

        if watch_subscribers.get(&uuid).is_none() {
            return XsWatchToken(uuid);
        }
    }
}

impl XsAsyncState {
    async fn process_message(
        &mut self,
        message: XsAsyncMessage,
        xs_sender: &mut mpsc::Sender<XsMessage>,
    ) -> anyhow::Result<()> {
        // Find a available task slot.
        let Some((req_id, slot)) = self
            .pending_tasks
            .iter_mut()
            .map(|slot| slot.get_mut())
            .enumerate()
            .find(|(_, slot)| slot.is_none())
        else {
            bail!("No available slot");
        };

        match message {
            XsAsyncMessage::Request(XsAsyncRequest {
                mut request,
                response_sender,
            }) => {
                request.request_id = req_id as u32;

                xs_sender.send(request).await?;
                *slot = Some(XsAsyncTask::Request(response_sender));
                self.task_count += 1;
            }
            XsAsyncMessage::WatchSubscribe {
                path,
                event_sender: channel,
                result_channel,
            } => {
                let token = find_suitable_token(&self.watch_subscribers);

                // Make the actual WATCH command
                xs_sender
                    .send(XsMessage::from_string_slice(
                        XsMessageType::Watch,
                        req_id as u32,
                        &[&path, &token.0.to_string()],
                    ))
                    .await?;

                // Wait until we got confirmation of the WATCH command by upstream.
                *slot = Some(XsAsyncTask::WatchSubscribe {
                    subscriber_info: WatchSubscriberInfo { channel, path },
                    result_channel,
                    token,
                });
                self.task_count += 1;
            }
            XsAsyncMessage::WatchUnsubscribe(token) => {
                let Some(WatchSubscriberInfo { path, .. }) = self.watch_subscribers.get(&token.0)
                else {
                    bail!("Attempting unwatch without watch.");
                };

                // Make the actual UNWATCH command
                xs_sender
                    .send(XsMessage::from_string_slice(
                        XsMessageType::Unwatch,
                        req_id as u32,
                        &[path, &token.0.to_string()],
                    ))
                    .await?;

                // Wait until we got confirmation of the WATCH command by upstream.
                *slot = Some(XsAsyncTask::WatchUnsubscribe(token));
                self.task_count += 1;
            }
        }

        Ok(())
    }

    async fn process_response(&mut self, response: XsMessage) -> anyhow::Result<()> {
        if response.msg_type == XsMessageType::WatchEvent {
            // Process a watch event (it's always req_id = 0) and is unsolicitated.
            return self.process_watch_entry(response).await;
        }

        // All other requests have a req_id and is solicitated,
        // thus they have a related pending_tasks entry.

        // Take a reference the the task slot (if any).
        let Some(slot) = self.pending_tasks.get_mut(response.request_id as usize) else {
            bail!("Invalid req_id received")
        };

        // Take it (leaving None at the place).
        let Some(task) = slot.take() else {
            bail!("No related request to this req_id")
        };
        self.task_count -= 1;

        match task {
            XsAsyncTask::Request(sender) => {
                // Usual request, forward response to caller (even if it is Error variant).
                sender.send(response).ok();
            }
            XsAsyncTask::WatchSubscribe {
                token,
                result_channel,
                subscriber_info,
            } => {
                match response.msg_type {
                    XsMessageType::Watch => {
                        // Now the watch is registered upstream, make it work here.
                        if self
                            .watch_subscribers
                            .insert(token.0, subscriber_info)
                            .is_some()
                        {
                            warn!("Overriden WATCH subscriber");
                        }

                        // Report that things got successful.
                        result_channel.send(Ok(token)).ok();
                    }
                    XsMessageType::Error => {
                        result_channel.send(Err(response.parse_error())).ok();
                    }
                    response => {
                        result_channel
                            .send(Err(Error::new(
                                ErrorKind::InvalidData,
                                format!("Got unexpected response ({response:?})"),
                            )))
                            .ok();
                        bail!("Got invalid response to WATCH command")
                    }
                }
            }
            XsAsyncTask::WatchUnsubscribe(token) => {
                match response.msg_type {
                    XsMessageType::Unwatch => {
                        // Make unwatch effective
                        if self.watch_subscribers.remove(&token.0).is_none() {
                            warn!("Unwatched nothing");
                        }
                    }
                    XsMessageType::Error => bail!("Unwatch failure"),
                    _ => bail!("Got invalid response to WATCH command"),
                }
            }
        }

        Ok(())
    }

    async fn process_watch_entry(&mut self, message: XsMessage) -> Result<(), anyhow::Error> {
        let [value, token] = message.parse_payload_list()?[..] else {
            bail!("Invalid watch event payload received")
        };

        let uuid = Uuid::try_parse(token).map_err(|_| anyhow!("Got non-UUID token"))?;

        if let Some(subscriber) = self.watch_subscribers.get_mut(&uuid) {
            if let Err(e) = subscriber.channel.send(value.into()).await {
                warn!("Lost watch subscriber: {e}");

                // Subscriber is dead, remove it.
                self.watch_subscribers.remove(&uuid);
            }
        } else {
            warn!("Unregistered watch message ? ({uuid})");
        }

        Ok(())
    }

    pub async fn run(
        mut self,
        mut message_channel: mpsc::Receiver<XsAsyncMessage>,
        mut xs_receiver: mpsc::Receiver<XsMessage>,
        mut xs_sender: mpsc::Sender<XsMessage>,
    ) {
        loop {
            if self.task_count == MAX_REQUEST_COUNT {
                // We can't process another task, only interface responses.
                debug!("Too much tasks");
                let Some(response) = xs_receiver.next().await else {
                    break;
                };

                if let Err(e) = self.process_response(response).await {
                    warn!("Process response failure: {e}")
                }
            } else {
                futures::select! {
                    response = xs_receiver.select_next_some() => {
                        if let Err(e) = self.process_response(response).await {
                            warn!("Process response failure: {e}")
                        }
                    },
                    message = message_channel.select_next_some() => {
                        if let Err(e) = self.process_message(message, &mut xs_sender).await {
                            warn!("Process message failure: {e}")
                        }
                    },
                    // In case we get a None, something is dead in the loop, stop here.
                    default => break,
                }
            }
        }

        info!("Communication channel died");
    }
}
