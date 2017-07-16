// Copyright (c) 2016 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;

use hab_net::server::{Application, Envelope};
use hab_net::socket::DEFAULT_CONTEXT;
use protobuf::{parse_from_bytes, Message};
use protocol::{self, routesrv};
use protocol::sharding::{ShardId, SHARD_COUNT};
use protocol::net::{ErrCode, Protocol};
use rand::{self, Rng};
use zmq::{self, Error as ZError};

use config::Config;
use error::{Error, Result};

pub struct Server {
    config: Config,
    socket: zmq::Socket,
    servers: ServerMap,
    state: SocketState,
    envelope: Envelope,
    req: zmq::Message,
    rng: rand::ThreadRng,
}

impl Server {
    pub fn new(config: Config) -> Self {
        let socket = (**DEFAULT_CONTEXT).as_mut().socket(zmq::ROUTER).unwrap();
        socket.set_router_mandatory(true).unwrap();
        Server {
            config: config,
            socket: socket,
            servers: ServerMap::default(),
            state: SocketState::default(),
            envelope: Envelope::default(),
            req: zmq::Message::new().unwrap(),
            rng: rand::thread_rng(),
        }
    }

    fn process_frontend(&mut self) -> Result<()> {
        loop {
            match self.state {
                SocketState::Ready => {
                    if self.envelope.max_hops() {
                        // We should force the sender to disconnect, they have a problem.
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    let hop = self.socket.recv_msg(0)?;
                    if self.envelope.hops().len() == 0 && hop.len() == 0 {
                        warn!("rejecting message, failed to receive identity frame from message");
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    debug!("received hop, {:?}/{:?}", hop.as_str(), hop.len());
                    self.envelope.add_hop(hop).unwrap();
                    self.state = SocketState::Hops;
                }
                SocketState::Hops => {
                    if self.envelope.max_hops() {
                        // We should force the sender to disconnect, they have a problem.
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    let hop = self.socket.recv_msg(0)?;
                    if hop.len() == 0 {
                        self.state = SocketState::Control;
                        continue;
                    }
                    debug!("received hop, {:?}/{:?}", hop.as_str(), hop.len());
                    self.envelope.add_hop(hop).unwrap();
                }
                SocketState::Control => {
                    self.socket.recv(&mut self.req, 0)?;
                    match self.req.as_str() {
                        Some("RP") => self.state = SocketState::Forwarding,
                        Some("RQ") => self.state = SocketState::Routing,
                        _ => {
                            warn!("framing error");
                            self.state = SocketState::Cleaning;
                        }
                    }
                }
                SocketState::Forwarding => {
                    self.socket.recv(&mut self.req, 0)?;
                    debug!("forwarding, msg={:?}", self.req.as_str());
                    for hop in &self.envelope.hops()[1..] {
                        self.socket.send(&*hop, zmq::SNDMORE).unwrap();
                    }
                    self.socket.send(&[], zmq::SNDMORE).unwrap();
                    self.socket.send(&*self.req, 0).unwrap();
                    self.state = SocketState::Cleaning;
                }
                SocketState::Routing => {
                    self.socket.recv(&mut self.req, 0)?;
                    if self.req.len() == 0 {
                        warn!("rejecting message, failed to receive a message body");
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    debug!("received req, {:?}/{:?}", self.req.as_str(), self.req.len());
                    match parse_from_bytes(&self.req) {
                        Ok(msg) => self.envelope.msg = msg,
                        Err(e) => {
                            println!("failed to parse message, err={:?}", e);
                            self.state = SocketState::Cleaning;
                            continue;
                        }
                    }
                    if !self.envelope.msg.has_route_info() {
                        warn!(
                            "received message without route-info, msg={:?}",
                            self.envelope.msg
                        );
                        self.state = SocketState::Cleaning;
                        continue;
                    }
                    match self.envelope.msg.get_route_info().get_protocol() {
                        Protocol::RouteSrv => self.handle_message()?,
                        _ => self.route_message()?,
                    }
                    self.state = SocketState::Cleaning;
                }
                SocketState::Cleaning => {
                    debug!("cleaning socket state");
                    self.reset();
                    self.state = SocketState::Ready;
                    break;
                }
            }
        }
        Ok(())
    }

    fn process_heartbeat(&mut self) -> Result<()> {
        self.socket.recv(&mut self.req, 0)?;
        let cmd = self.req.as_str().unwrap_or("").to_string();
        match cmd.as_ref() {
            "" => return Ok(()),
            "R" => {
                // Registration
                self.socket.recv(&mut self.req, 0)?;
                let mut reg: routesrv::Registration = parse_from_bytes(&self.req)?;
                debug!("received server reg, {:?}", reg);
                if self.servers.add(
                    reg.get_protocol(),
                    reg.take_endpoint(),
                    reg.take_shards(),
                )
                {
                    self.socket.send_str("REGOK", 0)?;
                } else {
                    self.socket.send_str("REGCONFLICT", 0)?;
                }
            }
            "P" => {
                // Pulse
                self.socket.recv(&mut self.req, 0)?;
                // JW TODO: Don't unwrap this
                self.servers.renew(self.req.as_str().unwrap());
            }
            ident => {
                // New connection
                self.socket.send_str(ident, zmq::SNDMORE)?;
                self.socket.send(&[], zmq::SNDMORE)?;
                self.socket.send_str("REG", 0)?;
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.envelope.reset();
    }

    fn handle_message(&mut self) -> Result<()> {
        let msg = &self.envelope.msg;
        trace!("handle-message, msg={:?}", &msg);
        match self.envelope.message_id() {
            "Connect" => {
                let req: routesrv::Connect = parse_from_bytes(msg.get_body()).unwrap();
                trace!("Connect={:?}", req);
                let rep = protocol::Message::new(&routesrv::ConnectOk::new()).build();
                self.socket.send(&rep.write_to_bytes().unwrap(), 0).unwrap();
            }
            "Disconnect" => {
                let req: routesrv::Disconnect = parse_from_bytes(msg.get_body()).unwrap();
                trace!("Disconnect={:?}", req);
                self.servers.drop(
                    &self.envelope.protocol(),
                    req.get_endpoint(),
                );
            }
            id => warn!("Unknown message, msg={}", id),
        }
        Ok(())
    }

    fn route_message(&mut self) -> Result<()> {
        let shard = self.select_shard();
        match self.servers.get(&self.envelope.protocol(), &shard) {
            Some(server) => {
                debug!(
                    "routing, srv={:?}, hops={:?}, msg={:?}",
                    server.endpoint,
                    self.envelope.hops().len(),
                    self.envelope.msg
                );
                self.socket.send_str(&server.endpoint, zmq::SNDMORE)?;
                for hop in self.envelope.hops() {
                    self.socket.send(&*hop, zmq::SNDMORE)?;
                }
                self.socket.send(&[], zmq::SNDMORE)?;
                self.socket.send(
                    &self.envelope.msg.write_to_bytes().unwrap(),
                    0,
                )?;
            }
            None => {
                warn!(
                    "failed to route message, no server servicing shard, msg={:?}",
                    self.envelope.msg
                );
                let err = protocol::Message::new(
                    &protocol::net::err(ErrCode::NO_SHARD, "rt:route:1"),
                ).build();
                let bytes = err.write_to_bytes()?;
                for hop in self.envelope.hops() {
                    self.socket.send(&*hop, zmq::SNDMORE)?;
                }
                self.socket.send(&[], zmq::SNDMORE)?;
                self.socket.send(&bytes, 0)?;
            }
        }
        Ok(())
    }

    fn select_shard(&mut self) -> u32 {
        if self.envelope.route_info().has_hash() {
            (self.envelope.route_info().get_hash() % SHARD_COUNT as u64) as u32
        } else {
            (self.rng.gen::<u64>() % SHARD_COUNT as u64) as u32
        }
    }

    fn wait_recv(&self) -> RecvResult {
        // JW TODO: figure out how long we should poll for
        match self.socket.poll(zmq::POLLIN, -1) {
            Ok(count) if count < 0 => unreachable!("zmq::poll, returned a negative count"),
            Ok(count) => RecvResult::Continue(count as u32),
            Err(ZError::EINTR) |
            Err(ZError::ETERM) => RecvResult::Shutdown,
            Err(ZError::EFAULT) => {
                unreachable!("zmq::poll, the provided _items_ was not valid (NULL)")
            }
            Err(err) => unreachable!("zmq::poll, returned an unexpected error, {:?}", err),
        }
    }
}

impl Application for Server {
    type Error = Error;

    fn run(&mut self) -> Result<()> {
        self.socket.bind(&self.config.addr())?;
        println!("Listening on ({})", self.config.addr());
        info!("builder-router is ready to go.");
        loop {
            trace!("waiting for message");
            match self.wait_recv() {
                RecvResult::Continue(count) => {
                    trace!("processing {}, messages", count);
                    trace!("processing front-end");
                    self.process_frontend()?;
                }
                RecvResult::Shutdown => {
                    info!("received shutdown signal, shutting down...");
                    break;
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct ServerMap(HashMap<Protocol, HashMap<ShardId, hab_net::ServerReg>>);

impl ServerMap {
    pub fn add(&mut self, protocol: Protocol, netid: String, shards: Vec<ShardId>) -> bool {
        if !self.0.contains_key(&protocol) {
            self.0.insert(protocol, HashMap::default());
        }
        let registrations = self.0.get_mut(&protocol).unwrap();
        for shard in shards.iter() {
            if let Some(reg) = registrations.get(&shard) {
                if &reg.endpoint != &netid {
                    return false;
                }
            }
        }
        let registration = hab_net::ServerReg::new(netid);
        for shard in shards {
            registrations.insert(shard, registration.clone());
        }
        true
    }

    pub fn drop(&mut self, protocol: &Protocol, netid: &str) {
        if let Some(map) = self.0.get_mut(protocol) {
            map.retain(|_, reg| reg.endpoint != netid);
        }
    }

    pub fn get(&self, protocol: &Protocol, shard: &ShardId) -> Option<&hab_net::ServerReg> {
        self.0.get(protocol).and_then(|shards| shards.get(shard))
    }

    pub fn renew(&mut self, netid: &str) {
        // JW TODO: We can't iterate like this every heartbeat
        for registrations in self.0.values_mut() {
            for registration in registrations.values_mut() {
                if registration.endpoint == netid {
                    registration.renew();
                }
            }
        }
    }
}

enum SocketState {
    Ready,
    Hops,
    Control,
    Routing,
    Forwarding,
    Cleaning,
}

impl Default for SocketState {
    fn default() -> SocketState {
        SocketState::Ready
    }
}

pub fn run(config: Config) -> Result<()> {
    Server::new(config).run()
}

enum RecvResult {
    Continue(u32),
    Shutdown,
}
