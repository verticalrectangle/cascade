use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let path = PathBuf::from("/home/alexis/.omp/agent/sessions/-dev-cascade/2026-08-21T17-16-11-256Z_01a02552-b078-7000-a8ac-1825062cc3e9.jsonl");
    let mut offset = std::fs::metadata(&path).unwrap().len();
    println!("watching from offset {offset}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let size = std::fs::metadata(&path).unwrap().len();
        if size != offset {
            let n = (size - offset).min(400) as usize;
            let mut f = std::fs::File::open(&path).unwrap();
            use std::io::{Read, Seek, SeekFrom};
            f.seek(SeekFrom::Start(offset)).unwrap();
            let mut head = vec![0u8; n];
            f.read_exact(&mut head).unwrap();
            println!("GREW +{} bytes; head of append: {:?}", size - offset, String::from_utf8_lossy(&head));
            offset = size;
        }
    }
    println!("done");
}
