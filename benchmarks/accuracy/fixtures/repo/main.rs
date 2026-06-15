//! Entrypoint for the demo service.

mod auth;
mod crypto;
mod db;
mod handlers;

use handlers::handle_request;

fn main() {
    let out = handle_request("token-abc", "user:1001");
    println!("{:?}", out);
}
