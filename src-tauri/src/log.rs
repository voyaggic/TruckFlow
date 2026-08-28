use std::fs::OpenOptions;
use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::time::SystemTime;

/// The sender half of the log channel. OnceLock ensures it's initialized
/// exactly once, even under concurrent access. The receiver is consumed
/// by the background writer thread.
static LOG_TX: OnceLock<Sender<String>> = OnceLock::new();

/// Initialize the async log system. Call once at app startup (in `run()`).
/// Spawns a background thread that drains the channel and writes to disk.
/// The thread batches writes for efficiency.
pub fn init() {
    let (tx, rx) = std::sync::mpsc::channel::<String>();

    let _ = LOG_TX.set(tx);

    std::thread::Builder::new()
        .name("log-writer".into())
        .spawn(move || {
            // Open the log file once, keep it open for the lifetime of the app.
            let dir = if let Ok(appdata) = std::env::var("APPDATA") {
                std::path::Path::new(&appdata).join("com.truckflow.app")
            } else if let Ok(home) = std::env::var("HOME") {
                std::path::Path::new(&home).join(".config").join("com.truckflow.app")
            } else {
                std::path::PathBuf::from(".")
            };
            let path = dir.join("truckflow.log");
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap_or_else(|_| std::fs::File::create(&path).expect("failed to create truckflow.log"));

            // Drain loop: wait for messages, batch them, write once.
            loop {
                // Block until at least one message arrives.
                match rx.recv() {
                    Ok(first) => {
                        let mut buf = first;
                        // Drain any additional queued messages without blocking.
                        while let Ok(msg) = rx.try_recv() {
                            buf.push_str(&msg);
                        }
                        let _ = file.write_all(buf.as_bytes());
                        // Flush periodically (after each batch) so the file
                        // is readable if the app crashes soon.
                        let _ = file.flush();
                    }
                    Err(_) => break, // channel closed, writer exits
                }
            }
        })
        .expect("failed to spawn log-writer thread");
}

/// Log a message — NON-BLOCKING, near-zero cost (~0µs).
/// Sends the message to a background thread via a lock-free channel.
/// The message is timestamped and formatted by the caller.
pub fn log(msg: &str) {
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| format!("{}", d.as_secs()))
        .unwrap_or_default();
    let line = format!("[{}] {}\n", timestamp, msg);
    if let Some(tx) = LOG_TX.get() {
        // send() on an unbounded channel never blocks and never waits.
        let _ = tx.send(line);
    }
}
