//! Minimal writer/reader roundtrip.
//!
//! Run with: `cargo run --example basic`

use walworm::{Error, Reader, ReaderOptions, Writer, WriterOptions};

fn main() -> walworm::Result<()> {
    let path = std::env::temp_dir().join(format!("walworm-example-{}.wal", std::process::id()));

    let mut writer = Writer::create(&path, WriterOptions::default())?;
    writer.write(b"hello")?;
    writer.write_batch([b"these".as_slice(), b"are".as_slice(), b"atomic".as_slice()])?;
    writer.flush()?;
    writer.close()?;

    let mut reader = Reader::open(&path, ReaderOptions::default())?;
    loop {
        match reader.next() {
            Some(Ok(record)) => println!("record: {:?}", String::from_utf8_lossy(&record)),
            Some(Err(Error::Incomplete)) => break,
            Some(Err(e)) => return Err(e),
            None => {
                println!("log closed by writer");
                break;
            }
        }
    }

    std::fs::remove_file(&path)?;
    Ok(())
}
