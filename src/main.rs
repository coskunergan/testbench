use actix_web::{web, App, HttpResponse, HttpServer, Responder, HttpRequest};
use actix_web_actors::ws;
use actix::{Actor, StreamHandler, ActorContext, Handler, Message, Addr, AsyncContext};
use std::net::{TcpListener, TcpStream};
use std::io::Read;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread;
use std::time::Duration;
use chrono::{DateTime, Utc, FixedOffset};
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{info, error, warn};
use rusqlite::{Connection, Result};
use serde::Serialize;
use csv::WriterBuilder;

// Packet counter for debugging
static PACKET_COUNT: AtomicUsize = AtomicUsize::new(0);

// Broadcaster actor to manage WebSocket clients and broadcast messages
struct Broadcaster {
    clients: Vec<Addr<WsSession>>,
}

impl Actor for Broadcaster {
    type Context = actix::Context<Self>;
}

#[derive(Message)]
#[rtype(result = "()")]
struct ClientMessage(String);

#[derive(Message)]
#[rtype(result = "()")]
struct Connect(Addr<WsSession>);

#[derive(Message)]
#[rtype(result = "()")]
struct Disconnect(Addr<WsSession>);

impl Handler<Connect> for Broadcaster {
    type Result = ();

    fn handle(&mut self, msg: Connect, _: &mut Self::Context) {
        self.clients.push(msg.0);
        info!("New WebSocket client connected. Total clients: {}", self.clients.len());
    }
}

impl Handler<Disconnect> for Broadcaster {
    type Result = ();

    fn handle(&mut self, msg: Disconnect, _: &mut Self::Context) {
        self.clients.retain(|client| client != &msg.0);
        info!("WebSocket client disconnected. Total clients: {}", self.clients.len());
    }
}

impl Handler<ClientMessage> for Broadcaster {
    type Result = ();

    fn handle(&mut self, msg: ClientMessage, _: &mut Self::Context) {
        for client in &self.clients {
            client.do_send(WsMessage(msg.0.clone()));
        }
    }
}

// WebSocket session
struct WsSession {
    broadcaster: Addr<Broadcaster>,
}

impl Actor for WsSession {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.broadcaster.do_send(Connect(ctx.address()));
    }

    fn stopped(&mut self, ctx: &mut Self::Context) {
        self.broadcaster.do_send(Disconnect(ctx.address()));
    }
}

#[derive(Message)]
#[rtype(result = "()")]
struct WsMessage(String);

impl Handler<WsMessage> for WsSession {
    type Result = ();

    fn handle(&mut self, msg: WsMessage, ctx: &mut Self::Context) {
        ctx.text(msg.0);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsSession {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Text(text)) => {
                ctx.text(text);
            }
            Ok(ws::Message::Close(reason)) => {
                ctx.close(reason);
                ctx.stop();
            }
            _ => {}
        }
    }
}

async fn ws_index(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    broadcaster: web::Data<Addr<Broadcaster>>,
) -> Result<HttpResponse, actix_web::Error> {
    let session = WsSession {
        broadcaster: broadcaster.get_ref().clone(),
    };
    ws::start(session, &req, stream)
}

async fn index() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html")
        .body(include_str!("../static/index.html"))
}

// Initialize SQLite database
fn init_db() -> Result<Connection> {
    let conn = Connection::open("testbench.db")?;
    // Create table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS packets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            barcode TEXT NOT NULL,
            status TEXT NOT NULL
        )",
        [],
    )?;
    // Set Write-Ahead Logging (WAL) mode
    let journal_mode: String = conn.query_row("PRAGMA journal_mode=WAL;", [], |row| row.get(0))?;
    if journal_mode != "wal" {
        error!("Failed to set WAL mode, got: {}", journal_mode);
    } else {
        info!("WAL mode enabled successfully");
    }
    Ok(conn)
}

// Save packet to database
fn save_packet(conn: &Connection, timestamp: &str, barcode: &str, status: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO packets (timestamp, barcode, status) VALUES (?1, ?2, ?3)",
        [timestamp, barcode, status],
    )?;
    Ok(())
}

// Get all packets for display
#[derive(Serialize)]
struct Packet {
    timestamp: String,
    barcode: String,
    status: String,
}

async fn get_packets() -> impl Responder {
    let conn = match Connection::open("testbench.db") {
        Ok(conn) => conn,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };
    let mut stmt = match conn.prepare("SELECT timestamp, barcode, status FROM packets") {
        Ok(stmt) => stmt,
        Err(e) => {
            error!("Failed to prepare statement: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };
    let rows = match stmt.query_map([], |row| {
        Ok(Packet {
            timestamp: row.get(0)?,
            barcode: row.get(1)?,
            status: row.get(2)?,
        })
    }) {
        Ok(rows) => rows,
        Err(e) => {
            error!("Failed to query packets: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };
    let packets: Vec<Packet> = rows.collect::<Result<Vec<_>, _>>().unwrap_or_default();
    HttpResponse::Ok().json(packets)
}

// Basic report (OK/HATA counts)
#[derive(Serialize)]
struct Report {
    ok_count: i64,
    fail_count: i64,
    total: i64,
}

async fn get_report(req: HttpRequest) -> impl Responder {
    let conn = match Connection::open("testbench.db") {
        Ok(conn) => conn,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };
    let mut stmt = match conn.prepare("SELECT status, COUNT(*) FROM packets GROUP BY status") {
        Ok(stmt) => stmt,
        Err(e) => {
            error!("Failed to prepare statement: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };
    let rows = match stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            error!("Failed to query report: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };
    let mut ok_count = 0;
    let mut fail_count = 0;
    for row in rows {
        let (status, count) = row.unwrap();
        if status == "OK" {
            ok_count = count;
        } else if status == "HATA" {
            fail_count = count;
        }
    }
    let report = Report {
        ok_count,
        fail_count,
        total: ok_count + fail_count,
    };
    HttpResponse::Ok().json(report)
}

// Export packets as CSV
async fn export_csv() -> impl Responder {
    let conn = match Connection::open("testbench.db") {
        Ok(conn) => conn,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };
    let mut stmt = match conn.prepare("SELECT timestamp, barcode, status FROM packets") {
        Ok(stmt) => stmt,
        Err(e) => {
            error!("Failed to prepare statement: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };
    let rows = match stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            error!("Failed to query packets: {}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }
    };

    let mut wtr = WriterBuilder::new().from_writer(vec![]);
    wtr.write_record(["Zaman", "Barkod", "Durum"]).unwrap();
    for row in rows {
        let (timestamp, barcode, status) = row.unwrap();
        wtr.write_record([timestamp, barcode, status]).unwrap();
    }
    let data = match wtr.into_inner() {
        Ok(data) => data,
        Err(e) => {
            error!("Failed to generate CSV: {}", e);
            return HttpResponse::InternalServerError().body("CSV error");
        }
    };

    // Generate timestamp for filename in Istanbul timezone (UTC+3)
    let istanbul_offset = FixedOffset::east_opt(3 * 3600).unwrap(); // UTC+3
    let datetime: DateTime<FixedOffset> = Utc::now().with_timezone(&istanbul_offset);
    let filename = format!("data_{}.csv", datetime.format("%Y-%m-%d_%H-%M-%S"));

    HttpResponse::Ok()
        .content_type("text/csv")
        .append_header(("Content-Disposition", format!("attachment; filename=\"{}\"", filename)))
        .body(data)
}

fn tcp_server(sender: SyncSender<String>) {
    let listener = match TcpListener::bind("0.0.0.0:5209") {
        Ok(listener) => listener,
        Err(e) => {
            error!("Error binding to port: {}", e);
            return;
        }
    };
    info!("TCP Server running on 0.0.0.0:5209...");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let sender = sender.clone();
                thread::spawn(move || {
                    handle_client(stream, sender);
                });
            }
            Err(e) => {
                error!("Error accepting connection: {}", e);
            }
        }
    }
}

fn handle_client(mut stream: TcpStream, sender: SyncSender<String>) {
    let mut buffer = [0; 1024];
    let mut temp_buffer = Vec::new(); // Only used for incomplete packets
    const PACKET_LENGTH: usize = 13; // Assuming 13 bytes for OK/FAIL check
    let conn = init_db().expect("Failed to initialize database");

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => {
                info!("Connection closed: {:?}", stream.peer_addr());
                break;
            }
            Ok(n) => {
                let data = &buffer[..n];

                // Process packets directly from data
                let mut offset = 0;
                while let Some((packet, next_offset)) = extract_packet(&data[offset..]) {
                    let packet_id = PACKET_COUNT.fetch_add(1, Ordering::SeqCst);
                    let hex = packet.iter().map(|b| format!("{:02X} ", b)).collect::<String>().trim().to_string();

                    // Check 13th byte for OK/HATA (index 12, 0-based)
                    let status = if packet.len() > 12 {
                        match packet[12] {
                            0x02 => "OK",
                            0x03 => "HATA",
                            _ => "Unknown",
                        }
                    } else {
                        "Invalid (too short)"
                    };

                    // Log all packets
                    info!("Packet {} from ({:?}): Hex: {}, status: {}", packet_id, stream.peer_addr(), hex, status);

                    // Only process OK or HATA packets
                    if status == "OK" || status == "HATA" {
                        // Get barcode from bytes 3 to 8 (index 2 to 7, 6 bytes)
                        let barcode: String = packet[2..8]
                            .iter()
                            .map(|b| format!("{:02X}", b))
                            .collect::<Vec<String>>()
                            .join("");

                        // Get current timestamp in Istanbul timezone (UTC+3)
                        let istanbul_offset = FixedOffset::east_opt(3 * 3600).unwrap(); // UTC+3
                        let datetime: DateTime<FixedOffset> = Utc::now().with_timezone(&istanbul_offset);
                        let formatted_datetime = datetime.format("%Y-%m-%d %H:%M:%S").to_string();

                        // Save to database
                        if let Err(e) = save_packet(&conn, &formatted_datetime, &barcode, status) {
                            error!("Failed to save packet {} to database: {}", packet_id, e);
                        }

                        // Send to WebSocket
                        let json = format!(
                            "{{\"timestamp\": \"{}\", \"barcode\": \"{}\", \"status\": \"{}\"}}",
                            formatted_datetime, barcode, status
                        );
                        if let Err(e) = sender.send(json) {
                            error!("Error sending packet {} to channel: {}", packet_id, e);
                        } else {
                            info!("Packet {} sent to broadcaster", packet_id);
                        }
                    }

                    offset += next_offset;
                }

                // Store remaining data (incomplete packet) in temp_buffer
                if offset < data.len() {
                    temp_buffer.extend_from_slice(&data[offset..]);
                } else {
                    temp_buffer.clear();
                }
            }
            Err(e) => {
                error!("Error reading data: {}", e);
                break;
            }
        }

        // Sleep briefly to prevent tight loop
        thread::sleep(Duration::from_millis(1));
    }
}

// Extract a single packet from the buffer slice (assuming 5A A5 start, 13 bytes long)
fn extract_packet(buffer: &[u8]) -> Option<(&[u8], usize)> {
    // Find start of packet (5A A5)
    let start = match buffer.windows(2).position(|window| window == [0x5A, 0xA5]) {
        Some(pos) => pos,
        None => {
            if !buffer.is_empty() {
                // Log invalid data
                let invalid_data = buffer.iter().map(|b| format!("{:02X} ", b)).collect::<String>().trim().to_string();
                warn!("Invalid data skipped (no 5A A5 start): {}", invalid_data);
            }
            return None;
        }
    };

    // Check if enough data for a full packet (13 bytes)
    const PACKET_LENGTH: usize = 13;
    if buffer.len() < start + PACKET_LENGTH {
        // Not enough data, keep for next read
        return None;
    }

    // Extract packet (13 bytes)
    let packet = &buffer[start..start + PACKET_LENGTH];
    // Return packet and offset to next data
    let next_offset = start + PACKET_LENGTH;
    Some((packet, next_offset))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // Initialize database
    let conn = match init_db() {
        Ok(conn) => conn,
        Err(e) => {
            error!("Failed to initialize database: {}", e);
            return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("Database initialization failed: {}", e)));
        }
    };
    info!("Database initialized successfully");

    // Start the broadcaster actor
    let broadcaster = Broadcaster { clients: vec![] }.start();

    // Create channel with buffer size
    let (sender, receiver) = sync_channel::<String>(1000);

    // Start TCP server in a separate thread
    let sender_clone = sender.clone();
    thread::spawn(move || {
        tcp_server(sender_clone);
    });

    // Process received messages and send to broadcaster
    let broadcaster_clone = broadcaster.clone();
    thread::spawn(move || {
        let mut packet_id = 0;
        while let Ok(message) = receiver.recv() {
            packet_id += 1;
            broadcaster_clone.do_send(ClientMessage(message));
        }
    });

    // Start HTTP server
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(broadcaster.clone()))
            .route("/", web::get().to(index))
            .route("/ws/", web::get().to(ws_index))
            .route("/packets", web::get().to(get_packets))
            .route("/report", web::get().to(get_report))
            .route("/export", web::get().to(export_csv))
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}