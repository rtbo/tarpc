// Copyright 2018 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

use futures::{
    future::{self, Ready},
    prelude::*,
};
use std::io;
use tarpc::{
    client, context,
    server::{self, Channel},
};
use tokio_serde::formats::Json;

/// This is the service definition. It looks a lot like a trait definition.
/// It defines one RPC, hello, which takes one arg, name, and returns a String.
#[tarpc::service]
pub trait World {
    async fn hello(name: String) -> String;
}

/// This is the type that implements the generated World trait. It is the business logic
/// and is used to start the server.
#[derive(Clone)]
struct HelloServer;

impl World for HelloServer {
    // Each defined rpc generates two items in the trait, a fn that serves the RPC, and
    // an associated type representing the future output by the fn.

    type HelloFut = Ready<String>;

    fn hello(self, _: context::Context, name: String) -> Self::HelloFut {
        future::ready(format!("Hello, {}!", name))
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let (client_transport, server_transport) = tarpc::transport::channel::unbounded();

    let server = server::BaseChannel::with_defaults(server_transport);
    tokio::spawn(server.execute(HelloServer.serve()));

    // WorldClient is generated by the #[tarpc::service] attribute. It has a constructor `new`
    // that takes a config and any Transport as input.
    let mut client = WorldClient::new(client::Config::default(), client_transport).spawn()?;

    // The client has an RPC method for each RPC defined in the annotated trait. It takes the same
    // args as defined, with the addition of a Context, which is always the first arg. The Context
    // specifies a deadline and trace information which can be helpful in debugging requests.
    let hello = client.hello(context::current(), "Stim".to_string()).await?;

    println!("{}", hello);

    Ok(())
}
