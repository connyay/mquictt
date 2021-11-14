use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use bytes::{Bytes, BytesMut};
use futures::{stream::FuturesUnordered, StreamExt};
use mqttbytes::v4;
use slab::Slab;

use crate::{Connection, Error, QuicServer};

type DataTx = flume::Sender<Bytes>;
type DataRx = flume::Receiver<Bytes>;

type SubReqTx = flume::Sender<DataTx>;
type SubReqRx = flume::Receiver<DataTx>;
type Mapper = Arc<RwLock<HashMap<String, SubReqTx>>>;

pub async fn server(addr: &SocketAddr) -> Result<(), Error> {
    let mut listener = QuicServer::new(addr)?;
    let mapper: Mapper = Arc::new(RwLock::new(HashMap::default()));

    loop {
        let conn = match listener.accept().await {
            Ok(conn) => conn,
            Err(Error::ConnectionBroken) => return Ok(()),
            e => e?,
        };
        let mapper = mapper.clone();

        tokio::spawn(connection_handler(conn, mapper));
    }
}

async fn connection_handler(mut conn: Connection, mapper: Mapper) -> Result<(), Error> {
    let (mut tx, mut rx) = conn.accept().await?;
    let mut buf = BytesMut::new();

    rx.read(&mut buf).await?;
    loop {
        match v4::read(&mut buf, 1024 * 1024) {
            Ok(v4::Packet::Connect(_)) => break,
            Ok(_) => continue,
            Err(mqttbytes::Error::InsufficientBytes(_)) => {
                rx.read(&mut buf).await?;
                continue;
            }
            Err(e) => return Err(Error::MQTT(e)),
        }
    }

    buf.clear();
    if let Err(e) = v4::ConnAck::new(v4::ConnectReturnCode::Success, false).write(&mut buf) {
        return Err(Error::MQTT(e));
    }
    let _write = tx.write(&buf).await?;

    buf.clear();
    loop {
        let (tx, rx) = conn.accept().await?;
        tokio::spawn(handle_new_stream(tx, rx, mapper.clone()));

        // tokio::select! {
        //     streams_result = conn.accept() => {
        //         let (tx, rx) = streams_result?;
        //         tokio::spawn(handle_new_stream(tx, rx, mapper.clone()));
        //     }
        // }
    }
}

async fn handle_new_stream(
    tx: quinn::SendStream,
    mut rx: quinn::RecvStream,
    mapper: Mapper,
) -> Result<(), Error> {
    let mut buf = BytesMut::new();

    rx.read(&mut buf).await?;
    loop {
        match v4::read(&mut buf, 1024 * 1024) {
            Ok(v4::Packet::Publish(v4::Publish { topic, .. })) => {
                // ignoring first publish's payload as there are no subscribers
                // TODO: handle case when subsribing to topic that is not in mapper
                let (sub_req_tx, sub_req_rx) = flume::bounded(1024);
                {
                    let mut map_writer = mapper.write().unwrap();
                    map_writer.insert(topic, sub_req_tx);
                }
                return handle_publish(rx, sub_req_rx, buf).await;
            }
            Ok(v4::Packet::Subscribe(v4::Subscribe { filters, .. })) => {
                // only handling a single subsribe for now, as client only sends 1 subscribe at a
                // time
                //
                // TODO: handle multiple subs
                let filter = match filters.get(0) {
                    Some(filter) => filter,
                    None => return Ok(()),
                };
                let (data_tx, data_rx) = flume::bounded(1024);
                {
                    let map_reader = mapper.read().unwrap();
                    // TODO: handle case when subsribing to topic that is not in mapper
                    let sub_req_tx = map_reader.get(&filter.path).unwrap();
                    // waiting blockingly as we are not allowed to await when holding a lock
                    sub_req_tx.send(data_tx)?;
                }
                return handle_subscribe(tx, rx, data_rx).await;
            }
            Ok(_) => continue,
            Err(mqttbytes::Error::InsufficientBytes(_)) => {
                rx.read(&mut buf).await?;
                continue;
            }
            Err(e) => return Err(Error::MQTT(e)),
        }
    }
}

async fn handle_publish(
    mut rx: quinn::RecvStream,
    sub_req_rx: SubReqRx,
    mut buf: BytesMut,
) -> Result<(), Error> {
    let mut subscribers: Slab<Arc<DataTx>> = Slab::with_capacity(1024);
    let mut send_queue = FuturesUnordered::new();
    let mut send_queue_empty = true;

    loop {
        tokio::select! {
            read = rx.read(&mut buf) => {
                let _len = match read? {
                    Some(len) => len,
                    None => break,
                };

                let v4::Publish { payload, .. } = match v4::read(&mut buf, 1024 * 1024) {
                    Ok(v4::Packet::Publish(publish)) => publish,
                    Ok(_) | Err(mqttbytes::Error::InsufficientBytes(_)) => continue,
                    Err(e) => return Err(Error::MQTT(e)),
                };

                for (slab_id, subsriber) in subscribers.iter() {
                    let subsriber = subsriber.clone();
                    let payload = payload.clone();
                    send_queue.push(async move {
                        match subsriber.send_async(payload).await {
                            Ok(_) => None,
                            Err(e) => Some((slab_id, e)),
                        }
                    });
                }
            }

            sub_req = sub_req_rx.recv_async() => {
                // if cannot be recved then mapper has been droped, exit normally
                let data_tx = match sub_req {
                    Ok(v) => v,
                    Err(_) => break,
                };
                subscribers.insert(Arc::new(data_tx));
            }

            send_opt = send_queue.next(), if !send_queue_empty => {
                match send_opt {
                    Some(Some((slab_id, _e))) => {
                        subscribers.remove(slab_id);
                    },
                    Some(None) => {}
                    None => send_queue_empty = false,
                }
            }
        }
    }

    Ok(())
}

async fn handle_subscribe(
    tx: quinn::SendStream,
    rx: quinn::RecvStream,
    data_rx: DataRx,
) -> Result<(), Error> {
    unimplemented!();
    Ok(())
}
