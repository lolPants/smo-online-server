use crate::{
    peer::Peer,
    players::{Players, SharedPlayer},
    settings::Settings,
};

use super::{
    packet::{ConnectionType, Content, Header, Packet, HEADER_SIZE},
    players::Player,
};
use anyhow::anyhow;
use anyhow::Result;
use bytes::Bytes;
use futures::{future::join_all, Future};
use std::collections::HashMap;
use tokio::{
    io::{split, AsyncReadExt, ReadHalf},
    net::TcpStream,
    sync::RwLock,
};
use tracing::{debug, info};
use uuid::Uuid;

const MAX_PLAYER: i16 = 10;

pub struct Server {
    peers: RwLock<HashMap<Uuid, Peer>>,
    players: Players,
    settings: Settings,
}

impl Server {
    pub fn new(settings: Settings) -> Self {
        Self {
            peers: RwLock::default(),
            players: Players::new(),
            settings,
        }
    }

    async fn broadcast(&self, packet: Packet) {
        let peers = self.peers.read().await;

        join_all(
            peers
                .iter()
                .filter(|(_, p)| p.connected && p.id != packet.id)
                .map(|(_, p)| p.send(packet.clone())),
        )
        .await;
    }

    async fn broadcast_map<F, Fut>(&self, packet: Packet, map: F)
    where
        F: Fn(SharedPlayer, Packet) -> Fut,
        Fut: Future<Output = Packet>,
    {
        let peers = self.peers.read().await;

        join_all(
            peers
                .iter()
                .filter(|(_, p)| p.connected && p.id != packet.id)
                .map(|(_, peer)| async {
                    let packet = match self.players.get(&packet.id).await {
                        Some(p) => (map)(p, packet.clone()).await,
                        None => packet.clone(),
                    };

                    peer.send(packet).await;
                }),
        )
        .await;
    }

    pub async fn handle_connection(&self, socket: TcpStream) -> Result<()> {
        let ip = socket.peer_addr()?;
        let (mut reader, writer) = split(socket);

        let mut peer = Peer::new(ip, writer);
        let id = peer.id.clone();

        peer.send(Packet::new(
            peer.id,
            Content::Init {
                max_player: MAX_PLAYER,
            },
        ))
        .await;

        let packet = receive_packet(&mut reader).await?;

        if !packet.content.is_connect() {
            debug!(
                "Player {} didn't send connection packet on first connection",
                packet.id
            );
            return Err(anyhow!("Didn't receive connection packet as first packet"));
        }

        let peers = self.peers.read().await;

        let connected_peers = peers
            .iter()
            .fold(0, |acc, p| if p.1.connected { acc + 1 } else { 0 });

        if connected_peers == MAX_PLAYER {
            info!("Player {} couldn't join server is full", packet.id);
            return Err(anyhow!("Server full"));
        }

        drop(peers);

        let mut peers = self.peers.write().await;

        // Remove stales clients and only keep the disconnected one
        let _ = peers.remove(&packet.id);

        match (packet.content, self.players.get(&packet.id).await) {
            // Player already exist so reconnecting
            (_, Some(_)) => {
                debug!("Client {} attempting to reconnect", id);

                peer.id = packet.id;
                peers.insert(packet.id, peer);
            }
            // Player doesn't exist so we create it
            (
                Content::Connect {
                    type_: _,
                    max_player: _,
                    client,
                },
                None,
            ) => {
                debug!("Client {} with id {} is joining", client, packet.id);
                peer.id = packet.id;

                let player = Player::new(packet.id, client);

                let _ = self.players.add(player).await;

                let peer = self.on_new_peer(peer).await?;

                peers.insert(packet.id, peer);
            }
            _ => {
                debug!("This case isn't supposed to be reach");
                return Err(anyhow!("This case isn't supposed to be reach"));
            }
        }

        let peers = self.peers.read().await;

        let peer = peers
            .get(&id)
            .ok_or(anyhow!("Player is supposed to be in the HashMap"))?;

        for (uuid, peer) in self.peers.read().await.iter() {
            if *uuid == id {
                continue;
            }

            let player = self
                .players
                .get(uuid)
                .await
                .expect("Peers and Players are desynchronized");

            let player = player.read().await;

            let _ = peer
                .send(Packet::new(
                    player.id,
                    Content::Connect {
                        type_: ConnectionType::First,
                        max_player: MAX_PLAYER as u16,
                        client: player.name.clone(),
                    },
                ))
                .await;

            if let Some(costume) = &player.costume {
                let _ = peer
                    .send(Packet::new(
                        player.id,
                        Content::Costume {
                            body: costume.body.clone(),
                            cap: costume.cap.clone(),
                        },
                    ))
                    .await;
            }

            drop(player);
        }

        drop(peer);
        drop(peers);

        let player = self
            .players
            .get(&id)
            .await
            .expect("Player is supposed to be here");

        loop {
            let packet = receive_packet(&mut reader).await?;

            if packet.id != id {
                debug!("Id mismatch: received {} - expecting {}", packet.id, id);

                return Err(anyhow!(
                    "Id mismatch: received {} - expecting {}",
                    packet.id,
                    id
                ));
            }

            match &packet.content {
                Content::Costume { body, cap } => {
                    let mut player = player.write().await;

                    player.set_costume(body.clone(), cap.clone());
                    drop(player);
                }
                Content::Game {
                    is_2d,
                    scenario,
                    stage,
                } => {
                    let mut player = player.write().await;

                    player.scenario = Some(*scenario);
                    player.is_2d = *is_2d;
                    player.last_game_packet = Some(packet.clone());

                    if stage == "CapWorldHomeStage" && *scenario == 0 {
                        player.is_speedrun = true;
                        player.shine_sync = vec![];
                        player.persist_shines().await;
                        info!("Entered Cap on new save, preventing moon sync until Cascade");
                    } else if stage == "WaterfallWorldHomeStage" {
                        let was_speedrun = player.is_speedrun;
                        player.is_speedrun = false;

                        if was_speedrun {
                            // TODO:
                            // Task.Run(async () => {
                            //     c.Logger.Info("Entered Cascade with moon sync disabled, enabling moon sync");
                            //     await Task.Delay(15000);
                            //     await ClientSyncShineBag(c);
                            // });
                        }
                    }

                    if self.settings.is_merge_enabled {
                        self.broadcast_map(packet.clone(), |player, packet| async move {
                            match packet.content {
                                Content::Game {
                                    is_2d,
                                    scenario: _,
                                    stage,
                                } => {
                                    let player = player.read().await;

                                    let scenario = player.scenario.unwrap_or(200);
                                    Packet::new(
                                        packet.id,
                                        Content::Game {
                                            is_2d,
                                            scenario,
                                            stage,
                                        },
                                    )
                                }
                                _ => packet,
                            }
                        })
                        .await;
                    }
                }
                Content::Tag {
                    update_type,
                    is_it,
                    seconds,
                    minutes,
                } => (),
                Content::Disconnect => break,
                _ => (),
            }

            self.broadcast(packet).await;
        }

        // TODO: Find out when peers & players are cleaned
        let mut peers = self.peers.write().await;
        let mut peer = peers.get_mut(&id).expect("Peer is supposed to be here");

        peer.connected = false;
        peer.disconnect().await;

        Ok(())
    }

    async fn on_new_peer(&self, peer: Peer) -> Result<Peer> {
        let is_ip_banned = self
            .settings
            .ban_list
            .ips
            .iter()
            .find(|addr| **addr == peer.ip)
            .is_some();

        let is_id_banned = self
            .settings
            .ban_list
            .ids
            .iter()
            .find(|addr| **addr == peer.id)
            .is_some();

        if is_id_banned || is_ip_banned {
            info!(
                "Banned player {} with ip {} tried to joined",
                peer.ip, peer.id
            );

            Err(anyhow!(
                "Banned player {} with ip {} tried to joined",
                peer.ip,
                peer.id
            ))
        } else {
            let packets = self.players.get_last_game_packets().await;

            for packet in packets {
                peer.send(packet).await;
            }

            Ok(peer)
        }
    }
}

async fn receive_packet(reader: &mut ReadHalf<TcpStream>) -> Result<Packet> {
    let mut header_buf = [0; HEADER_SIZE];

    match reader.read_exact(&mut header_buf).await {
        Ok(n) if n == 0 => return Ok(Packet::new(Uuid::nil(), Content::Disconnect)),
        Ok(_) => (),
        Err(e) => {
            debug!("Error reading header {}", e);
            return Err(anyhow!(e));
        }
    };

    let header = match Header::from_bytes(Bytes::from(header_buf.to_vec())) {
        Ok(h) => h,
        Err(e) => {
            return Err(e);
        }
    };

    let body = if header.packet_size > 0 {
        let mut body_buf = vec![0; header.packet_size];

        match reader.read_exact(&mut body_buf).await {
            Ok(n) if n == 0 => return Err(anyhow!("End of file reached")),
            Ok(_) => (),
            Err(e) => {
                debug!("Error reading header {}", e);
                return Err(anyhow!(e));
            }
        };

        Bytes::from(body_buf)
    } else {
        Bytes::new()
    };

    Ok(header.make_packet(body)?)
}
