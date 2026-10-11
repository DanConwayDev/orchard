//! Dev probe: send one REQ and print every frame for a few seconds.
//! Not a shipped tool; exists to see exactly what a relay serves.
//!
//!     probe 'ws://127.0.0.1:8008' '{"kinds":[30066]}'

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (url, filter) = match args.len() {
        3 => (args[1].clone(), args[2].clone()),
        _ => {
            eprintln!("usage: probe <relay-url> <filter-json>");
            std::process::exit(1);
        }
    };
    let (mut socket, _response) = tungstenite::connect(url).expect("connect");
    socket
        .send(tungstenite::Message::text(
            serde_json::json!(["REQ", "probe", serde_json::from_str::<serde_json::Value>(&filter).expect("filter json")])
                .to_string(),
        ))
        .expect("send");
    let reader = std::thread::spawn(move || loop {
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                println!("{text}");
                std::io::stdout().flush().ok();
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("read: {err}");
                break;
            }
        }
    });
    std::thread::sleep(std::time::Duration::from_secs(4));
    drop(reader);
    std::process::exit(0);
}
