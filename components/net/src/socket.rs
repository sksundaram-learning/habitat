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

use std::cell::UnsafeCell;

use zmq;

lazy_static! {
    /// A threadsafe shared ZMQ context for consuming services.
    ///
    /// You probably want to use this context to create new ZMQ sockets unless you *do not* want to
    /// connect them together using an in-proc queue.
    pub static ref DEFAULT_CONTEXT: Box<SocketContext> = {
        let ctx = SocketContext::new();
        Box::new(ctx)
    };
}

/// This is a wrapper to provide interior mutability of an underlying `zmq::Context` and allows
/// for sharing/sending of a `zmq::Context` between threads.
pub struct SocketContext(UnsafeCell<zmq::Context>);

impl SocketContext {
    pub fn new() -> Self {
        SocketContext(UnsafeCell::new(zmq::Context::new()))
    }

    pub fn as_mut(&self) -> &mut zmq::Context {
        unsafe { &mut *self.0.get() }
    }
}

unsafe impl Send for SocketContext {}
unsafe impl Sync for SocketContext {}
