use rusqlite::{params, Connection};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileA, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::System::Ioctl::{FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL};
use windows::Win32::System::IO::DeviceIoControl;

// --- RAW NTFS STRUCTURES ---
#[repr(C)]
#[derive(Debug, Default)]
struct UsnJournalData {
    usn_journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
}

#[repr(C)]
struct MftEnumData {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UsnRecordHeader {
    record_length: u32,
    major_version: u16,
    minor_version: u16,
    file_reference_number: u64,
    parent_file_reference_number: u64,
    usn: i64,
    timestamp: i64,
    reason: u32,
    source_info: u32,
    security_id: u32,
    file_attributes: u32,
    file_name_length: u16,
    file_name_offset: u16,
}

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x00000010;
const BATCH_SIZE: usize = 10_000; // Commit to DB every 10k files to save RAM

fn init_db() -> Connection {
    let conn = Connection::open("mft_index.db").expect("Could not create DB file");
    
    // Speed optimizations
    conn.execute("PRAGMA synchronous = OFF", []).unwrap();
    
    // FIX: Use query_row because this command returns a value ("wal")
    let _ : String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0)).unwrap();

    // Table for Folders
    conn.execute(
        "CREATE TABLE IF NOT EXISTS folders (
            id INTEGER PRIMARY KEY, 
            parent_id INTEGER, 
            name TEXT
        )",
        [],
    ).unwrap();

    // Table for Files
    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY, 
            parent_id INTEGER, 
            name TEXT
        )",
        [],
    ).unwrap();

    // Indices
    conn.execute("CREATE INDEX IF NOT EXISTS idx_folder_parent ON folders(parent_id)", []).unwrap();
    conn.execute("CREATE INDEX IF NOT EXISTS idx_file_name ON files(name)", []).unwrap();
    
    conn
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        println!("Usage:");
        println!("  cargo run --release -- scan       -> Index C: drive to mft_index.db");
        println!("  cargo run --release -- find <txt> -> Search DB and decode paths");
        return;
    }

    match args[1].as_str() {
        "scan" => scan_mft(),
        "find" => {
            if args.len() > 2 {
                find_file(&args[2]);
            } else {
                println!("Please provide a filename to find.");
            }
        },
        _ => println!("Unknown command"),
    }
}

fn scan_mft() {
    println!("[-] Accessing C: drive (Ensure Admin privileges)...");
    
    let mut conn = init_db();
    let volume_path = "\\\\.\\C:\0";
    
    let handle = unsafe {
        CreateFileA(
            windows::core::PCSTR(volume_path.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0, 
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE(0),
        )
    };

    if handle == Ok(INVALID_HANDLE_VALUE) || handle.is_err() {
        eprintln!("[!] Access Denied. Right-click terminal and 'Run as Administrator'.");
        return;
    }
    let handle = handle.unwrap();

    // 1. Get Journal Max USN
    let mut journal_data = UsnJournalData::default();
    let mut bytes_returned = 0u32;
    let success = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut journal_data as *mut _ as *mut c_void),
            size_of::<UsnJournalData>() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };

    if !success.as_bool() {
        eprintln!("[!] Failed to query USN Journal.");
        return;
    }

    println!("[-] Starting Indexing...");

    let mut med = MftEnumData {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: journal_data.max_usn,
    };

    let mut buffer = vec![0u8; 65536]; // 64KB Buffer
    let mut total_count = 0;
    let mut batch_count = 0;

    // Start the first transaction
    let mut tx = conn.transaction().unwrap();

    loop {
        let success = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_ENUM_USN_DATA,
                Some(&mut med as *mut _ as *mut c_void),
                size_of::<MftEnumData>() as u32,
                Some(buffer.as_mut_ptr() as *mut c_void),
                buffer.len() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        if !success.as_bool() { break; } // Done

        let mut offset = 8; // Skip USN at start
        while offset < bytes_returned as usize {
            let p_record = unsafe { &*(buffer.as_ptr().add(offset) as *const UsnRecordHeader) };
            let fname_len = p_record.file_name_length as usize;
            
            if fname_len > 0 {
                let name_slice = unsafe {
                    std::slice::from_raw_parts(
                        buffer.as_ptr().add(offset + p_record.file_name_offset as usize) as *const u16,
                        fname_len / 2,
                    )
                };
                
                // 'from_utf16_lossy' ensures no crashes on weird characters
                let name = String::from_utf16_lossy(name_slice);
                let is_dir = (p_record.file_attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;

                if is_dir {
                    tx.execute(
                        "INSERT OR IGNORE INTO folders (id, parent_id, name) VALUES (?, ?, ?)",
                        params![
                            p_record.file_reference_number as i64, 
                            p_record.parent_file_reference_number as i64, 
                            name
                        ],
                    ).unwrap();
                } else {
                    // Storing RAW IDs. Duplicates are fine (different rows)
                    tx.execute(
                        "INSERT OR IGNORE INTO files (id, parent_id, name) VALUES (?, ?, ?)",
                        params![
                            p_record.file_reference_number as i64, 
                            p_record.parent_file_reference_number as i64, 
                            name
                        ],
                    ).unwrap();
                }

                total_count += 1;
                batch_count += 1;
            }

            offset += p_record.record_length as usize;
        }

        // MEMORY MANAGEMENT: Commit and restart transaction periodically
        if batch_count >= BATCH_SIZE {
            tx.commit().unwrap();
            tx = conn.transaction().unwrap();
            print!("    Records: {}\r", total_count); // Update status
            batch_count = 0;
        }

        med.start_file_reference_number = unsafe { ptr::read(buffer.as_ptr() as *const u64) };
    }

    tx.commit().unwrap();
    unsafe { windows::Win32::Foundation::CloseHandle(handle) };
    println!("\n[-] Done. Total indexed: {}", total_count);
}

// --- DECODE FUNCTION ---
// This calculates the path on the fly using the raw IDs in the DB
fn find_file(filename: &str) {
    let conn = Connection::open("mft_index.db").unwrap();
    
    println!("[-] Searching for *{}*...", filename);
    
    // Search for name
    let mut stmt = conn.prepare("SELECT id, parent_id, name FROM files WHERE name LIKE ? LIMIT 20").unwrap();
    let rows = stmt.query_map(params![format!("%{}%", filename)], |row| {
        Ok((
            row.get::<_, i64>(0)?, // File ID
            row.get::<_, i64>(1)?, // Parent ID
            row.get::<_, String>(2)? // File Name
        ))
    }).unwrap();

    for row in rows {
        let (id, parent_id, name) = row.unwrap();
        // Pass the RAW parent ID to the decoder
        let full_path = decode_path_from_id(&conn, parent_id); 
        println!("Found: {}\\{} (ID: {})", full_path, name, id);
    }
}

// This is the recursive function you asked for
// It takes a plain number (Parent ID) and looks up the chain
fn decode_path_from_id(conn: &Connection, mut current_id: i64) -> String {
    let mut parts = Vec::new();
    let mut safety_check = 0;

    // Prepared statement for speed
    let mut stmt = conn.prepare("SELECT parent_id, name FROM folders WHERE id = ?").unwrap();

    loop {
        let mut rows = stmt.query(params![current_id]).unwrap();
        
        if let Some(row) = rows.next().unwrap() {
            let parent_of_current: i64 = row.get(0).unwrap();
            let name: String = row.get(1).unwrap();
            
            parts.push(name);

            // Check for root (usually points to itself or 0)
            if parent_of_current == current_id || safety_check > 50 {
                break;
            }
            current_id = parent_of_current;
            safety_check += 1;
        } else {
            // ID not found (shouldn't happen if MFT is consistent)
            parts.push("???".to_string());
            break;
        }
    }

    parts.reverse();
    parts.join("\\")
}