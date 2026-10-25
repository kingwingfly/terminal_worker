//! The worker that highlights [`Group::Web`](terminal_worker_preview::language::Group)'s languages.

fn main() {
    terminal_worker_preview::worker::serve_code(terminal_worker_preview::code::web());
}
