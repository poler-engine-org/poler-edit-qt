pub mod buffer;
pub mod protocol;

fn main() {
    let code = protocol::run_edit_server();
    std::process::exit(code);
}
