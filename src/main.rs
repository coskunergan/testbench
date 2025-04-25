use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use actix_web_actors::ws;
use actix::{Actor, StreamHandler, ActorContext, Handler, Message, Addr, AsyncContext};
use std::net::{TcpListener, TcpStream};
use std::io::Read;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH, Duration};
use rand::Rng;
use chrono::NaiveDateTime;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{info, error, warn};

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

                    // Check 13th byte for OK/FAIL (index 12, 0-based)
                    let status = if packet.len() > 12 {
                        match packet[12] {
                            0x02 => "OK",
                            0x03 => "FAIL",
                            _ => "Unknown",
                        }
                    } else {
                        "Invalid (too short)"
                    };

                    // Log all packets
                    info!("Packet {} from ({:?}): Hex: {}, status: {}", packet_id, stream.peer_addr(), hex, status);

                    // Only send OK or FAIL packets to WebSocket
                    if status == "OK" || status == "FAIL" {
                        // Generate random 10-digit barcode
                        let barcode: u64 = rand::thread_rng().gen_range(1_000_000_000..10_000_000_000);

                        // Get current timestamp
                        let timestamp = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        let datetime = NaiveDateTime::from_timestamp_opt(timestamp as i64, 0)
                            .unwrap()
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string();

                        // Send data to WebSocket clients as JSON (without hex)
                        let json = format!(
                            "{{\"timestamp\": \"{}\", \"barcode\": \"{}\", \"status\": \"{}\"}}",
                            datetime, barcode, status
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
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}