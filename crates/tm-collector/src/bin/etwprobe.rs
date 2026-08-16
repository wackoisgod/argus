//! Diagnostic: subscribe to Microsoft-Windows-Kernel-Disk, generate disk
//! I/O, and dump raw event ids + field parse results. Run elevated.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::UserTrace;
use ferrisetw::EventRecord;

static COUNT: AtomicU64 = AtomicU64::new(0);

fn main() {
    let _ = ferrisetw::trace::stop_trace_by_name("tm-etwprobe");
    let samples = Arc::new(Mutex::new(Vec::<String>::new()));
    let s2 = Arc::clone(&samples);
    let disk = Provider::by_guid("c7bde69a-e1e0-4177-b6ef-283ad1525271")
        .any(u64::MAX)
        .add_callback(move |record: &EventRecord, sl: &SchemaLocator| {
            let n = COUNT.fetch_add(1, Ordering::Relaxed);
            if n < 12 {
                let mut line = format!(
                    "event_id={} header_pid={}",
                    record.event_id(),
                    record.process_id()
                );
                match sl.event_schema(record) {
                    Ok(schema) => {
                        let parser = Parser::create(record, &schema);
                        for name in ["IssuingProcessId", "IssuingThreadId", "TransferSize"] {
                            if let Ok(v) = parser.try_parse::<u32>(name) {
                                line.push_str(&format!(" {name}(u32)={v}"));
                            } else if let Ok(v) = parser.try_parse::<u64>(name) {
                                line.push_str(&format!(" {name}(u64)={v}"));
                            } else if let Ok(v) = parser.try_parse::<i64>(name) {
                                line.push_str(&format!(" {name}(i64)={v}"));
                            } else if let Ok(v) = parser.try_parse::<String>(name) {
                                line.push_str(&format!(" {name}(str)={v}"));
                            } else {
                                line.push_str(&format!(" {name}=ERR"));
                            }
                        }
                    }
                    Err(_) => line.push_str(" schema=ERR"),
                }
                s2.lock().unwrap().push(line);
            }
        })
        .build();

    let trace = UserTrace::new()
        .named("tm-etwprobe".to_string())
        .enable(disk)
        .start_and_process()
        .expect("start trace (elevated?)");

    {
        use std::io::Write;
        let path = std::env::temp_dir().join("etwprobe.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        let buf = vec![7u8; 4 * 1024 * 1024];
        for _ in 0..25 {
            f.write_all(&buf).unwrap();
        }
        f.sync_all().unwrap();
        drop(f);
        let _ = std::fs::remove_file(&path);
    }
    std::thread::sleep(std::time::Duration::from_secs(3));

    println!("total kernel-disk events: {}", COUNT.load(Ordering::Relaxed));
    for line in samples.lock().unwrap().iter() {
        println!("{line}");
    }
    trace.stop().unwrap();
}
