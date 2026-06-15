//! Service entrypoint.

mod auth;
mod db;
mod handlers;
mod util;

use db::connect_db;
use handlers::handle_request;

fn main() {
    let conn = connect_db("postgres://localhost/app");
    let result = handle_request(&conn, "token-abc", "user:1001");
    match result {
        Ok(body) => println!("ok: {}", body),
        Err(e) => eprintln!("error: {}", e),
    }
}
