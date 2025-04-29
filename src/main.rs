use actix::{Actor, ActorContext, Addr, AsyncContext, Handler, Message, StreamHandler};
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_web_actors::ws;
use chrono::{DateTime, FixedOffset, Utc};
use csv::WriterBuilder;
use rusqlite::{Connection, Result};
use serde::Serialize;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

use lazy_static::lazy_static;
use std::sync::Mutex;

// Packet counter for debugging
static PACKET_COUNT: AtomicUsize = AtomicUsize::new(0);

lazy_static! {
    static ref NEW_BARCODE: Mutex<String> = Mutex::new(String::from(""));
    static ref NEW_STATUS: Mutex<String> = Mutex::new(String::from(""));
    static ref IS_TEST_START: Mutex<bool> = Mutex::new(false);
    static ref PACKET_COUNTDOWN: Mutex<u8> = Mutex::new(0);
    static ref TEST_STAGE: Mutex<u8> = Mutex::new(0);
}

// Barcode keys
const BARCODE_KEY_SAVE: &str = "SAVE_DB"; // For CSV export
const BARCODE_KEY_CLEAR: &str = "DEL_DB"; // For clearing database

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
        info!(
            "New WebSocket client connected. Total clients: {}",
            self.clients.len()
        );
    }
}

impl Handler<Disconnect> for Broadcaster {
    type Result = ();

    fn handle(&mut self, msg: Disconnect, _: &mut Self::Context) {
        self.clients.retain(|client| client != &msg.0);
        info!(
            "WebSocket client disconnected. Total clients: {}",
            self.clients.len()
        );
    }
}

impl Handler<ClientMessage> for Broadcaster {
    type Result = ();

    fn handle(&mut self, msg: ClientMessage, _: &mut Self::Context) {
        for client in &self.clients {
            client.do_send(WsMessage(msg.0.clone())); // Clone to avoid move
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
    conn.execute(
        "CREATE TABLE IF NOT EXISTS packets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            barcode TEXT NOT NULL,
            status TEXT NOT NULL
        )",
        [],
    )?;
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

// Update packet in database
fn update_packet(conn: &Connection, barcode: &str, timestamp: &str, status: &str) -> Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT id FROM packets WHERE barcode = ?1 AND status = 'TEST' ORDER BY id DESC LIMIT 1",
    )?;
    let mut rows = stmt.query([barcode])?;
    if let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        conn.execute(
            "UPDATE packets SET timestamp = ?1, status = ?2 WHERE id = ?3",
            [timestamp, status, &id.to_string()],
        )?;
        info!(
            "Updated packet id {} for barcode {} to status {}",
            id, barcode, status
        );
        Ok(true)
    } else {
        Ok(false)
    }
}

// Clear all data from packets table
fn clear_database(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM packets", [])?;
    info!("Database cleared successfully");
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

async fn get_report(_req: HttpRequest) -> impl Responder {
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
        match status.as_str() {
            "OK" => ok_count = count,
            "HATA" => fail_count = count,
            _ => {}
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
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
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
        if barcode == BARCODE_KEY_SAVE || barcode == BARCODE_KEY_CLEAR {
            info!("Skipping barcode {} in CSV export", barcode);
            continue;
        }
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
    let istanbul_offset = FixedOffset::east_opt(3 * 3600).unwrap();
    let datetime: DateTime<FixedOffset> = Utc::now().with_timezone(&istanbul_offset);
    let filename = format!("data_{}.csv", datetime.format("%Y-%m-%d_%H-%M-%S"));

    HttpResponse::Ok()
        .content_type("text/csv")
        .append_header((
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        ))
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
    let mut temp_buffer = Vec::new();
    let conn = init_db().expect("Failed to initialize database");

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => {
                info!("Connection closed: {:?}", stream.peer_addr());
                break;
            }
            Ok(n) => {
                let data = &buffer[..n];

                let mut offset = 0;
                while let Some((packet, next_offset)) = extract_packet(&data[offset..]) {
                    let packet_id = PACKET_COUNT.fetch_add(1, Ordering::SeqCst);
                    let hex_temp: String = packet.iter().map(|b| format!("{:02X} ", b)).collect();
                    let hex = hex_temp.trim();

                    if packet.len() > 8 {
                        // Lock all mutexes safely
                        let mut new_barcode = NEW_BARCODE.lock().unwrap_or_else(|e| {
                            error!("Mutex poisoned for NEW_BARCODE: {}", e);
                            panic!("Mutex poisoned");
                        });
                        let mut new_status = NEW_STATUS.lock().unwrap_or_else(|e| {
                            error!("Mutex poisoned for NEW_STATUS: {}", e);
                            panic!("Mutex poisoned");
                        });
                        let mut is_test_start = IS_TEST_START.lock().unwrap_or_else(|e| {
                            error!("Mutex poisoned for IS_TEST_START: {}", e);
                            panic!("Mutex poisoned");
                        });
                        let mut packet_countdown = PACKET_COUNTDOWN.lock().unwrap_or_else(|e| {
                            error!("Mutex poisoned for PACKET_COUNTDOWN: {}", e);
                            panic!("Mutex poisoned");
                        });
                        let mut test_stage = TEST_STAGE.lock().unwrap_or_else(|e| {
                            error!("Mutex poisoned for TEST_STAGE: {}", e);
                            panic!("Mutex poisoned");
                        });

                        *new_status = String::from("");

                        // Handle special barcode keys
                        if *new_barcode == BARCODE_KEY_SAVE {
                            info!(
                                "Barcode {} matches CSV key, triggering CSV export",
                                *new_barcode
                            );
                            let json = r#"{"action": "export_csv"}"#.to_string();
                            if let Err(e) = sender.send(json) {
                                error!(
                                    "Error sending export_csv signal for barcode {}: {}",
                                    *new_barcode, e
                                );
                            } else {
                                info!(
                                    "Export CSV signal sent for barcode {}, skipping packet saving and table update",
                                    *new_barcode
                                );
                            }
                            *new_barcode = String::from("");
                            offset += next_offset;
                            continue;
                        } else if *new_barcode == BARCODE_KEY_CLEAR {
                            info!(
                                "Barcode {} matches clear key, clearing database",
                                *new_barcode
                            );
                            if let Err(e) = clear_database(&conn) {
                                error!(
                                    "Error clearing database for barcode {}: {}",
                                    *new_barcode, e
                                );
                            } else {
                                let json = r#"{"action": "clear_database"}"#.to_string();
                                if let Err(e) = sender.send(json) {
                                    error!(
                                        "Error sending clear_database signal for barcode {}: {}",
                                        *new_barcode, e
                                    );
                                } else {
                                    info!(
                                        "Clear database signal sent for barcode {}, skipping packet saving and table update",
                                        *new_barcode
                                    );
                                }
                            }
                            *new_barcode = String::from("");
                            offset += next_offset;
                            continue;
                        }

                        // Process packet for barcode and status
                        if packet[4] == 0x4 && packet[5] == 0xC {
                            let barcode: String = packet[6..]
                                .iter()
                                .filter(|&&b| b.is_ascii())
                                .map(|&b| b as char)
                                .collect();
                            *new_barcode = barcode;
                            *new_status = String::from("TEST");
                            *is_test_start = false;
                            *packet_countdown = 255;
                            *test_stage = 0;
                        } else if packet[4] == 0x1 && packet[5] == 0x5 {
                            if *packet_countdown > packet[packet.len() - 2] {
                                *packet_countdown = packet[packet.len() - 2];
                                *test_stage += 1;
                                if *test_stage > 3 {
                                    *is_test_start = true;
                                }
                            }
                        } else if packet[4] == 0x0 && packet[5] == 0x0 && *is_test_start {
                            if packet[12] == 0x2 && *packet_countdown == 0x0 {
                                *new_status = String::from("OK");
                                *is_test_start = false;
                                *test_stage = 0;
                                *packet_countdown = 255;
                            } else if packet[12] == 0x3 {
                                *new_status = String::from("HATA");
                                *is_test_start = false;
                                *test_stage = 0;
                                *packet_countdown = 255;
                            }
                        }

                        info!(
                            "Packet {} from ({:?}): Hex: {}, status: {}",
                            packet_id,
                            stream.peer_addr(),
                            hex,
                            *new_status
                        );

                        // Save or update packet if status is valid
                        if !new_status.is_empty() {
                            let istanbul_offset = FixedOffset::east_opt(3 * 3600).unwrap();
                            let datetime: DateTime<FixedOffset> =
                                Utc::now().with_timezone(&istanbul_offset);
                            let formatted_datetime =
                                datetime.format("%Y-%m-%d %H:%M:%S").to_string();

                            let updated = match update_packet(
                                &conn,
                                &*new_barcode,
                                &formatted_datetime,
                                &*new_status,
                            ) {
                                Ok(val) => val,
                                Err(e) => {
                                    error!("Failed to update packet {}: {}", packet_id, e);
                                    false
                                }
                            };

                            if updated {
                                let json = format!(
                                    "{{\"action\": \"update\", \"barcode\": \"{}\", \"timestamp\": \"{}\", \"status\": \"{}\"}}",
                                    *new_barcode, formatted_datetime, *new_status
                                );
                                if let Err(e) = sender.send(json) {
                                    error!(
                                        "Error sending update packet {} to channel: {}",
                                        packet_id, e
                                    );
                                } else {
                                    info!("Update packet {} sent to broadcaster", packet_id);
                                }
                            } else {
                                if let Err(e) = save_packet(
                                    &conn,
                                    &formatted_datetime,
                                    &*new_barcode,
                                    &*new_status,
                                ) {
                                    error!(
                                        "Failed to save packet {} to database: {}",
                                        packet_id, e
                                    );
                                } else {
                                    let json = format!(
                                        "{{\"timestamp\": \"{}\", \"barcode\": \"{}\", \"status\": \"{}\"}}",
                                        formatted_datetime, *new_barcode, *new_status
                                    );
                                    if let Err(e) = sender.send(json) {
                                        error!(
                                            "Error sending packet {} to channel: {}",
                                            packet_id, e
                                        );
                                    } else {
                                        info!("Packet {} sent to broadcaster", packet_id);
                                    }
                                }
                            }
                        }
                    }

                    offset += next_offset;
                }

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

        thread::sleep(Duration::from_millis(1));
    }
}

fn extract_packet(buffer: &[u8]) -> Option<(&[u8], usize)> {
    let start = match buffer.windows(2).position(|window| window == [0x5A, 0xA5]) {
        Some(pos) => pos,
        None => {
            if !buffer.is_empty() {
                let invalid_data_temp: String =
                    buffer.iter().map(|b| format!("{:02X} ", b)).collect();
                let invalid_data = invalid_data_temp.trim();
                warn!("Invalid data skipped (no 5A A5 start): {}", invalid_data);
            }
            return None;
        }
    };

    if buffer.len() < start + 3 {
        return None;
    }

    let packet_length = buffer[start + 3] as usize;

    if buffer.len() < start + packet_length {
        return None;
    }

    let packet = &buffer[start..start + packet_length];
    let next_offset = start + packet_length;
    Some((packet, next_offset))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let conn = match init_db() {
        Ok(conn) => conn,
        Err(e) => {
            error!("Failed to initialize database: {}", e);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Database initialization failed: {}", e),
            ));
        }
    };
    info!("Database initialized successfully");

    let broadcaster = Broadcaster { clients: vec![] }.start();
    let (sender, receiver) = sync_channel::<String>(1000);

    let sender_clone = sender.clone();
    thread::spawn(move || {
        tcp_server(sender_clone);
    });

    let broadcaster_clone = broadcaster.clone();
    thread::spawn(move || {
        while let Ok(message) = receiver.recv() {
            broadcaster_clone.do_send(ClientMessage(message));
        }
    });

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
