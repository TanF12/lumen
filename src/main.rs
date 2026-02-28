mod cli;
mod config;
mod http;
mod server;
mod state;
mod thread_pool;
mod utils;

fn main() {
    // hand over control to the CLI parser immediately
    cli::execute();
}
