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

pub mod prelude;
mod dispatcher;

use std::sync::Arc;

use core::os::process;
use zmq::{self, Error as ZError};

pub use self::dispatcher::{Dispatcher, Request};
use self::dispatcher::DispatcherPool;
use config::{RouterCfg, ToAddrString};
use error::{Error, Result};
use socket::DEFAULT_CONTEXT;

enum RecvResult {
    OnMessage(u32),
    Shutdown,
    Timeout,
}

/// Apply to a struct containing worker state that will be passed as a mutable reference on each
/// call of `dispatch()` to an implementer of `Dispatcher`.
pub trait ApplicationState: Clone + Send {
    fn is_initialized(&self) -> bool;
}

pub struct Application<T: Dispatcher> {
    config: T::Config,
    dispatcher: zmq::Socket,
    dispatcher_queue: Arc<String>,
    msg_buf: zmq::Message,
    router: zmq::Socket,
}

impl<T> Application<T>
where
    T: Dispatcher,
{
    fn new(config: T::Config) -> Result<Self> {
        let net_ident = net_ident();
        let router = (**DEFAULT_CONTEXT).as_mut().socket(zmq::DEALER)?;
        router.set_identity(net_ident.as_bytes())?;
        router.set_probe_router(true)?;
        let dispatcher = (**DEFAULT_CONTEXT).as_mut().socket(zmq::DEALER).unwrap();
        Ok(Application {
            config: config,
            dispatcher: dispatcher,
            dispatcher_queue: Arc::new(format!("inproc://{}.net.dispatch", net_ident)),
            msg_buf: zmq::Message::new()?,
            router: router,
        })
    }

    fn forward_messages(&mut self) -> Result<()> {
        loop {
            match self.router.recv(&mut self.msg_buf, zmq::DONTWAIT) {
                Ok(()) => {
                    let flags = if self.msg_buf.get_more() { zmq::SNDMORE } else { 0 };
                    self.dispatcher.send(&*self.msg_buf, flags)?;
                }
                Err(ZError::EAGAIN) => break,
                Err(err) => return Err(Error::Zmq(err)),
            }
        }
        loop {
            match self.dispatcher.recv(&mut self.msg_buf, zmq::DONTWAIT) {
                Ok(()) => {
                    let flags = if self.msg_buf.get_more() { zmq::SNDMORE } else { 0 };
                    self.router.send(&*self.msg_buf, flags)?;
                }
                Err(ZError::EAGAIN) => break,
                Err(err) => return Err(Error::Zmq(err)),
            }
        }
        Ok(())
    }

    fn run(mut self, state: T::State) -> Result<()> {
        for addr in self.config.route_addrs() {
            self.router.connect(&addr.to_addr_string())?;
        }
        self.dispatcher.bind(&*self.dispatcher_queue)?;
        // JW TODO: I shouldn't need to start a RouteBroker here, but I currently do because
        // it isn't using the right inproc queue. I should probably share the inproc queue that
        // the dispatcher is bound to.
        DispatcherPool::<T>::new(self.dispatcher_queue.clone(), &self.config, state).run();
        info!("application is ready to go.");
        loop {
            trace!("waiting for message");
            match self.wait_recv() {
                RecvResult::OnMessage(count) => {
                    trace!("processing '{}' messages", count);
                    self.forward_messages()?;
                }
                RecvResult::Timeout => {
                    self.router.send_str("HB", 0)?;
                }
                RecvResult::Shutdown => {
                    info!("received shutdown signal, shutting down...");
                    break;
                }
            }
        }
        Ok(())
    }

    fn wait_recv(&mut self) -> RecvResult {
        match zmq::poll(
            &mut [
                self.router.as_poll_item(zmq::POLLIN),
                self.dispatcher.as_poll_item(zmq::POLLIN),
            ],
            30_000,
        ) {
            Ok(count) if count < 0 => unreachable!("zmq::poll, returned with a negative count"),
            Ok(count) if count == 0 => {
                println!("JW TODO: CASE 1");
                RecvResult::Timeout
            }
            Ok(count) => RecvResult::OnMessage(count as u32),
            Err(ZError::EAGAIN) => {
                println!("JW TODO: CASE 2");
                RecvResult::Timeout
            }
            Err(ZError::EINTR) |
            Err(ZError::ETERM) => RecvResult::Shutdown,
            Err(ZError::EFAULT) => panic!("zmq::poll, the provided _items_ was not valid (NULL)"),
            Err(err) => unreachable!("zmq::poll, returned an unexpected error, {:?}", err),
        }
    }
}

pub fn start<T>(cfg: T::Config, state: T::State) -> Result<()>
where
    T: Dispatcher,
{
    let app = Application::<T>::new(cfg)?;
    app.run(state)
}

fn net_ident() -> String {
    let hostname = super::hostname().unwrap();
    let pid = process::current_pid();
    format!("{}@{}", pid, hostname)
}
