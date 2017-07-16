// Copyright (c) 2017 Chef Software Inc. and/or applicable contributors
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

use std::marker::PhantomData;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use protobuf::{self, parse_from_bytes, Message};
use protocol;
use zmq;

use super::ApplicationState;
use config::{DispatcherCfg, RouterCfg};
use error::{Error, Result};
use routing::{RouteBroker, RouteConn};
use socket::DEFAULT_CONTEXT;

const MAX_HOPS: usize = 8;

/// Dispatchers connect to Message Queue Servers
pub trait Dispatcher: Sized + Send + 'static {
    type Config: DispatcherCfg + RouterCfg;
    type State: ApplicationState;

    fn dispatch<'a>(request: &'a mut Request, conn: &mut RouteConn, state: &mut Self::State);

    /// Callback to perform dispatcher initialization.
    ///
    /// The default implementation will take your initial state and convert it into the actual
    /// state of the worker. Override this function if you need to perform additional steps to
    /// initialize your worker state.
    #[allow(unused_mut)]
    fn init(mut state: Self::State) -> Self::State {
        state
    }
}

pub struct DispatcherPool<T: Dispatcher> {
    state: T::State,
    queue: Arc<String>,
    worker_count: usize,
    workers: Vec<mpsc::Receiver<()>>,
    marker: PhantomData<T>,
}

impl<T> DispatcherPool<T>
where
    T: Dispatcher,
{
    pub fn new<C>(queue: Arc<String>, config: &C, state: T::State) -> Self
    where
        C: DispatcherCfg,
    {
        DispatcherPool {
            state: state,
            queue: queue,
            worker_count: config.worker_count(),
            workers: Vec::with_capacity(config.worker_count()),
            marker: PhantomData,
        }
    }

    /// Start a pool of message dispatchers.
    pub fn run(mut self) {
        for worker_id in 0..self.worker_count {
            self.spawn_dispatcher(worker_id);
        }
        thread::spawn(move || {
            loop {
                for i in 0..self.worker_count {
                    match self.workers[i].try_recv() {
                        Err(mpsc::TryRecvError::Disconnected) => {
                            info!("Worker[{}] restarting...", i);
                            self.spawn_dispatcher(i);
                        }
                        Ok(msg) => warn!("Worker[{}] sent unexpected msg: {:?}", i, msg),
                        Err(mpsc::TryRecvError::Empty) => continue,
                    }
                }
                // JW TODO: switching to zmq from channels will allow us to call select across
                // multiple queues and avoid sleeping
                thread::sleep(Duration::from_millis(500));
            }
        });
    }

    fn spawn_dispatcher(&mut self, worker_id: usize) {
        let (tx, rx) = mpsc::sync_channel(1);
        let state = T::init(self.state.clone());
        let queue = self.queue.clone();
        thread::spawn(move || worker_run::<T>(tx, queue, state));
        if rx.recv().is_ok() {
            debug!("Worker[{}] ready", worker_id);
            self.workers.insert(worker_id, rx);
        } else {
            error!("Worker[{}] failed to start", worker_id);
            self.workers.remove(worker_id);
        }
    }
}

pub struct Request<'a> {
    header_written: bool,
    hops: Vec<zmq::Message>,
    msg: Option<protocol::net::Msg>,
    socket: &'a mut zmq::Socket,
}

impl<'a> Request<'a> {
    pub fn new(socket: &'a mut zmq::Socket) -> Self {
        Request {
            header_written: false,
            hops: Vec::with_capacity(MAX_HOPS),
            msg: None,
            socket: socket,
        }
    }

    pub fn add_hop(&mut self, hop: zmq::Message) -> Result<()> {
        if self.max_hops() {
            return Err(Error::MaxHops);
        }
        self.hops.push(hop);
        Ok(())
    }

    pub fn message_id(&self) -> &str {
        self.msg.as_ref().unwrap().get_message_id()
    }

    pub fn parse_msg<M>(&self) -> Result<M>
    where
        M: protobuf::MessageStatic,
    {
        let msg = parse_from_bytes::<M>(self.msg.as_ref().unwrap().get_body())?;
        Ok(msg)
    }

    pub fn reply<M>(&mut self, msg: &M) -> Result<()>
    where
        M: protobuf::Message,
    {
        self.send_header()?;
        let rep = protocol::Message::new(msg).build();
        self.socket.send(
            &rep.write_to_bytes().unwrap(),
            zmq::SNDMORE,
        )?;
        Ok(())
    }

    pub fn reply_complete<M>(&mut self, msg: &M) -> Result<()>
    where
        M: protobuf::Message,
    {
        self.send_header()?;
        let rep = protocol::Message::new(msg).build();
        let bytes = rep.write_to_bytes()?;
        self.socket.send(&bytes, 0)?;
        Ok(())
    }

    fn max_hops(&self) -> bool {
        self.hops.len() >= MAX_HOPS
    }

    fn reset(&mut self) {
        self.header_written = false;
        self.hops.clear();
        self.msg = None;
    }

    fn send_header(&mut self) -> Result<()> {
        if self.header_written {
            return Ok(());
        }
        for hop in self.hops.iter() {
            self.socket.send(hop, zmq::SNDMORE)?;
        }
        self.socket.send(&[], zmq::SNDMORE)?;
        self.socket.send_str("RP", zmq::SNDMORE)?;
        self.header_written = true;
        Ok(())
    }
}

fn worker_run<T>(rz: mpsc::SyncSender<()>, queue: Arc<String>, mut state: T::State)
where
    T: Dispatcher,
{
    debug_assert!(
        state.is_initialized(),
        "Dispatcher state not initialized! wrongfully \
        implements the `init()` callback or omits an override implementation where the default \
        implementation isn't enough to initialize the dispatcher's state?"
    );
    let mut raw = zmq::Message::new().unwrap();
    let mut socket = (**DEFAULT_CONTEXT).as_mut().socket(zmq::DEALER).unwrap();
    socket.connect(&*queue).unwrap();
    let mut route_conn = RouteBroker::connect().unwrap();
    let mut request = Request::new(&mut socket);
    rz.send(()).unwrap();
    'recv: loop {
        request.reset();
        'hops: loop {
            let hop = request.socket.recv_msg(0).unwrap();
            if hop.len() == 0 {
                break;
            }
            if request.add_hop(hop).is_err() {
                warn!("drop message, too many hops");
                break 'recv;
            }
        }
        request.socket.recv(&mut raw, 0).unwrap();
        match parse_from_bytes(&raw) {
            Ok(msg) => {
                debug!("OnMessage, {:?}", &msg);
                request.msg = Some(msg);
                T::dispatch(&mut request, &mut route_conn, &mut state);
            }
            Err(e) => warn!("OnMessage bad message, {}", e),
        }
    }
}
