use crate::RosLibRustError;

use super::tcpros::ConnectionHeader;
use abort_on_drop::ChildTask;
use roslibrust_codegen::RosMessageType;
use std::{
    marker::PhantomData,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, RwLock},
};

pub struct Publisher<T> {
    topic_name: String,
    sender: mpsc::Sender<Vec<u8>>,
    phantom: PhantomData<T>,
}

impl<T: RosMessageType> Publisher<T> {
    pub(crate) fn new(topic_name: &str, sender: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            topic_name: topic_name.to_owned(),
            sender,
            phantom: PhantomData,
        }
    }

    pub async fn publish(&self, data: &T) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let data = serde_rosmsg::to_vec(&data)
            // Gotta do some funny error mapping here as serde_rosmsg's error type is not sync
            .map_err(|e| RosLibRustError::Unexpected(anyhow::anyhow!("{e:?}")))?;
        self.sender.send(data).await?;
        log::debug!("Publishing data on topic {}", self.topic_name);
        Ok(())
    }
}

pub struct Publication {
    topic_type: String,
    listener_port: u16,
    _channel_task: ChildTask<()>,
    _publish_task: ChildTask<()>,
    publish_sender: mpsc::Sender<Vec<u8>>,
}

impl Publication {
    pub async fn new(
        node_name: &str,
        latching: bool,
        topic_name: &str,
        host_addr: Ipv4Addr,
        queue_size: usize,
        msg_definition: &str,
        md5sum: &str,
        topic_type: &str,
    ) -> Result<Self, std::io::Error> {
        let host_addr = SocketAddr::from((host_addr, 0));
        let tcp_listener = tokio::net::TcpListener::bind(host_addr).await?;
        let listener_port = tcp_listener.local_addr().unwrap().port();

        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(queue_size);

        let responding_conn_header = ConnectionHeader {
            caller_id: node_name.to_owned(),
            latching,
            msg_definition: msg_definition.to_owned(),
            md5sum: md5sum.to_owned(),
            topic: topic_name.to_owned(),
            topic_type: topic_type.to_owned(),
            tcp_nodelay: false,
        };

        let subscriber_streams = Arc::new(RwLock::new(Vec::new()));

        let subscriber_streams_copy = subscriber_streams.clone();
        let listener_handle = tokio::spawn(async move {
            let subscriber_streams = subscriber_streams_copy;
            loop {
                if let Ok((mut stream, peer_addr)) = tcp_listener.accept().await {
                    let topic_name = responding_conn_header.topic.as_str();
                    log::info!(
                        "Received connection from subscriber at {peer_addr} for topic {topic_name}"
                    );
                    let mut connection_header = Vec::with_capacity(16 * 1024);
                    if let Ok(bytes) = stream.read_buf(&mut connection_header).await {
                        if let Ok(connection_header) =
                            ConnectionHeader::from_bytes(&connection_header[..bytes])
                        {
                            if connection_header.md5sum == responding_conn_header.md5sum {
                                log::debug!(
                                    "Received subscribe request for {}",
                                    connection_header.topic
                                );
                                // Write our own connection header in response
                                let response_header_bytes = responding_conn_header
                                    .to_bytes(false)
                                    .expect("Couldn't serialize connection header");
                                stream
                                    .write(&response_header_bytes[..])
                                    .await
                                    .expect("Unable to respond on tcpstream");
                                let mut wlock = subscriber_streams.write().await;
                                wlock.push(stream);
                                log::debug!(
                                    "Added stream for topic {} to subscriber {}",
                                    connection_header.topic,
                                    peer_addr
                                );
                            }
                        } else {
                            let header_str = connection_header[..bytes]
                                .into_iter()
                                .map(|ch| if *ch < 128 { *ch as char } else { '.' })
                                .collect::<String>();
                            log::error!(
                                "Failed to parse connection header: ({bytes} bytes) {header_str}",
                            )
                        }
                    }
                }
            }
        });

        let publish_task = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Some(msg_to_publish) => {
                        let mut streams = subscriber_streams.write().await;
                        let mut streams_to_remove = vec![];
                        for (stream_idx, stream) in streams.iter_mut().enumerate() {
                            if let Err(err) = stream.write(&msg_to_publish[..]).await {
                                // TODO: A single failure between nodes that cross host boundaries is probably normal, should make this more robust perhaps
                                log::debug!("Failed to send data to subscriber: {err}, removing");
                                streams_to_remove.push(stream_idx);
                            }
                        }
                        // Subtract the removed count to account for shifting indices after each
                        // remove, only works if they're sorted which should be the case given how
                        // it's being populated (forward enumeration)
                        streams_to_remove.into_iter().enumerate().for_each(
                            |(removed_cnt, stream_idx)| {
                                streams.remove(stream_idx - removed_cnt);
                            },
                        );
                    }
                    None => {
                        log::debug!("No more senders for the publisher channel, exiting...");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            topic_type: topic_type.to_owned(),
            _channel_task: listener_handle.into(),
            listener_port,
            publish_sender: sender,
            _publish_task: publish_task.into(),
        })
    }

    pub fn get_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.publish_sender.clone()
    }

    pub fn port(&self) -> u16 {
        self.listener_port
    }

    pub fn topic_type(&self) -> &str {
        &self.topic_type
    }
}
